//! C API for `libsystema-shared-rs`.
//!
//! # Threading
//!
//! * Every `systema_shared_*` function may be called from any C thread.
//! * Blocking calls (admin clients, `query_unit_cgroup`, worker `wait`/`free`)
//!   drive a library-owned runtime thread and wait for the answer.  Making
//!   such a call from inside a library runtime thread or a C callback returns
//!   [`SystemaStatus::Reentrant`] instead of deadlocking.
//! * Fire-and-forget calls (`publish_*`, `subscribe_*`, `send_*`, `stop`) only
//!   enqueue work and are safe to call from a callback.
//!
//! # Ownership
//!
//! * Every `*_new` / `*_from_json` / `*_parse_json` / `*_decode` /
//!   `*_array_alloc` returns a pointer owned by the caller and released with
//!   the matching `*_free` (strings use `systema_shared_string_free`, byte
//!   buffers `systema_shared_bytes_free`).
//! * Output strings and byte buffers are copies; the library keeps no alias.
//! * Input pointers are borrowed for the duration of the call only.
//! * Protobuf results (`UnitRegistrationAck`, `StagingQueryResult`,
//!   `AdminStagingResult`, `UnitCgroupResult`) are returned as raw bytes for
//!   the caller to decode with the definitions in `proto/`.
//!
//! # Errors
//!
//! Every fallible function returns a [`SystemaStatus`]; details are available
//! per thread through `systema_shared_last_error_message` and
//! `systema_shared_last_error_code` until the next call on that thread.

use std::ffi::{c_char, CString};
use std::sync::OnceLock;

#[macro_use]
mod ffi;

pub use error::{
    systema_shared_clear_error, systema_shared_last_error_code, systema_shared_last_error_message,
    systema_shared_last_error_status,
};
pub use ffi::{
    systema_shared_bytes_free, systema_shared_string_free, systema_shared_string_list_free,
    systema_shared_string_list_get, systema_shared_string_list_len, SystemaStringList,
};

pub mod config_map;
pub mod controller;
mod error;
pub mod finder;
mod runtime;
pub mod staging_admin;
pub mod status;
pub mod unit_ir;
pub mod unit_status;
pub mod unitstate_admin;
pub mod worker;

/// ABI revision of the C surface.  Bump when an existing symbol or type
/// changes meaning; additive changes keep the value.
pub const SYSTEMA_SHARED_ABI_VERSION: u32 = 1;

static VERSION: OnceLock<CString> = OnceLock::new();

/// The library version, as a NUL-terminated string owned by the library.
#[no_mangle]
pub extern "C" fn systema_shared_version() -> *const c_char {
    VERSION
        .get_or_init(|| CString::new(env!("CARGO_PKG_VERSION")).unwrap_or_default())
        .as_ptr()
}

/// The ABI revision of the C surface.
#[no_mangle]
pub extern "C" fn systema_shared_abi_version() -> u32 {
    SYSTEMA_SHARED_ABI_VERSION
}
