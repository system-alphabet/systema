//! C API for `libsystema-shared-rs`.
//!
//! # Threading
//!
//! * Every `sysa_*` function may be called from any C thread.
//! * Blocking calls (admin clients, `query_unit_cgroup`, worker `wait`/`free`)
//!   drive a library-owned runtime thread and wait for the answer.  Making
//!   such a call from inside a library runtime thread or a C callback returns
//!   [`SysaStatus::Reentrant`] instead of deadlocking.
//! * Fire-and-forget calls (`publish_*`, `subscribe_*`, `send_*`, `stop`) only
//!   enqueue work and are safe to call from a callback.
//!
//! # Ownership
//!
//! * Every `*_new` / `*_from_json` / `*_parse_json` / `*_decode` /
//!   `*_array_alloc` returns a pointer owned by the caller and released with
//!   the matching `*_free` (strings use `sysa_string_free`, byte
//!   buffers `sysa_bytes_free`).
//! * Output strings and byte buffers are copies; the library keeps no alias.
//! * Input pointers are borrowed for the duration of the call only.
//! * Protobuf results (`UnitRegistrationAck`, `StagingQueryResult`,
//!   `AdminStagingResult`, `UnitCgroupResult`) are returned as raw bytes for
//!   the caller to decode with the definitions in `proto/`.
//!
//! # Errors
//!
//! Every fallible function returns a [`SysaStatus`]; details are available
//! per thread through `sysa_last_error_message` and
//! `sysa_last_error_code` until the next call on that thread.

use std::ffi::{c_char, CString};
use std::sync::OnceLock;

#[macro_use]
mod ffi;

pub use error::{
    sysa_clear_error, sysa_last_error_code, sysa_last_error_message, sysa_last_error_status,
};
pub use ffi::{
    sysa_bytes_free, sysa_string_free, sysa_string_list_free, sysa_string_list_get,
    sysa_string_list_len, SysaStringList,
};

pub mod config_map;
pub mod controller;
mod error;
pub mod finder;
pub mod manager;
mod runtime;
pub mod staging_admin;
pub mod status;
pub mod unit_ir;
pub mod unit_status;
pub mod unitstate_admin;
pub mod worker;

/// ABI revision of the C surface.  Bump when an existing symbol or type
/// changes meaning; additive changes keep the value.
pub const SYSA_SHARED_ABI_VERSION: u32 = 1;

static VERSION: OnceLock<CString> = OnceLock::new();

/// The library version, as a NUL-terminated string owned by the library.
#[no_mangle]
pub extern "C" fn sysa_version() -> *const c_char {
    VERSION
        .get_or_init(|| CString::new(env!("CARGO_PKG_VERSION")).unwrap_or_default())
        .as_ptr()
}

/// The ABI revision of the C surface.
#[no_mangle]
pub extern "C" fn sysa_abi_version() -> u32 {
    SYSA_SHARED_ABI_VERSION
}
