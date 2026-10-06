//! PAM integration for System S (dlopen-based).
//!
//! When a service sets both `PAMName=` and `User=` (like `user@.service`
//! with `PAMName=systemd-user`), the worker opens a PAM session before exec,
//! mirroring PID1's `setup_pam()` (systemd `src/core/exec-invoke.c`): the
//! PAM stack runs — e.g. `pam_systemd.so` asks logind for the user's runtime
//! directory, producing `XDG_RUNTIME_DIR`, `XDG_SESSION_*`, `HOME`, ... —
//! and the resulting environment is merged into the child's environment.
//!
//! libpam is loaded at runtime via `dlopen(3)` (exactly like systemd's
//! `dlopen_libpam()`), so PAM support is optional: if libpam is missing or
//! the PAM setup fails, the service still starts, just without the PAM
//! environment (a warning is logged).
//!
//! The PAM session is held open for as long as the service's main process
//! runs.  Once the process exits, the session is torn down (close session,
//! delete credentials, free the handle).  See [`PamSession::close`] for the
//! model used and its TODO.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::Arc;

use nix::libc;
use tracing::{debug, warn};

// PAM return codes (security/_pam_types.h)
const PAM_SUCCESS: c_int = 0;
const PAM_CONV_ERR: c_int = 19;

// PAM item types (security/_pam_types.h)
const PAM_TTY: c_int = 3;

// pam_setcred() flags (security/_pam_types.h)
const PAM_ESTABLISH_CRED: c_int = 0x0002;
const PAM_DELETE_CRED: c_int = 0x0004;

// ---------------------------------------------------------------------------
// PAM C types
// ---------------------------------------------------------------------------

#[repr(C)]
struct PamConv {
    conv: Option<
        unsafe extern "C" fn(
            num_msg: c_int,
            msg: *const *const PamMessage,
            resp: *mut *mut PamResponse,
            appdata_ptr: *mut c_void,
        ) -> c_int,
    >,
    appdata_ptr: *mut c_void,
}

#[repr(C)]
struct PamMessage {
    msg_style: c_int,
    msg: *const c_char,
}

#[repr(C)]
struct PamResponse {
    resp: *mut c_char,
    resp_retcode: c_int,
}

// ---------------------------------------------------------------------------
// libpam function pointers (dlopen'd)
// ---------------------------------------------------------------------------

type PamStartFn = unsafe extern "C" fn(
    service_name: *const c_char,
    user: *const c_char,
    conv: *const PamConv,
    pamh: *mut *mut c_void,
) -> c_int;
type PamEndFn = unsafe extern "C" fn(pamh: *mut c_void, status: c_int) -> c_int;
type PamSetItemFn =
    unsafe extern "C" fn(pamh: *mut c_void, item_type: c_int, item: *const c_void) -> c_int;
type PamSetCredFn = unsafe extern "C" fn(pamh: *mut c_void, flags: c_int) -> c_int;
type PamOpenSessionFn = unsafe extern "C" fn(pamh: *mut c_void, flags: c_int) -> c_int;
type PamCloseSessionFn = unsafe extern "C" fn(pamh: *mut c_void, flags: c_int) -> c_int;
type PamGetEnvListFn = unsafe extern "C" fn(pamh: *mut c_void) -> *mut *mut c_char;

/// Resolved libpam symbols.  Kept alive (Arc) for as long as any
/// [`PamSession`] references it, so the library cannot be unloaded while a
/// handle is in use.
struct PamLib {
    _handle: *mut c_void,
    start: PamStartFn,
    end: PamEndFn,
    set_item: PamSetItemFn,
    set_cred: PamSetCredFn,
    open_session: PamOpenSessionFn,
    close_session: PamCloseSessionFn,
    get_envlist: PamGetEnvListFn,
}

// The library is immutable after load; raw pointers are only ever used
// through the functions above, which are invoked serially per handle.
unsafe impl Send for PamLib {}
unsafe impl Sync for PamLib {}

/// Open the PAM library, trying the usual sonames.
fn dlopen_pam() -> Result<Arc<PamLib>, String> {
    let candidates = [c"libpam.so.2", c"libpam.so.0", c"libpam.so"];
    let mut last_err: Option<String> = None;

    for name in candidates {
        let handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
        if handle.is_null() {
            let e = unsafe { CStr::from_ptr(libc::dlerror()) }
                .to_string_lossy()
                .into_owned();
            last_err = Some(e);
            continue;
        }

        // dlsym() returns a data pointer; casting it to a function pointer
        // is not expressible in safe Rust, so we transmute (the standard
        // practice for dlopen-based bindings).
        unsafe fn load<T>(handle: *mut c_void, name: &CStr) -> Result<T, String> {
            let p = libc::dlsym(handle, name.as_ptr());
            if p.is_null() {
                return Err(format!(
                    "{}: {}",
                    name.to_string_lossy(),
                    CStr::from_ptr(libc::dlerror()).to_string_lossy()
                ));
            }
            Ok(std::mem::transmute_copy::<*mut c_void, T>(&p))
        }

        let r = (|| -> Result<Arc<PamLib>, String> {
            Ok(Arc::new(PamLib {
                _handle: handle,
                start: unsafe { load::<PamStartFn>(handle, c"pam_start")? },
                end: unsafe { load::<PamEndFn>(handle, c"pam_end")? },
                set_item: unsafe { load::<PamSetItemFn>(handle, c"pam_set_item")? },
                set_cred: unsafe { load::<PamSetCredFn>(handle, c"pam_setcred")? },
                open_session: unsafe { load::<PamOpenSessionFn>(handle, c"pam_open_session")? },
                close_session: unsafe { load::<PamCloseSessionFn>(handle, c"pam_close_session")? },
                get_envlist: unsafe { load::<PamGetEnvListFn>(handle, c"pam_getenvlist")? },
            }))
        })();

        match r {
            Ok(pam) => return Ok(pam),
            Err(e) => {
                unsafe {
                    libc::dlclose(handle);
                }
                last_err = Some(e);
            }
        }
    }

    Err(match last_err {
        Some(e) => sysa::l10n::fmt(
            sysa::l10n::t_("cannot load libpam: {e}"),
            &[("e", &e.to_string())],
        ),
        None => sysa::l10n::t_("cannot load libpam").to_string(),
    })
}

// ---------------------------------------------------------------------------
// Conversation function
// ---------------------------------------------------------------------------

/// Minimal PAM conversation: services are non-interactive, so any prompt is
/// refused with `PAM_CONV_ERR` (systemd's own `ask_password_conv()` would
/// prompt on the TTY; none of the modules in the service stacks we run —
/// including `pam_systemd.so` — ever prompt during session setup).
unsafe extern "C" fn pam_conv_stub(
    _num_msg: c_int,
    _msg: *const *const PamMessage,
    resp: *mut *mut PamResponse,
    _appdata_ptr: *mut c_void,
) -> c_int {
    if !resp.is_null() {
        *resp = ptr::null_mut();
    }
    PAM_CONV_ERR
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// A held-open PAM session; closing it tears the session down.
pub struct PamSession {
    pam: Arc<PamLib>,
    handle: *mut c_void,
}

// The handle is only ever used by one caller at a time (setup completes
// before the session is handed out; close runs later, strictly serialized
// by the caller), so passing the raw pointer between tasks is safe.
unsafe impl Send for PamSession {}

/// Open a PAM session for `pam_name`/`username`, mirroring systemd's
/// `setup_pam()`:
///
/// 1. `pam_start(pam_name, username, conv, &pamh)`
/// 2. `pam_set_item(PAM_TTY, tty)` (when a TTY is configured)
/// 3. `pam_setcred(PAM_ESTABLISH_CRED)`
/// 4. `pam_open_session(0)` — the PAM modules run here; `pam_systemd.so`
///    registers the session with logind and exports `XDG_RUNTIME_DIR` & co.
/// 5. `pam_getenvlist()` — capture the PAM environment for the child.
///
/// On success returns the PAM environment as `KEY=VALUE` pairs (ordered
/// like systemd's `pam_getenvlist()` result) plus the open session, which
/// the caller must close once the service's main process exits.
///
/// On failure returns an error message; the caller should log a warning and
/// continue without the PAM environment (matching systemd's tolerance for
/// a missing/unsable libpam).
pub fn pam_setup(
    pam_name: &str,
    username: &str,
    tty: Option<&str>,
) -> Result<(Vec<(String, String)>, PamSession), String> {
    let pam = dlopen_pam()?;

    let service = CString::new(pam_name).map_err(|_| {
        sysa::l10n::fmt(
            sysa::l10n::t_("PAM service name '{pam_name}' contains a NUL byte"),
            &[("pam_name", &pam_name.to_string())],
        )
    })?;
    let user = CString::new(username).map_err(|_| {
        sysa::l10n::fmt(
            sysa::l10n::t_("PAM user '{username}' contains a NUL byte"),
            &[("username", &username.to_string())],
        )
    })?;

    let conv = PamConv {
        conv: Some(pam_conv_stub),
        appdata_ptr: ptr::null_mut(),
    };
    let mut handle: *mut c_void = ptr::null_mut();

    let r = unsafe { (pam.start)(service.as_ptr(), user.as_ptr(), &conv, &mut handle) };
    if r != PAM_SUCCESS {
        return Err(sysa::l10n::fmt(
            sysa::l10n::t_("pam_start('{pam_name}') failed: {r}"),
            &[("pam_name", &pam_name.to_string()), ("r", &r.to_string())],
        ));
    }

    if let Some(tty) = tty {
        let tty_c = CString::new(tty).map_err(|_| {
            sysa::l10n::fmt(
                sysa::l10n::t_("PAM tty '{tty}' contains a NUL byte"),
                &[("tty", &tty.to_string())],
            )
        })?;
        let r = unsafe { (pam.set_item)(handle, PAM_TTY, tty_c.as_ptr().cast()) };
        if r != PAM_SUCCESS {
            debug!("pam_set_item(PAM_TTY, '{tty}') failed: {r}");
        }
    }

    let r = unsafe { (pam.set_cred)(handle, PAM_ESTABLISH_CRED) };
    if r != PAM_SUCCESS {
        // Matching systemd: credential establishment failure is logged but
        // session setup continues (some modules legitimately refuse).
        debug!("pam_setcred(PAM_ESTABLISH_CRED) failed: {r}");
    }

    let r = unsafe { (pam.open_session)(handle, 0) };
    if r != PAM_SUCCESS {
        unsafe {
            (pam.end)(handle, r);
        }
        return Err(sysa::l10n::fmt(
            sysa::l10n::t_("pam_open_session('{pam_name}') failed: {r}"),
            &[("pam_name", &pam_name.to_string()), ("r", &r.to_string())],
        ));
    }

    // Capture the PAM environment (malloc'd by libpam; free() each entry
    // and the array, like systemd's strv_free(pam_env)).
    let mut env: Vec<(String, String)> = Vec::new();
    let list = unsafe { (pam.get_envlist)(handle) };
    if !list.is_null() {
        let mut p = list;
        while !unsafe { *p }.is_null() {
            let entry = unsafe { CStr::from_ptr(*p) }.to_string_lossy().into_owned();
            if let Some((key, value)) = entry.split_once('=') {
                env.push((key.to_string(), value.to_string()));
            }
            unsafe {
                libc::free(*p as *mut c_void);
            }
            p = unsafe { p.add(1) };
        }
        unsafe {
            libc::free(list as *mut c_void);
        }
    }

    Ok((env, PamSession { pam, handle }))
}

impl PamSession {
    /// Tear down the PAM session: close the session, delete the
    /// credentials, and free the handle (systemd's sd-pam sequence).
    ///
    /// TODO: systemd runs this in a dedicated "(sd-pam)" child process
    /// (PDEATHSIG-guarded) so the PAM session is guaranteed to be closed
    /// even if the manager dies.  We instead run it from a worker tokio
    /// task that waits for the service's main process to exit, which is
    /// equivalent as long as the worker stays alive; if the worker itself
    /// dies, the logind session is still cleaned up by logind itself
    /// (sessions are abandoned and scopes killed).
    pub fn close(self) {
        let pam = self.pam;
        let handle = self.handle;
        unsafe {
            if (pam.close_session)(handle, 0) != PAM_SUCCESS {
                warn!("pam_close_session() failed");
            }
            if (pam.set_cred)(handle, PAM_DELETE_CRED) != PAM_SUCCESS {
                debug!("pam_setcred(PAM_DELETE_CRED) failed");
            }
            (pam.end)(handle, PAM_SUCCESS);
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    use super::*;

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn dlopen_finds_libpam() {
        assert!(
            dlopen_pam().is_ok(),
            "libpam must be dlopen-able (libpam.so.2/.so.0/.so)"
        );
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn pam_setup_with_missing_service_fails() {
        // No /etc/pam.d/<name> file exists → the session open must fail
        // with an error message (never panic), proving the failure path is
        // tolerated by callers.
        let r = pam_setup("systema-no-such-pam-service", "root", None);
        assert!(r.is_err(), "expected an error for an unknown PAM service");
    }
}
