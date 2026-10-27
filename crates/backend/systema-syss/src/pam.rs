//! PAM integration for System S (dlopen-based, forked holder child).
//!
//! When a service sets both `PAMName=` and `User=` (like `user@.service`
//! with `PAMName=systemd-user`), a dedicated child process is forked to run
//! the whole PAM stack and to hold the session open — systemd's "(sd-pam)"
//! helper: the PAM stack runs — e.g. `pam_systemd.so` asks logind for the
//! user's runtime directory, producing `XDG_RUNTIME_DIR`,
//! `DBUS_SESSION_BUS_ADDRESS`, `HOME`, ... — and the resulting environment
//! is returned to the syss parent (which execs the service with it), while
//! the child parks holding the PAM session for the service's lifetime and
//! tears it down in the same execution context (`pam_close_session()`,
//! `pam_setcred(PAM_DELETE_CRED)`, `pam_end()`) when the parent drops the
//! session.
//!
//! # Why a forked child, not a thread
//!
//! The kernel only lets the task that *is* the process write
//! `/proc/self/loginuid`: `fs/proc/base.c:proc_loginuid_write()` rejects
//! every writer whose `current` is not `pid_task(proc_pid(inode),
//! PIDTYPE_PID)` — i.e. the thread-group leader — with `-EPERM`, however
//! capable it is (identity check, not a capability check).  A forked child
//! is the leader of its own new process; a helper thread of the syss tokio
//! runtime (a `CLONE_THREAD` member) is not.  With `pam_loginuid.so`
//! required in the stacks we run, running the stack on a worker thread
//! always fails with PAM 14 (`PAM_SESSION_ERR`).
//!
//! By keeping the session in a parked process we additionally make the
//! teardown possible in the same task context that opened it, which no
//! exec'd-away process could ever do by itself (sshd runs its PAM child
//! through exec into the login shell and simply lets the session die with
//! the process; systemd starts *services*, not logins, so it keeps a
//! holder child instead).
//!
//! # Failure tolerance
//!
//! libpam is loaded at runtime via `dlopen(3)` (exactly like systemd's
//! `dlopen_libpam()`), so PAM support is optional: if libpam is missing or
//! the PAM session fails, the service still starts, just without the PAM
//! environment (a warning is logged by the caller).

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::os::unix::io::RawFd;
use std::ptr;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use nix::libc;
use tracing::warn;

// PAM return codes (security/_pam_types.h)
const PAM_SUCCESS: c_int = 0;
const PAM_CONV_ERR: c_int = 19;

// PAM item types (security/_pam_types.h)
const PAM_TTY: c_int = 3;

// pam_setcred() flags (security/_pam_types.h)
const PAM_ESTABLISH_CRED: c_int = 0x0002;
const PAM_DELETE_CRED: c_int = 0x0004;

// Result frames from the holder child, on the env pipe: one status byte,
// then NUL-terminated entries (env "K=V" strings on success, one ASCII
// detail message on failure).  EOF terminates the frame.
const FRAME_FAIL: u8 = 0;
const FRAME_OK: u8 = 1;

/// Holder-child exit statuses.
const EXIT_FAILURE: c_int = 1;
const EXIT_TEARDOWN_ERR: c_int = 2;

/// Cosmetic process name for `ps` (systemd's "(sd-pam)" equivalent).
#[cfg(any(target_os = "linux", target_os = "android"))]
const CHILD_NAME: &CStr = c"(syss-pam)";

/// Parent-side cap on waiting for the holder child's result frame.
/// `pam_open_session` may legitimately take seconds (module → logind D-Bus
/// round trips), but a hung module must not stall every later spawn.
const RESULT_DEADLINE: Duration = Duration::from_secs(30);

/// Bounded wait for the parked child's teardown before we lose patience.
const TEARDOWN_WAIT: Duration = Duration::from_secs(10);

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

/// Resolved libpam symbols.  Loaded once per process (cached) and never
/// unloaded, so the holder children forked afterwards inherit a stable
/// mapping (dlopen must never run in the forked child itself).
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

/// Process-cached libpam handle: loaded once, before the first fork of a
/// holder child — dlopen inside the forked child could deadlock on loader
/// locks held by concurrent tasks, so the child only ever uses the (forked
/// copy of the) mapping prepared here.
fn pam_lib() -> Result<Arc<PamLib>, String> {
    static CACHE: OnceLock<Result<Arc<PamLib>, String>> = OnceLock::new();
    CACHE.get_or_init(dlopen_pam).clone()
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
// Small syscall helpers (shared by the parent and the holder child)
// ---------------------------------------------------------------------------

/// Last errno as an error, for messages.
fn os_err() -> std::io::Error {
    std::io::Error::last_os_error()
}

/// `write()` loop over a byte slice; retries on EINTR and partial writes.
///
/// Also used inside the holder child (where `std::io::Write` is not an
/// option because tracing/log-less code must not allocate paths).
fn write_all_fd(fd: RawFd, mut buf: &[u8]) -> bool {
    while !buf.is_empty() {
        let n = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
        if n < 0 {
            if os_err().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return false;
        }
        buf = &buf[n as usize..];
    }
    true
}

/// Binary write helpers: `write_int` produces the decimal digits of a
/// `c_int` without allocations (the holder child writes with only libc).
fn write_int(mut v: c_int, out: &mut [u8; 12]) -> usize {
    if v == 0 {
        out[0] = b'0';
        return 1;
    }
    let neg = v < 0;
    v = if neg { -v } else { v };
    let mut digits: [u8; 11] = [0; 11];
    let mut i = 0;
    while v > 0 {
        digits[i] = b'0' + (v % 10) as u8;
        v /= 10;
        i += 1;
    }
    let n = i + neg as usize;
    let mut p = 0;
    if neg {
        out[p] = b'-';
        p += 1;
    }
    // digits were produced least-significant first.
    while i > 0 {
        i -= 1;
        out[p] = digits[i];
        p += 1;
    }
    n
}

fn pipe_cloexec() -> Result<(RawFd, RawFd), std::io::Error> {
    let mut fds: [c_int; 2] = [0, 0];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(os_err());
    }
    for fd in fds {
        // O_CLOEXEC matters doubly: the parent later forks *service*
        // processes with tokio::process::Command — without CLOEXEC a
        // service child would keep `end_write` open and the parked holder
        // would never observe the EOF that ends the service lifetime.
        for _ in 0..2 {
            let r = unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
            if r == 0 {
                break;
            }
            if os_err().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
        }
    }
    Ok((fds[0], fds[1]))
}

/// Fork-side fd hygiene (holder child only): keep the two pipes and the
/// standard streams, close everything else, so the parked child does not
/// hold the tokio runtime's epoll fds, worker-IPC sockets or log files for
/// the service's lifetime.  Capped at 65536 fds: daemons raising
/// `RLIMIT_NOFILE` far beyond that are rare, and the syscall cost of the
/// blind loop matters more than a leaking fd with no owner.
fn close_all_fds_except(keep: &[RawFd]) {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    let mut cap: libc::rlim_t = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } == 0 {
        lim.rlim_cur
    } else {
        65536
    };
    if cap as u64 > 65536 {
        cap = 65536;
    }

    let kept = |fd: RawFd| keep.contains(&fd);
    for fd in 0..cap as RawFd {
        if !kept(fd) {
            unsafe { libc::close(fd) };
        }
    }
}

/// Bounded collection of a holder child: waitpid(WNOHANG) polling until the
/// deadline, then SIGKILL + a final blocking reap.  `ECHILD` (the reaper
/// already collected the child) counts as done; true shutdown waits for it.
fn collect_holder_child(pid: libc::pid_t, wait: Duration) {
    let deadline = Instant::now() + wait;
    loop {
        let mut status: c_int = 0;
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if r == pid {
            if libc::WIFEXITED(status) {
                let code = libc::WEXITSTATUS(status);
                if code != 0 {
                    warn!("pam holder child exited with status {code}");
                }
            }
            return;
        }
        if r < 0 {
            let e = os_err().raw_os_error();
            if e == Some(libc::EINTR) {
                continue;
            }
            // ECHILD (reaped elsewhere) or other kernel refusal: done.
            return;
        }
        if Instant::now() >= deadline {
            unsafe { libc::kill(pid, libc::SIGKILL) };
            loop {
                let mut status: c_int = 0;
                let r = unsafe { libc::waitpid(pid, &mut status, 0) };
                if r == pid || (r < 0 && os_err().raw_os_error() != Some(libc::EINTR)) {
                    break;
                }
            }
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ---------------------------------------------------------------------------
// Holder child (the "(syss-pam)" analogue of systemd's "(sd-pam)")
// ---------------------------------------------------------------------------

/// Run the whole PAM stack in the forked holder child; park holding the
/// session; tear it down in this same context when the lifecycle pipe is
/// closed; end with `_exit(ExitState)`.  **Never returns**; everything here
/// is single-shot synchronous code (libc/libpam + pipes only) — no tracing,
/// no tokio, no Rust-level unwinding or drops.  This runs in a forked copy
/// of the (multi-threaded) syss process: only the calling thread survives,
/// and any allocation that exists must either be done before the fork (the
/// CStrings and the dlopen'd libpam) or be libc-side, which is fork-safe.
fn pam_child_main(
    pam: &PamLib,
    service: &CStr,
    user: &CStr,
    tty: Option<&CStr>,
    env_write: RawFd,
    end_read: RawFd,
) -> ! {
    // Die together with syss.  PDEATHSIG is Linux-only; on the other
    // targets the EOF and the parent-side bounded wait do the bookkeeping.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        unsafe {
            libc::prctl(
                libc::PR_SET_PDEATHSIG,
                libc::SIGKILL as libc::c_ulong,
                0,
                0,
                0,
            );
        }
        // Race: syss may have died between fork() and the prctl above.
        if unsafe { libc::getppid() } == 1 {
            unsafe { libc::_exit(EXIT_FAILURE) };
        }
        // `ps` cosmetics (systemd's "(sd-pam)").
        unsafe {
            libc::prctl(libc::PR_SET_NAME, CHILD_NAME.as_ptr(), 0, 0, 0);
        }
    }

    // Keep only the two pipes (+ stdio; PAM modules may reach /dev/log and
    // stray module chatter goes to stderr): everything else — notably the
    // runtime's epoll fds and worker sockets — must not be held open for
    // the whole service lifetime.
    close_all_fds_except(&[0, 1, 2, env_write, end_read]);

    // -- the stack ---------------------------------------------------------
    let conv = PamConv {
        conv: Some(pam_conv_stub),
        appdata_ptr: ptr::null_mut(),
    };
    let mut handle: *mut c_void = ptr::null_mut();

    let r = unsafe { (pam.start)(service.as_ptr(), user.as_ptr(), &conv, &mut handle) };
    if r != PAM_SUCCESS {
        write_fail_frame(env_write, "pam_start failed: ", r);
        unsafe { libc::_exit(EXIT_FAILURE) };
    }
    if let Some(tty) = tty {
        // Best effort (like before): a refused TTY item is not fatal.
        unsafe { (pam.set_item)(handle, PAM_TTY, tty.as_ptr().cast()) };
    }

    // Matching systemd: credential establishment failure is logged but
    // session setup continues (some modules legitimately refuse).
    unsafe { (pam.set_cred)(handle, PAM_ESTABLISH_CRED) };

    let r = unsafe { (pam.open_session)(handle, 0) };
    if r != PAM_SUCCESS {
        unsafe { (pam.end)(handle, r) };
        write_fail_frame(env_write, "pam_open_session failed: ", r);
        unsafe { libc::_exit(EXIT_FAILURE) };
    }

    // Success frame + the PAM environment (malloc'd by libpam; freed like
    // systemd's strv_free(pam_env)).
    if !write_ok_frame(env_write, pam, handle) {
        unsafe { libc::_exit(EXIT_FAILURE) };
    }

    // The frame ends at EOF, so the write side must go before parking:
    // keeping it open leaves the parent's read loop blocked until its 30 s
    // deadline, which turns every first `user@UID.service` start into a
    // half-minute stall and throws the environment away.
    unsafe { libc::close(env_write) };

    // -- park --------------------------------------------------------------
    // Hold the PAM session open until the parent closes the lifecycle pipe
    // (service's main process exited) or syss dies (PDEATHSIG).
    wait_for_eof(end_read);

    // -- teardown, in the context that opened the session -------------------
    // (systemd's sd-pam sequence;) exit codes tell the parent whether the
    // close went cleanly, since a parked child must not log via tracing.
    let closed = unsafe { (pam.close_session)(handle, 0) } == PAM_SUCCESS;
    unsafe {
        (pam.set_cred)(handle, PAM_DELETE_CRED);
        (pam.end)(handle, PAM_SUCCESS);
    }
    if closed {
        unsafe { libc::_exit(0) };
    }
    unsafe { libc::_exit(EXIT_TEARDOWN_ERR) };
}

/// Failure frame: status byte `FRAME_FAIL` + ASCII detail + newline.
fn write_fail_frame(fd: RawFd, detail: &str, code: c_int) {
    let head = [FRAME_FAIL];
    if !write_all_fd(fd, &head) {
        return; // parent is gone; nothing to say anymore
    }
    write_all_fd(fd, detail.as_bytes());
    let mut digits = [0u8; 12];
    let n = write_int(code, &mut digits);
    write_all_fd(fd, &digits[..n]);
    write_all_fd(fd, b"\n");
}

/// Success frame: status byte `FRAME_OK` + every `pam_getenvlist()` entry
/// (NUL-terminated, raw C bytes — no allocation in the child).
fn write_ok_frame(fd: RawFd, pam: &PamLib, handle: *mut c_void) -> bool {
    let head = [FRAME_OK];
    if !write_all_fd(fd, &head) {
        return false;
    }
    let list = unsafe { (pam.get_envlist)(handle) };
    if list.is_null() {
        return true; // empty environment is not an error
    }
    let mut p = list;
    loop {
        let entry = unsafe { *p };
        if entry.is_null() {
            break;
        }
        let len = unsafe { libc::strlen(entry) } as usize + 1; // include NUL
        if !write_all_fd(fd, unsafe { std::slice::from_raw_parts(entry.cast(), len) }) {
            return false; // aborted mid-frame: parent is gone
        }
        unsafe { libc::free(entry.cast()) };
        p = unsafe { p.add(1) };
    }
    unsafe { libc::free(list.cast()) };
    true
}

/// Park until the lifecycle pipe sees EOF (parent closed its write end).
fn wait_for_eof(fd: RawFd) {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let mut bit = [0u8; 1];
    loop {
        let n = unsafe { libc::poll(&mut pfd, 1, -1) };
        if n < 0 {
            if os_err().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            // poll() itself failed badly: park-ending is still the safe
            // interpretation (the parent must have moved on).
            return;
        }
        if pfd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            let r = unsafe { libc::read(fd, bit.as_mut_ptr().cast(), 1) };
            if r == 0 {
                return; // EOF: service lifetime is over
            }
            if r < 0 && os_err().raw_os_error() != Some(libc::EINTR) {
                return;
            }
            // A stray byte is not possible (the parent never writes);
            // loop back to polling if somehow delivered.
        }
    }
}

// ---------------------------------------------------------------------------
// Parent side
// ---------------------------------------------------------------------------

/// A held-open PAM session kept alive by a parked holder child.
///
/// The child is the *only* context with the PAM handle; from the parent's
/// point of view the session is "the child pid + one pipe".  Closing
/// `end_write` (drop or explicit [`close`](Self::close)) delivers EOF to
/// the parked child, which tears the session down in its own context.
pub struct PamSession {
    /// Pid of the parked holder child (the session's process-form context).
    pid: libc::pid_t,
    /// Write end of the lifecycle pipe; closing it ends the service's
    /// PAM session (EOF → child teardown).  O_CLOEXEC, see `pipe_cloexec`.
    end_write: RawFd,
}

impl Drop for PamSession {
    fn drop(&mut self) {
        // EOF alone is a complete teardown trigger: the parked child runs
        // the close sequence on its own and exits; whoever dropped the
        // session early relies on `reaper.rs` to collect the exit.
        let fd = std::mem::replace(&mut self.end_write, -1);
        if fd >= 0 {
            unsafe { libc::close(fd) };
        }
    }
}

impl PamSession {
    /// Tear down the PAM session: deliver EOF to the parked holder child,
    /// wait (bounded) for it to close the session/delete credentials/free
    /// the handle, and collect the child.
    ///
    /// The caller runs this from a blocking task once the service's main
    /// process has exited.
    pub fn close(mut self) {
        // Child-side teardown trigger.
        let fd = std::mem::replace(&mut self.end_write, -1);
        if fd >= 0 {
            unsafe { libc::close(fd) };
        }
        collect_holder_child(self.pid, TEARDOWN_WAIT);
    }
}

/// Run the PAM stack in a forked holder child and return its environment.
///
/// Mirrors systemd's `setup_pam()` (see the module docs for why this must
/// be a process, and how the session is held):
///
/// 1. parent dlopens libpam (cached; must happen *before* the fork)
/// 2. pipe pair for the env frame (child → parent), pipe pair for the
///    lifecycle signal (parent closes its write end → EOF → child teardown)
/// 3. `fork()`; the child runs the stack and parks
/// 4. the parent reads the frame (30 s deadline), returning the
///    `KEY=VALUE` pairs, ordered like systemd's `pam_getenvlist()` result.
///
/// On failure returns an error message; the caller should log a warning and
/// continue without the PAM environment (matching systemd's tolerance for a
/// missing/unusable libpam).
pub fn pam_session_open(
    pam_name: &str,
    username: &str,
    tty: Option<&str>,
) -> Result<(Vec<(String, String)>, PamSession), String> {
    // Everything the child needs is prepared *before* the fork: the
    // libpam mapping and all CStrings.  The child sees COW copies.
    let pam = pam_lib()?;

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
    let tty_c = match tty {
        Some(tty) => Some(CString::new(tty).map_err(|_| {
            sysa::l10n::fmt(
                sysa::l10n::t_("PAM tty '{tty}' contains a NUL byte"),
                &[("tty", &tty.to_string())],
            )
        })?),
        None => None,
    };

    let (env_read, env_write) = pipe_cloexec().map_err(|e| format!("pipe() failed: {e}"))?;
    let (end_read, end_write) = pipe_cloexec().map_err(|e| format!("pipe() failed: {e}"))?;

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        let e = os_err().to_string();
        unsafe { libc::close(env_read) };
        unsafe { libc::close(env_write) };
        unsafe { libc::close(end_read) };
        unsafe { libc::close(end_write) };
        return Err(format!("fork() failed: {e}"));
    }
    if pid == 0 {
        // Holder child (never returns; `_exit`s).
        pam_child_main(&pam, &service, &user, tty_c.as_deref(), env_write, end_read);
    }

    // Parent: drop the child-side pipe ends *first*, or the result frame
    // can never be terminated by EOF.
    unsafe { libc::close(env_write) };
    unsafe { libc::close(end_read) };

    let result = read_child_result(env_read);
    // The frame has been read (success or failure): the env pipe is done.
    unsafe { libc::close(env_read) };

    match result {
        Ok(env) => Ok((env, PamSession { pid, end_write })),
        Err(detail) => {
            // The child either reported a failure or died: make sure it is
            // gone and collected, then surface the failure to the caller
            // (which continues without the PAM environment).
            unsafe { libc::close(end_write) };
            collect_holder_child(pid, Duration::from_secs(2));
            Err(detail)
        }
    }
}

/// Read and decode the holder child's result frame: poll-drain until EOF
/// (child exits) or the deadline.  Success: `FRAME_OK` + `K=V` entries;
/// failure: `FRAME_FAIL` + ASCII detail.
fn read_child_result(fd: RawFd) -> Result<Vec<(String, String)>, String> {
    let deadline = Instant::now() + RESULT_DEADLINE;
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("pam holder child did not report a result in time; killed".to_string());
        }
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = (remaining.as_millis() as libc::c_int).min(30_000);
        let n = unsafe { libc::poll(&mut pfd, 1, ms) };
        if n < 0 {
            if os_err().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(format!("poll on pam holder pipe failed: {}", os_err()));
        }
        if n == 0 {
            continue; // poll timeout hit (part of the deadline)
        }
        if pfd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) == 0 {
            continue;
        }
        let r = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
        if r == 0 {
            break; // EOF = frame complete
        }
        if r < 0 {
            let e = os_err();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(format!("read on pam holder pipe failed: {e}"));
        }
        buf.extend_from_slice(&chunk[..r as usize]);
        if buf.len() > 1 << 20 {
            // PAM environments are a few dozen entries at most.
            return Err("pam holder produced an implausible environment".to_string());
        }
    }

    if buf.is_empty() {
        return Err("pam holder child died without reporting a result".to_string());
    }

    match buf[0] {
        FRAME_FAIL => {
            let detail = String::from_utf8_lossy(&buf[1..]).trim_end().to_string();
            Err(if detail.is_empty() {
                "pam holder child reported a failure without detail".to_string()
            } else {
                detail
            })
        }
        FRAME_OK => {
            let mut env = Vec::new();
            for entry in buf[1..].split(|b| *b == 0) {
                if entry.is_empty() {
                    continue;
                }
                let entry = String::from_utf8_lossy(entry);
                if let Some((key, value)) = entry.split_once('=') {
                    env.push((key.to_string(), value.to_string()));
                }
            }
            Ok(env)
        }
        _ => Err(format!(
            "pam holder child sent an unknown status byte {}",
            buf[0]
        )),
    }
}

#[cfg(test)]
mod tests {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    use super::*;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    use std::sync::atomic::{AtomicPtr, Ordering};
    #[cfg(any(target_os = "linux", target_os = "android"))]
    use std::time::Instant;

    /// Environment handed back by the stub `pam_getenvlist()` below.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    static TEST_ENVLIST: AtomicPtr<*mut c_char> = AtomicPtr::new(ptr::null_mut());

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn holder_child_hands_the_environment_over_before_parking() {
        // Exercise the real child path (frame → close → park) with a stub
        // PAM stack: the parent must read the environment immediately, not
        // sit on `read_child_result()` until its 30 s deadline because the
        // env pipe's write end is still open in the parked child.
        unsafe extern "C" fn start(
            _service: *const c_char,
            _user: *const c_char,
            _conv: *const PamConv,
            pamh: *mut *mut c_void,
        ) -> c_int {
            unsafe { *pamh = 1 as *mut c_void };
            PAM_SUCCESS
        }
        unsafe extern "C" fn end(_pamh: *mut c_void, _status: c_int) -> c_int {
            PAM_SUCCESS
        }
        unsafe extern "C" fn set_item(
            _pamh: *mut c_void,
            _item: c_int,
            _value: *const c_void,
        ) -> c_int {
            PAM_SUCCESS
        }
        unsafe extern "C" fn set_cred(_pamh: *mut c_void, _flags: c_int) -> c_int {
            PAM_SUCCESS
        }
        unsafe extern "C" fn open_session(_pamh: *mut c_void, _flags: c_int) -> c_int {
            PAM_SUCCESS
        }
        unsafe extern "C" fn close_session(_pamh: *mut c_void, _flags: c_int) -> c_int {
            PAM_SUCCESS
        }
        unsafe extern "C" fn get_envlist(_pamh: *mut c_void) -> *mut *mut c_char {
            TEST_ENVLIST.load(Ordering::SeqCst)
        }

        let pam = PamLib {
            _handle: ptr::null_mut(),
            start,
            end,
            set_item,
            set_cred,
            open_session,
            close_session,
            get_envlist,
        };

        // libpam hands the child a malloc'd strv; the child frees it.
        let entries = [
            c"XDG_RUNTIME_DIR=/run/user/1000",
            c"DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus",
        ];
        let list = unsafe {
            libc::calloc(entries.len() + 1, std::mem::size_of::<*mut c_char>()) as *mut *mut c_char
        };
        assert!(!list.is_null());
        for (i, entry) in entries.iter().enumerate() {
            unsafe { *list.add(i) = libc::strdup(entry.as_ptr()) };
        }
        TEST_ENVLIST.store(list, Ordering::SeqCst);

        let service = c"systema-test";
        let user = c"root";
        let (env_read, env_write) = pipe_cloexec().expect("env pipe");
        let (end_read, end_write) = pipe_cloexec().expect("lifecycle pipe");

        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            pam_child_main(&pam, service, user, None, env_write, end_read);
        }

        unsafe {
            libc::close(env_write);
            libc::close(end_read);
        }

        let started = Instant::now();
        let env = read_child_result(env_read).expect("holder child must report its environment");
        let elapsed = started.elapsed();
        unsafe { libc::close(env_read) };

        assert!(
            elapsed < Duration::from_secs(5),
            "environment handover took {elapsed:?}: the child kept the env pipe open"
        );
        assert_eq!(
            env,
            vec![
                ("XDG_RUNTIME_DIR".to_string(), "/run/user/1000".to_string()),
                (
                    "DBUS_SESSION_BUS_ADDRESS".to_string(),
                    "unix:path=/run/user/1000/bus".to_string()
                ),
            ]
        );

        // End the session and collect the parked child.
        unsafe { libc::close(end_write) };
        collect_holder_child(pid, Duration::from_secs(2));
    }

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
    fn pam_session_open_with_missing_service_fails() {
        // No /etc/pam.d/<name> file exists → the holder child must report
        // a failure (never panic, never hang), proving the failure path is
        // tolerated by callers.
        let r = pam_session_open("systema-no-such-pam-service", "root", None);
        assert!(r.is_err(), "expected an error for an unknown PAM service");
    }

    #[test]
    fn write_int_formats_decimal() {
        let mut b = [0u8; 12];
        // `&mut b` is passed straight into the index expression, whose
        // receiver borrows `b` first — take the length in its own step.
        for (value, expected) in [
            (0, &b"0"[..]),
            (1, &b"1"[..]),
            (14, &b"14"[..]),
            (29239070, &b"29239070"[..]),
            (-1, &b"-1"[..]),
        ] {
            let len = write_int(value, &mut b);
            assert_eq!(&b[..len], expected);
        }
    }
}
