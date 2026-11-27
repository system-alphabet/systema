//! C wrapper around the finder-side client (`finder.register_units`,
//! `finder.commit_units`, `staging.query`).

use std::ffi::c_char;

use prost::Message;
use systema_shared::finder::UnitFinder;

use crate::status::SysaStatus;

/// Opaque, library-owned finder client.
pub struct SysaFinder {
    socket_path: String,
}

/// Create a finder client for the given System A socket path.
#[no_mangle]
pub extern "C" fn sysa_finder_new(socket_path: *const c_char) -> *mut SysaFinder {
    ffi_try!(std::ptr::null_mut(), {
        let path = match unsafe { crate::ffi::require_str(socket_path, "socket_path") } {
            Ok(value) => value.to_string(),
            Err(_) => return std::ptr::null_mut(),
        };
        Box::into_raw(Box::new(SysaFinder { socket_path: path }))
    })
}

/// Register a batch of units; the answer is a protobuf `UnitRegistrationAck`.
#[no_mangle]
pub extern "C" fn sysa_finder_register_units(
    finder: *const SysaFinder,
    name: *const c_char,
    units_json: *const u8,
    units_json_len: usize,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> SysaStatus {
    ffi_try!(SysaStatus::Internal, {
        if finder.is_null() {
            return crate::error::fail(SysaStatus::InvalidArgument, "finder is null");
        }
        let name = match unsafe { crate::ffi::require_str(name, "name") } {
            Ok(value) => value.to_string(),
            Err(status) => return status,
        };
        let units =
            match unsafe { crate::ffi::require_bytes(units_json, units_json_len, "units_json") } {
                Ok(value) => value.to_vec(),
                Err(status) => return status,
            };
        let path = unsafe { &*finder }.socket_path.clone();
        let ack = match crate::runtime::block_on(
            async move {
                UnitFinder::with_socket(path)
                    .register_units(&name, units)
                    .await
            },
            crate::runtime::DEFAULT_WAIT,
        ) {
            Ok(ack) => ack,
            Err(status) => return status,
        };
        crate::ffi::check(crate::ffi::write_out_bytes(
            out,
            out_len,
            ack.encode_to_vec(),
        ))
    })
}

/// Commit a staging area; the answer is a protobuf `UnitRegistrationAck`.
#[no_mangle]
pub extern "C" fn sysa_finder_commit_units(
    finder: *const SysaFinder,
    name: *const c_char,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> SysaStatus {
    ffi_try!(SysaStatus::Internal, {
        if finder.is_null() {
            return crate::error::fail(SysaStatus::InvalidArgument, "finder is null");
        }
        let name = match unsafe { crate::ffi::require_str(name, "name") } {
            Ok(value) => value.to_string(),
            Err(status) => return status,
        };
        let path = unsafe { &*finder }.socket_path.clone();
        let ack = match crate::runtime::block_on(
            async move { UnitFinder::with_socket(path).commit_units(&name).await },
            crate::runtime::DEFAULT_WAIT,
        ) {
            Ok(ack) => ack,
            Err(status) => return status,
        };
        crate::ffi::check(crate::ffi::write_out_bytes(
            out,
            out_len,
            ack.encode_to_vec(),
        ))
    })
}

/// Query a staging area; the answer is a protobuf `StagingQueryResult`.
#[no_mangle]
pub extern "C" fn sysa_finder_query_staging(
    finder: *const SysaFinder,
    name: *const c_char,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> SysaStatus {
    ffi_try!(SysaStatus::Internal, {
        if finder.is_null() {
            return crate::error::fail(SysaStatus::InvalidArgument, "finder is null");
        }
        let name = match unsafe { crate::ffi::require_str(name, "name") } {
            Ok(value) => value.to_string(),
            Err(status) => return status,
        };
        let path = unsafe { &*finder }.socket_path.clone();
        let result = match crate::runtime::block_on(
            async move { UnitFinder::with_socket(path).query_staging(&name).await },
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

/// Release a finder client.
#[no_mangle]
pub extern "C" fn sysa_finder_free(finder: *mut SysaFinder) {
    ffi_try!((), {
        if !finder.is_null() {
            drop(unsafe { Box::from_raw(finder) });
        }
    })
}
