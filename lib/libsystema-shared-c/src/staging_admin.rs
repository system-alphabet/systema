//! C wrapper around the staging-admin client.

use std::ffi::{c_char, CString};

use prost::Message;
use systema_shared::staging_admin::StagingAdmin;

use crate::status::SysaStatus;

/// Opaque, library-owned staging-admin client.
pub struct SysaStagingAdmin {
    socket_path: String,
}

/// Opaque list of `(uid, name)` staging areas.
pub struct SysaStagingList {
    entries: Vec<(u32, CString)>,
}

/// Create a staging-admin client for the given control socket path.
#[no_mangle]
pub extern "C" fn sysa_staging_admin_new(socket_path: *const c_char) -> *mut SysaStagingAdmin {
    ffi_try!(std::ptr::null_mut(), {
        let path = match unsafe { crate::ffi::require_str(socket_path, "socket_path") } {
            Ok(value) => value.to_string(),
            Err(_) => return std::ptr::null_mut(),
        };
        Box::into_raw(Box::new(SysaStagingAdmin { socket_path: path }))
    })
}

/// List every staging area as `(uid, name)` pairs.
#[no_mangle]
pub extern "C" fn sysa_staging_admin_list(
    admin: *const SysaStagingAdmin,
    out: *mut *mut SysaStagingList,
) -> SysaStatus {
    ffi_try!(SysaStatus::Internal, {
        if admin.is_null() || out.is_null() {
            return crate::error::fail(SysaStatus::InvalidArgument, "argument is null");
        }
        let path = unsafe { &*admin }.socket_path.clone();
        let entries = match crate::runtime::block_on(
            async move { StagingAdmin::with_socket(path).list().await },
            crate::runtime::DEFAULT_WAIT,
        ) {
            Ok(entries) => entries,
            Err(status) => return status,
        };
        let entries = entries
            .into_iter()
            .map(|(uid, name)| (uid, crate::ffi::sanitize(&name)))
            .collect();
        unsafe { *out = Box::into_raw(Box::new(SysaStagingList { entries })) };
        SysaStatus::Ok
    })
}

/// Query every staging area owned by a UID (protobuf `AdminStagingResult`).
#[no_mangle]
pub extern "C" fn sysa_staging_admin_query_by_uid(
    admin: *const SysaStagingAdmin,
    uid: u32,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> SysaStatus {
    ffi_try!(SysaStatus::Internal, {
        if admin.is_null() {
            return crate::error::fail(SysaStatus::InvalidArgument, "admin is null");
        }
        let path = unsafe { &*admin }.socket_path.clone();
        let result = match crate::runtime::block_on(
            async move { StagingAdmin::with_socket(path).query_by_uid(uid).await },
            crate::runtime::DEFAULT_WAIT,
        ) {
            Ok(result) => result,
            Err(status) => return status,
        };
        crate::ffi::check(crate::ffi::write_out_bytes(
            out,
            out_len,
            result.encode_to_vec(),
        ))
    })
}

/// Run a staging-area query by name (protobuf `AdminStagingResult`).
#[no_mangle]
pub extern "C" fn sysa_staging_admin_query_by_name(
    admin: *const SysaStagingAdmin,
    name: *const c_char,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> SysaStatus {
    ffi_try!(SysaStatus::Internal, {
        if admin.is_null() {
            return crate::error::fail(SysaStatus::InvalidArgument, "admin is null");
        }
        let name = match unsafe { crate::ffi::require_str(name, "name") } {
            Ok(value) => value.to_string(),
            Err(status) => return status,
        };
        let path = unsafe { &*admin }.socket_path.clone();
        let result = match crate::runtime::block_on(
            async move { StagingAdmin::with_socket(path).query_by_name(&name).await },
            crate::runtime::DEFAULT_WAIT,
        ) {
            Ok(result) => result,
            Err(status) => return status,
        };
        crate::ffi::check(crate::ffi::write_out_bytes(
            out,
            out_len,
            result.encode_to_vec(),
        ))
    })
}

/// Run a staging-area query by `(uid, name)` (protobuf `AdminStagingResult`).
#[no_mangle]
pub extern "C" fn sysa_staging_admin_query(
    admin: *const SysaStagingAdmin,
    uid: u32,
    name: *const c_char,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> SysaStatus {
    ffi_try!(SysaStatus::Internal, {
        if admin.is_null() {
            return crate::error::fail(SysaStatus::InvalidArgument, "admin is null");
        }
        let name = match unsafe { crate::ffi::require_str(name, "name") } {
            Ok(value) => value.to_string(),
            Err(status) => return status,
        };
        let path = unsafe { &*admin }.socket_path.clone();
        let result = match crate::runtime::block_on(
            async move { StagingAdmin::with_socket(path).query(uid, &name).await },
            crate::runtime::DEFAULT_WAIT,
        ) {
            Ok(result) => result,
            Err(status) => return status,
        };
        crate::ffi::check(crate::ffi::write_out_bytes(
            out,
            out_len,
            result.encode_to_vec(),
        ))
    })
}

/// Query every staging area (protobuf `AdminStagingResult`).
#[no_mangle]
pub extern "C" fn sysa_staging_admin_query_all(
    admin: *const SysaStagingAdmin,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> SysaStatus {
    ffi_try!(SysaStatus::Internal, {
        if admin.is_null() {
            return crate::error::fail(SysaStatus::InvalidArgument, "admin is null");
        }
        let path = unsafe { &*admin }.socket_path.clone();
        let result = match crate::runtime::block_on(
            async move { StagingAdmin::with_socket(path).query_all().await },
            crate::runtime::DEFAULT_WAIT,
        ) {
            Ok(result) => result,
            Err(status) => return status,
        };
        crate::ffi::check(crate::ffi::write_out_bytes(
            out,
            out_len,
            result.encode_to_vec(),
        ))
    })
}

/// Commit a staging area (protobuf `AdminStagingResult`).
#[no_mangle]
pub extern "C" fn sysa_staging_admin_commit(
    admin: *const SysaStagingAdmin,
    uid: u32,
    name: *const c_char,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> SysaStatus {
    ffi_try!(SysaStatus::Internal, {
        if admin.is_null() {
            return crate::error::fail(SysaStatus::InvalidArgument, "admin is null");
        }
        let name = match unsafe { crate::ffi::require_str(name, "name") } {
            Ok(value) => value.to_string(),
            Err(status) => return status,
        };
        let path = unsafe { &*admin }.socket_path.clone();
        let result = match crate::runtime::block_on(
            async move { StagingAdmin::with_socket(path).commit(uid, &name).await },
            crate::runtime::DEFAULT_WAIT,
        ) {
            Ok(result) => result,
            Err(status) => return status,
        };
        crate::ffi::check(crate::ffi::write_out_bytes(
            out,
            out_len,
            result.encode_to_vec(),
        ))
    })
}

/// Number of staging areas in the list.
#[no_mangle]
pub extern "C" fn sysa_staging_list_len(list: *const SysaStagingList) -> usize {
    ffi_try!(0, {
        if list.is_null() {
            crate::error::fail(SysaStatus::InvalidArgument, "list is null");
            return 0;
        }
        unsafe { &*list }.entries.len()
    })
}

/// The UID of entry `index`; 0 when the index is out of range.
#[no_mangle]
pub extern "C" fn sysa_staging_list_uid(list: *const SysaStagingList, index: usize) -> u32 {
    ffi_try!(0, {
        if list.is_null() {
            crate::error::fail(SysaStatus::InvalidArgument, "list is null");
            return 0;
        }
        unsafe { &*list }
            .entries
            .get(index)
            .map(|(uid, _)| *uid)
            .unwrap_or(0)
    })
}

/// The name of entry `index`; null when the index is out of range.
#[no_mangle]
pub extern "C" fn sysa_staging_list_name(
    list: *const SysaStagingList,
    index: usize,
) -> *const c_char {
    ffi_try!(std::ptr::null(), {
        if list.is_null() {
            crate::error::fail(SysaStatus::InvalidArgument, "list is null");
            return std::ptr::null();
        }
        unsafe { &*list }
            .entries
            .get(index)
            .map(|(_, name)| name.as_ptr())
            .unwrap_or(std::ptr::null())
    })
}

/// Release a staging list.
#[no_mangle]
pub extern "C" fn sysa_staging_list_free(list: *mut SysaStagingList) {
    ffi_try!((), {
        if !list.is_null() {
            drop(unsafe { Box::from_raw(list) });
        }
    })
}

/// Release a staging-admin client.
#[no_mangle]
pub extern "C" fn sysa_staging_admin_free(admin: *mut SysaStagingAdmin) {
    ffi_try!((), {
        if !admin.is_null() {
            drop(unsafe { Box::from_raw(admin) });
        }
    })
}
