//! C-visible unit status: an opaque builder plus accessors.

use std::collections::HashMap;
use std::ffi::{c_char, CString};

use systema_shared::controller::UnitStatus;

use crate::ffi::{require_bytes, require_str, write_out_bytes};
use crate::status::SysaStatus;

/// Opaque, library-owned `UnitStatus`.
pub struct SysaUnitStatus {
    unit_name: CString,
    active_state: CString,
    sub_state: CString,
    main_pid: u32,
    invocation_id: CString,
    extensions: Vec<(CString, CString)>,
}

impl SysaUnitStatus {
    fn from_status(status: &UnitStatus) -> Self {
        let mut extensions: Vec<(CString, CString)> = status
            .extensions
            .iter()
            .map(|(key, value)| (sanitize(key), sanitize(value)))
            .collect();
        extensions.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
        SysaUnitStatus {
            unit_name: sanitize(&status.unit_name),
            active_state: sanitize(&status.active_state),
            sub_state: sanitize(&status.sub_state),
            main_pid: status.main_pid,
            invocation_id: sanitize(&status.invocation_id),
            extensions,
        }
    }

    fn to_status(&self) -> UnitStatus {
        let mut extensions = HashMap::new();
        for (key, value) in &self.extensions {
            extensions.insert(
                key.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            );
        }
        UnitStatus {
            unit_name: self.unit_name.to_string_lossy().into_owned(),
            active_state: self.active_state.to_string_lossy().into_owned(),
            sub_state: self.sub_state.to_string_lossy().into_owned(),
            main_pid: self.main_pid,
            invocation_id: self.invocation_id.to_string_lossy().into_owned(),
            extensions,
        }
    }
}

fn sanitize(text: &str) -> CString {
    CString::new(text.replace('\0', " ")).unwrap_or_default()
}

/// Take ownership of a status pointer created by this library.
///
/// # Safety
///
/// `ptr` must come from [`sysa_unit_status_new`] or
/// [`sysa_unit_status_decode`] and not be used afterwards.
pub(crate) unsafe fn take_unit_status(ptr: *mut SysaUnitStatus) -> Option<UnitStatus> {
    if ptr.is_null() {
        None
    } else {
        Some(Box::from_raw(ptr).to_status())
    }
}

/// Borrow a status pointer without consuming it.
///
/// # Safety
///
/// `ptr` must be null or point to a live `SysaUnitStatus`.
pub(crate) unsafe fn unit_status_from_ptr(ptr: *const SysaUnitStatus) -> Option<UnitStatus> {
    ptr.as_ref().map(SysaUnitStatus::to_status)
}

/// Allocate a status for handing back to the library (e.g. from `status`).
#[no_mangle]
pub extern "C" fn sysa_unit_status_new(
    unit_name: *const c_char,
    active_state: *const c_char,
    sub_state: *const c_char,
    main_pid: u32,
    invocation_id: *const c_char,
) -> *mut SysaUnitStatus {
    ffi_try!(std::ptr::null_mut(), {
        let status = (|| {
            Ok(SysaUnitStatus {
                unit_name: sanitize(unsafe { require_str(unit_name, "unit_name")? }),
                active_state: sanitize(unsafe { require_str(active_state, "active_state")? }),
                sub_state: sanitize(unsafe { require_str(sub_state, "sub_state")? }),
                main_pid,
                invocation_id: sanitize(unsafe { require_str(invocation_id, "invocation_id")? }),
                extensions: Vec::new(),
            })
        })();
        match status {
            Ok(status) => Box::into_raw(Box::new(status)),
            Err(status) => {
                crate::error::fail(status, "invalid unit status field");
                std::ptr::null_mut()
            }
        }
    })
}

/// Add or replace one extension entry.
#[no_mangle]
pub extern "C" fn sysa_unit_status_set_extension(
    status: *mut SysaUnitStatus,
    key: *const c_char,
    value: *const c_char,
) -> SysaStatus {
    ffi_try!(SysaStatus::Internal, {
        if status.is_null() {
            return crate::error::fail(SysaStatus::InvalidArgument, "status is null");
        }
        let key = match unsafe { require_str(key, "key") } {
            Ok(key) => key,
            Err(status) => return status,
        };
        let value = match unsafe { require_str(value, "value") } {
            Ok(value) => value,
            Err(status) => return status,
        };
        let status = unsafe { &mut *status };
        status
            .extensions
            .retain(|(existing, _)| existing.as_bytes() != key.as_bytes());
        status.extensions.push((sanitize(key), sanitize(value)));
        SysaStatus::Ok
    })
}

/// Borrow the unit name; valid until the status is freed or modified.
#[no_mangle]
pub extern "C" fn sysa_unit_status_unit_name(status: *const SysaUnitStatus) -> *const c_char {
    ffi_try!(std::ptr::null(), {
        if status.is_null() {
            crate::error::fail(SysaStatus::InvalidArgument, "status is null");
            return std::ptr::null();
        }
        unsafe { &*status }.unit_name.as_ptr()
    })
}

/// Borrow the active state; valid until the status is freed or modified.
#[no_mangle]
pub extern "C" fn sysa_unit_status_active_state(status: *const SysaUnitStatus) -> *const c_char {
    ffi_try!(std::ptr::null(), {
        if status.is_null() {
            crate::error::fail(SysaStatus::InvalidArgument, "status is null");
            return std::ptr::null();
        }
        unsafe { &*status }.active_state.as_ptr()
    })
}

/// Borrow the sub state; valid until the status is freed or modified.
#[no_mangle]
pub extern "C" fn sysa_unit_status_sub_state(status: *const SysaUnitStatus) -> *const c_char {
    ffi_try!(std::ptr::null(), {
        if status.is_null() {
            crate::error::fail(SysaStatus::InvalidArgument, "status is null");
            return std::ptr::null();
        }
        unsafe { &*status }.sub_state.as_ptr()
    })
}

/// Borrow the invocation ID; valid until the status is freed or modified.
#[no_mangle]
pub extern "C" fn sysa_unit_status_invocation_id(status: *const SysaUnitStatus) -> *const c_char {
    ffi_try!(std::ptr::null(), {
        if status.is_null() {
            crate::error::fail(SysaStatus::InvalidArgument, "status is null");
            return std::ptr::null();
        }
        unsafe { &*status }.invocation_id.as_ptr()
    })
}

/// Main PID carried by the status.
#[no_mangle]
pub extern "C" fn sysa_unit_status_main_pid(status: *const SysaUnitStatus) -> u32 {
    ffi_try!(0, {
        if status.is_null() {
            crate::error::fail(SysaStatus::InvalidArgument, "status is null");
            return 0;
        }
        unsafe { &*status }.main_pid
    })
}

/// Number of extension entries.
#[no_mangle]
pub extern "C" fn sysa_unit_status_extension_count(status: *const SysaUnitStatus) -> usize {
    ffi_try!(0, {
        if status.is_null() {
            crate::error::fail(SysaStatus::InvalidArgument, "status is null");
            return 0;
        }
        unsafe { &*status }.extensions.len()
    })
}

/// Borrow the key of extension `index`; null when out of range.
#[no_mangle]
pub extern "C" fn sysa_unit_status_extension_key(
    status: *const SysaUnitStatus,
    index: usize,
) -> *const c_char {
    ffi_try!(std::ptr::null(), {
        if status.is_null() {
            crate::error::fail(SysaStatus::InvalidArgument, "status is null");
            return std::ptr::null();
        }
        unsafe { &*status }
            .extensions
            .get(index)
            .map(|(key, _)| key.as_ptr())
            .unwrap_or(std::ptr::null())
    })
}

/// Borrow the value of extension `index`; null when out of range.
#[no_mangle]
pub extern "C" fn sysa_unit_status_extension_value(
    status: *const SysaUnitStatus,
    index: usize,
) -> *const c_char {
    ffi_try!(std::ptr::null(), {
        if status.is_null() {
            crate::error::fail(SysaStatus::InvalidArgument, "status is null");
            return std::ptr::null();
        }
        unsafe { &*status }
            .extensions
            .get(index)
            .map(|(_, value)| value.as_ptr())
            .unwrap_or(std::ptr::null())
    })
}

/// Serialize the status as a `UnitStatus` protobuf message.
#[no_mangle]
pub extern "C" fn sysa_unit_status_encode(
    status: *const SysaUnitStatus,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> SysaStatus {
    ffi_try!(SysaStatus::Internal, {
        if status.is_null() {
            return crate::error::fail(SysaStatus::InvalidArgument, "status is null");
        }
        let bytes = unsafe { &*status }.to_status().encode_to_vec();
        crate::ffi::check(write_out_bytes(out, out_len, bytes))
    })
}

/// Parse a `UnitStatus` protobuf message.
#[no_mangle]
pub extern "C" fn sysa_unit_status_decode(
    data: *const u8,
    len: usize,
    out: *mut *mut SysaUnitStatus,
) -> SysaStatus {
    ffi_try!(SysaStatus::Internal, {
        if out.is_null() {
            return crate::error::fail(SysaStatus::InvalidArgument, "out is null");
        }
        let bytes = match unsafe { require_bytes(data, len, "data") } {
            Ok(bytes) => bytes,
            Err(status) => return status,
        };
        match UnitStatus::decode_from(bytes) {
            Some(status) => {
                unsafe { *out = Box::into_raw(Box::new(SysaUnitStatus::from_status(&status))) };
                SysaStatus::Ok
            }
            None => {
                unsafe { *out = std::ptr::null_mut() };
                crate::error::fail(SysaStatus::Protocol, "the UnitStatus payload is invalid")
            }
        }
    })
}

/// Allocate an array of `count` null status pointers for a `sync_state`
/// callback.
///
/// The library always takes ownership of an array produced by this function:
/// it reads every non-null entry, frees them, and frees the array itself.
#[no_mangle]
pub extern "C" fn sysa_unit_status_array_alloc(
    count: usize,
    out: *mut *mut *mut SysaUnitStatus,
) -> SysaStatus {
    ffi_try!(SysaStatus::Internal, {
        if out.is_null() {
            return crate::error::fail(SysaStatus::InvalidArgument, "out is null");
        }
        let mut items = vec![std::ptr::null_mut(); count].into_boxed_slice();
        let pointer = items.as_mut_ptr();
        std::mem::forget(items);
        unsafe { *out = pointer };
        SysaStatus::Ok
    })
}

/// Release an array allocated by
/// [`sysa_unit_status_array_alloc`], without freeing its entries.
///
/// # Safety
///
/// `items` must come from [`sysa_unit_status_array_alloc`] with the
/// same `count`, and the entries must be freed separately (or be null).
pub(crate) unsafe fn free_status_array(items: *mut *mut SysaUnitStatus, count: usize) {
    if !items.is_null() {
        drop(Box::from_raw(std::slice::from_raw_parts_mut(items, count)));
    }
}

/// Release a status returned by this library.
#[no_mangle]
pub extern "C" fn sysa_unit_status_free(status: *mut SysaUnitStatus) {
    ffi_try!((), {
        if !status.is_null() {
            drop(unsafe { Box::from_raw(status) });
        }
    })
}
