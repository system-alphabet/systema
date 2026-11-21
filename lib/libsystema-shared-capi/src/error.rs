//! Thread-local last-error state and the mapping from Rust errors.

use std::cell::RefCell;
use std::ffi::{c_char, CString};

use systema_shared::error_code;

use crate::status::{map_code, SystemaErrorCode, SystemaStatus};

struct LastError {
    status: SystemaStatus,
    code: SystemaErrorCode,
    message: CString,
}

thread_local! {
    static LAST_ERROR: RefCell<Option<LastError>> = const { RefCell::new(None) };
}

fn sanitize(message: &str) -> CString {
    CString::new(message.replace('\0', " ")).unwrap_or_default()
}

/// Record the failure that the next `*_last_error_*` call reports.
pub(crate) fn set_error(status: SystemaStatus, code: SystemaErrorCode, message: &str) {
    LAST_ERROR.with(|slot| {
        *slot.borrow_mut() = Some(LastError {
            status,
            code,
            message: sanitize(message),
        });
    });
}

/// Whether the thread currently carries a recorded failure.
pub(crate) fn is_empty() -> bool {
    LAST_ERROR.with(|slot| slot.borrow().is_none())
}

/// Record a failure that is not tied to an IPC [`ErrorCode`].
pub(crate) fn fail(status: SystemaStatus, message: impl AsRef<str>) -> SystemaStatus {
    set_error(status, SystemaErrorCode::None, message.as_ref());
    status
}

/// Report a Rust error, keeping the IPC code when there is one.
pub(crate) fn report(error: anyhow::Error) -> SystemaStatus {
    let (status, code) = match error.downcast_ref::<error_code::SysError>() {
        Some(sys) => map_code(sys.code()),
        None => match error.downcast_ref::<std::io::Error>() {
            Some(_) => (SystemaStatus::Io, SystemaErrorCode::None),
            None => (SystemaStatus::Internal, SystemaErrorCode::None),
        },
    };
    set_error(status, code, &error.to_string());
    status
}

/// Pointer to the thread's last error message, or null when none is set.
///
/// The pointer stays valid until the next library call on the same thread
/// overwrites or clears the error.
#[no_mangle]
pub extern "C" fn systema_shared_last_error_message() -> *const c_char {
    ffi_try!(std::ptr::null(), {
        LAST_ERROR.with(|slot| match &*slot.borrow() {
            Some(error) => error.message.as_ptr(),
            None => std::ptr::null(),
        })
    })
}

/// Coarse status of the thread's last failure.
#[no_mangle]
pub extern "C" fn systema_shared_last_error_status() -> SystemaStatus {
    ffi_try!(SystemaStatus::Internal, {
        LAST_ERROR.with(|slot| {
            slot.borrow()
                .as_ref()
                .map(|error| error.status)
                .unwrap_or(SystemaStatus::Ok)
        })
    })
}

/// IPC code of the thread's last failure, or `None`.
#[no_mangle]
pub extern "C" fn systema_shared_last_error_code() -> SystemaErrorCode {
    ffi_try!(SystemaErrorCode::None, {
        LAST_ERROR.with(|slot| {
            slot.borrow()
                .as_ref()
                .map(|error| error.code)
                .unwrap_or(SystemaErrorCode::None)
        })
    })
}

/// Forget the thread's last failure.
#[no_mangle]
pub extern "C" fn systema_shared_clear_error() {
    ffi_try!((), {
        LAST_ERROR.with(|slot| *slot.borrow_mut() = None);
    })
}
