//! Shared helpers for the `extern "C"` surface: panic containment, argument
//! validation and ownership transfer.

use std::ffi::{c_char, CStr, CString};
use std::slice;

use crate::status::SystemaStatus;

/// Wrap an exported body so a Rust panic becomes `SYSTEMA_STATUS_INTERNAL`
/// instead of unwinding across the C ABI (which aborts since Rust 1.81).
macro_rules! ffi_try {
    ($default:expr, $body:block) => {{
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| $body)) {
            Ok(value) => value,
            Err(payload) => {
                let message = crate::ffi::panic_message(payload);
                crate::error::set_error(
                    crate::status::SystemaStatus::Internal,
                    crate::status::SystemaErrorCode::None,
                    &message,
                );
                $default
            }
        }
    }};
}

pub(crate) fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        format!("panic in the C API: {message}")
    } else if let Some(message) = payload.downcast_ref::<String>() {
        format!("panic in the C API: {message}")
    } else {
        "panic in the C API".to_string()
    }
}

/// Borrow a required NUL-terminated UTF-8 C string.
///
/// # Safety
///
/// `ptr` must be null or point to a valid NUL-terminated string.
pub(crate) unsafe fn require_str<'a>(
    ptr: *const c_char,
    what: &str,
) -> Result<&'a str, SystemaStatus> {
    if ptr.is_null() {
        return Err(crate::error::fail(
            SystemaStatus::InvalidArgument,
            format!("{what} is null"),
        ));
    }
    match CStr::from_ptr(ptr).to_str() {
        Ok(text) => Ok(text),
        Err(_) => Err(crate::error::fail(
            SystemaStatus::InvalidArgument,
            format!("{what} is not valid UTF-8"),
        )),
    }
}

/// Borrow a required byte buffer.  A null pointer is accepted only when the
/// length is zero.
///
/// # Safety
///
/// `ptr` must be null or point to `len` readable bytes.
pub(crate) unsafe fn require_bytes<'a>(
    ptr: *const u8,
    len: usize,
    what: &str,
) -> Result<&'a [u8], SystemaStatus> {
    if len == 0 {
        return Ok(&[]);
    }
    if ptr.is_null() {
        return Err(crate::error::fail(
            SystemaStatus::InvalidArgument,
            format!("{what} is null"),
        ));
    }
    Ok(slice::from_raw_parts(ptr, len))
}

/// Move a Rust string into a caller-owned `char *`.
pub(crate) fn into_c_string(text: &str) -> *mut c_char {
    sanitize(text).into_raw()
}

/// Move a byte buffer into a caller-owned pointer/length pair.
pub(crate) fn into_bytes(data: Vec<u8>) -> (*mut u8, usize) {
    if data.is_empty() {
        return (std::ptr::null_mut(), 0);
    }
    let boxed = data.into_boxed_slice();
    let len = boxed.len();
    (Box::into_raw(boxed) as *mut u8, len)
}

/// Validate and write the two output parameters of a byte-returning call.
pub(crate) fn write_out_bytes(
    out: *mut *mut u8,
    out_len: *mut usize,
    data: Vec<u8>,
) -> Result<(), SystemaStatus> {
    if out.is_null() || out_len.is_null() {
        return Err(crate::error::fail(
            SystemaStatus::InvalidArgument,
            "output parameter is null",
        ));
    }
    let (ptr, len) = into_bytes(data);
    unsafe {
        *out = ptr;
        *out_len = len;
    }
    Ok(())
}

/// An owned list of strings, used wherever the Rust API returns `Vec<String>`.
pub struct SystemaStringList {
    items: Vec<CString>,
}

impl SystemaStringList {
    fn from_strings(items: Vec<String>) -> Self {
        SystemaStringList {
            items: items.iter().map(|item| sanitize(item)).collect(),
        }
    }
}

pub(crate) fn into_string_list(items: Vec<String>) -> *mut SystemaStringList {
    Box::into_raw(Box::new(SystemaStringList::from_strings(items)))
}

pub(crate) fn sanitize(text: &str) -> CString {
    CString::new(text.replace('\0', " ")).unwrap_or_default()
}

/// Number of strings in `list`.
#[no_mangle]
pub extern "C" fn systema_shared_string_list_len(list: *const SystemaStringList) -> usize {
    ffi_try!(0, {
        if list.is_null() {
            crate::error::fail(SystemaStatus::InvalidArgument, "list is null");
            return 0;
        }
        unsafe { &*list }.items.len()
    })
}

/// Borrow string `index`; null when the index is out of range.
#[no_mangle]
pub extern "C" fn systema_shared_string_list_get(
    list: *const SystemaStringList,
    index: usize,
) -> *const c_char {
    ffi_try!(std::ptr::null(), {
        if list.is_null() {
            crate::error::fail(SystemaStatus::InvalidArgument, "list is null");
            return std::ptr::null();
        }
        unsafe { &*list }
            .items
            .get(index)
            .map(|item| item.as_ptr())
            .unwrap_or(std::ptr::null())
    })
}

/// Release a list returned by this library.
#[no_mangle]
pub extern "C" fn systema_shared_string_list_free(list: *mut SystemaStringList) {
    ffi_try!((), {
        if !list.is_null() {
            drop(unsafe { Box::from_raw(list) });
        }
    })
}

/// Release a string returned by this library.
#[no_mangle]
pub extern "C" fn systema_shared_string_free(text: *mut c_char) {
    ffi_try!((), {
        if !text.is_null() {
            drop(unsafe { CString::from_raw(text) });
        }
    })
}

/// Release a byte buffer returned by this library.
#[no_mangle]
pub extern "C" fn systema_shared_bytes_free(data: *mut u8, len: usize) {
    ffi_try!((), {
        if !data.is_null() {
            drop(unsafe { Box::from_raw(slice::from_raw_parts_mut(data, len)) });
        }
    })
}

/// Map a fallible FFI helper onto the status convention, reporting the error.
pub(crate) fn check(result: Result<(), SystemaStatus>) -> SystemaStatus {
    match result {
        Ok(()) => SystemaStatus::Ok,
        Err(status) => {
            if crate::error::is_empty() {
                crate::error::fail(status, format!("{status:?}"));
            }
            status
        }
    }
}
