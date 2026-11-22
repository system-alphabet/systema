//! C wrapper around the streaming unit-state admin client.

use std::ffi::{c_char, c_void};

use systema_shared::unitstate_admin::UnitStateAdmin;

use crate::status::SysaStatus;

/// `callback(userdata, name, json, json_len)` — return `SYSA_STATUS_OK` to
/// keep streaming; any other status aborts the iteration.
pub type SysaUnitStateCallback = Option<
    unsafe extern "C" fn(
        userdata: *mut c_void,
        name: *const c_char,
        json: *const u8,
        json_len: usize,
    ) -> SysaStatus,
>;

/// Opaque, library-owned unit-state admin client.
pub struct SysaUnitStateAdmin {
    socket_path: String,
}

/// Create a unit-state admin client for the given control socket path.
#[no_mangle]
pub extern "C" fn sysa_unitstate_admin_new(socket_path: *const c_char) -> *mut SysaUnitStateAdmin {
    ffi_try!(std::ptr::null_mut(), {
        let path = match unsafe { crate::ffi::require_str(socket_path, "socket_path") } {
            Ok(value) => value.to_string(),
            Err(_) => return std::ptr::null_mut(),
        };
        Box::into_raw(Box::new(SysaUnitStateAdmin { socket_path: path }))
    })
}

/// Stream every cached unit to `callback`; `out_total` receives the streamed
/// entry count.
///
/// The snapshot is collected inside the library runtime first; the callback
/// then runs on the calling thread.
#[no_mangle]
pub extern "C" fn sysa_unitstate_admin_list(
    admin: *const SysaUnitStateAdmin,
    callback: SysaUnitStateCallback,
    userdata: *mut c_void,
    out_total: *mut u32,
) -> SysaStatus {
    ffi_try!(SysaStatus::Internal, {
        if admin.is_null() {
            return crate::error::fail(SysaStatus::InvalidArgument, "admin is null");
        }
        let Some(callback) = callback else {
            return crate::error::fail(SysaStatus::InvalidArgument, "callback is null");
        };
        let path = unsafe { &*admin }.socket_path.clone();
        let (result, entries) = match crate::runtime::block_on(
            async move {
                let admin = UnitStateAdmin::with_socket(path);
                let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
                let result = admin
                    .list(|name, json| {
                        entries.push((name.to_string(), json.to_vec()));
                        Ok(())
                    })
                    .await?;
                Ok((result, entries))
            },
            crate::runtime::DEFAULT_WAIT,
        ) {
            Ok(value) => value,
            Err(status) => return status,
        };
        if !out_total.is_null() {
            unsafe { *out_total = result.total };
        }
        for (name, json) in entries {
            let name = crate::ffi::sanitize(&name);
            let status = unsafe { callback(userdata, name.as_ptr(), json.as_ptr(), json.len()) };
            if status != SysaStatus::Ok {
                return status;
            }
        }
        SysaStatus::Ok
    })
}

/// Release a unit-state admin client.
#[no_mangle]
pub extern "C" fn sysa_unitstate_admin_free(admin: *mut SysaUnitStateAdmin) {
    ffi_try!((), {
        if !admin.is_null() {
            drop(unsafe { Box::from_raw(admin) });
        }
    })
}
