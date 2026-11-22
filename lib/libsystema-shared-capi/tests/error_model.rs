//! Argument validation, the thread-local error slot and status mapping.

use std::ffi::{c_char, CStr, CString};
use std::ptr;

use systema_shared_capi::config_map::sysa_config_map_parse_json;
use systema_shared_capi::finder::{sysa_finder_free, sysa_finder_new, sysa_finder_register_units};
use systema_shared_capi::status::{SysaErrorCode, SysaStatus};
use systema_shared_capi::{
    sysa_bytes_free, sysa_clear_error, sysa_last_error_code, sysa_last_error_message,
    sysa_last_error_status, sysa_string_free,
};

fn last_message() -> Option<String> {
    let pointer = sysa_last_error_message();
    if pointer.is_null() {
        None
    } else {
        Some(
            unsafe { CStr::from_ptr(pointer) }
                .to_string_lossy()
                .into_owned(),
        )
    }
}

#[test]
fn null_arguments_report_invalid_argument() {
    let status = sysa_config_map_parse_json(ptr::null(), 0, ptr::null_mut());
    assert_eq!(status, SysaStatus::InvalidArgument);
    assert_eq!(sysa_last_error_status(), SysaStatus::InvalidArgument);
    assert!(last_message().is_some());
}

#[test]
fn invalid_utf8_is_rejected() {
    let bad = b"\x80\0";
    let finder = sysa_finder_new(bad.as_ptr() as *const c_char);
    assert!(finder.is_null());
    assert_eq!(sysa_last_error_status(), SysaStatus::InvalidArgument);
    let message = last_message().expect("an error message is recorded");
    assert!(message.contains("UTF-8"), "{message}");
}

#[test]
fn the_last_error_is_overwritten_by_the_next_failure() {
    let bad = b"\x80\0";
    sysa_finder_new(bad.as_ptr() as *const c_char);
    assert!(last_message().unwrap().contains("UTF-8"));

    sysa_finder_new(ptr::null());
    assert!(last_message().unwrap().contains("null"));

    sysa_clear_error();
    assert!(last_message().is_none());
    assert_eq!(sysa_last_error_status(), SysaStatus::Ok);
    assert_eq!(sysa_last_error_code(), SysaErrorCode::None);
}

#[test]
fn connect_failures_carry_the_ipc_error_code() {
    let path = CString::new("/nonexistent/systema-capi-test/allocator.sock").unwrap();
    let finder = sysa_finder_new(path.as_ptr());
    assert!(!finder.is_null());
    let name = CString::new("area").unwrap();
    let mut out = ptr::null_mut();
    let mut out_len = 0usize;
    let status = sysa_finder_register_units(
        finder,
        name.as_ptr(),
        ptr::null(),
        0,
        &mut out,
        &mut out_len,
    );
    assert_eq!(status, SysaStatus::Io);
    assert_eq!(sysa_last_error_code(), SysaErrorCode::ConnectFailed);
    assert_eq!(sysa_last_error_status(), status);
    sysa_finder_free(finder);
}

#[test]
fn exported_strings_and_buffers_can_be_freed() {
    let text = CString::new("owned by the caller").unwrap();
    sysa_string_free(text.into_raw());

    let bytes = vec![1u8, 2, 3, 4].into_boxed_slice();
    let len = bytes.len();
    sysa_bytes_free(Box::into_raw(bytes) as *mut u8, len);
}

#[test]
fn version_and_abi_are_exposed() {
    assert_eq!(systema_shared_capi::sysa_abi_version(), 1);
    let pointer = systema_shared_capi::sysa_version();
    assert!(!pointer.is_null());
    assert!(!unsafe { CStr::from_ptr(pointer) }.to_bytes().is_empty());
}
