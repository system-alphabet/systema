//! C-visible `ConfigMap` (the flattened type-specific directive map).

use std::ffi::c_char;

use systema_shared::ir::{config, ConfigMap};

use crate::ffi::{require_bytes, require_str, write_out_bytes};
use crate::status::SysaStatus;

/// Opaque, library-owned `ConfigMap`.
pub struct SysaConfigMap {
    map: ConfigMap,
}

impl SysaConfigMap {
    pub(crate) fn new(map: ConfigMap) -> Self {
        SysaConfigMap { map }
    }
}

/// Parse a JSON object into a new map.
#[no_mangle]
pub extern "C" fn sysa_config_map_parse_json(
    data: *const u8,
    len: usize,
    out: *mut *mut SysaConfigMap,
) -> SysaStatus {
    ffi_try!(SysaStatus::Internal, {
        if out.is_null() {
            return crate::error::fail(SysaStatus::InvalidArgument, "out is null");
        }
        let bytes = match unsafe { require_bytes(data, len, "data") } {
            Ok(bytes) => bytes,
            Err(status) => return status,
        };
        match config::parse_json(bytes) {
            Ok(map) => {
                unsafe { *out = Box::into_raw(Box::new(SysaConfigMap::new(map))) };
                SysaStatus::Ok
            }
            Err(error) => {
                unsafe { *out = std::ptr::null_mut() };
                crate::error::report(error)
            }
        }
    })
}

/// Serialize the map back to JSON.
#[no_mangle]
pub extern "C" fn sysa_config_map_to_json(
    map: *const SysaConfigMap,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> SysaStatus {
    ffi_try!(SysaStatus::Internal, {
        if map.is_null() {
            return crate::error::fail(SysaStatus::InvalidArgument, "map is null");
        }
        let bytes = config::to_json_bytes(&unsafe { &*map }.map);
        crate::ffi::check(write_out_bytes(out, out_len, bytes))
    })
}

/// Read a string directive; `*out` is null and `NOT_FOUND` is returned when
/// the key is absent.
#[no_mangle]
pub extern "C" fn sysa_config_map_get_string(
    map: *const SysaConfigMap,
    key: *const c_char,
    out: *mut *mut c_char,
) -> SysaStatus {
    ffi_try!(SysaStatus::Internal, {
        if map.is_null() || out.is_null() {
            return crate::error::fail(SysaStatus::InvalidArgument, "argument is null");
        }
        let key = match unsafe { require_str(key, "key") } {
            Ok(key) => key,
            Err(status) => return status,
        };
        match config::get_string(&unsafe { &*map }.map, key) {
            Some(value) => {
                unsafe { *out = crate::ffi::into_c_string(&value) };
                SysaStatus::Ok
            }
            None => {
                unsafe { *out = std::ptr::null_mut() };
                crate::error::fail(SysaStatus::NotFound, format!("{key} is not set"))
            }
        }
    })
}

/// Read a repeated directive into a new string list (empty when absent).
#[no_mangle]
pub extern "C" fn sysa_config_map_get_strings(
    map: *const SysaConfigMap,
    key: *const c_char,
    out: *mut *mut crate::ffi::SysaStringList,
) -> SysaStatus {
    ffi_try!(SysaStatus::Internal, {
        if map.is_null() || out.is_null() {
            return crate::error::fail(SysaStatus::InvalidArgument, "argument is null");
        }
        let key = match unsafe { require_str(key, "key") } {
            Ok(key) => key,
            Err(status) => return status,
        };
        let values = config::get_strings(&unsafe { &*map }.map, key);
        unsafe { *out = crate::ffi::into_string_list(values) };
        SysaStatus::Ok
    })
}

/// Read a boolean directive (absent keys read as false, like the Rust API).
#[no_mangle]
pub extern "C" fn sysa_config_map_get_bool(
    map: *const SysaConfigMap,
    key: *const c_char,
    out: *mut u8,
) -> SysaStatus {
    ffi_try!(SysaStatus::Internal, {
        if map.is_null() || out.is_null() {
            return crate::error::fail(SysaStatus::InvalidArgument, "argument is null");
        }
        let key = match unsafe { require_str(key, "key") } {
            Ok(key) => key,
            Err(status) => return status,
        };
        let value = config::get_bool(&unsafe { &*map }.map, key);
        unsafe { *out = u8::from(value) };
        SysaStatus::Ok
    })
}

/// Read an unsigned directive; `NOT_FOUND` when absent or not an integer.
#[no_mangle]
pub extern "C" fn sysa_config_map_get_u32(
    map: *const SysaConfigMap,
    key: *const c_char,
    out: *mut u32,
) -> SysaStatus {
    ffi_try!(SysaStatus::Internal, {
        if map.is_null() || out.is_null() {
            return crate::error::fail(SysaStatus::InvalidArgument, "argument is null");
        }
        let key = match unsafe { require_str(key, "key") } {
            Ok(key) => key,
            Err(status) => return status,
        };
        match config::get_u32(&unsafe { &*map }.map, key) {
            Some(value) => {
                unsafe { *out = value };
                SysaStatus::Ok
            }
            None => crate::error::fail(SysaStatus::NotFound, format!("{key} is not set")),
        }
    })
}

/// Read a signed directive; `NOT_FOUND` when absent or not an integer.
#[no_mangle]
pub extern "C" fn sysa_config_map_get_i32(
    map: *const SysaConfigMap,
    key: *const c_char,
    out: *mut i32,
) -> SysaStatus {
    ffi_try!(SysaStatus::Internal, {
        if map.is_null() || out.is_null() {
            return crate::error::fail(SysaStatus::InvalidArgument, "argument is null");
        }
        let key = match unsafe { require_str(key, "key") } {
            Ok(key) => key,
            Err(status) => return status,
        };
        let value = unsafe { &*map }
            .map
            .get(key)
            .and_then(|value| value.as_i64())
            .and_then(|value| i32::try_from(value).ok());
        match value {
            Some(value) => {
                unsafe { *out = value };
                SysaStatus::Ok
            }
            None => crate::error::fail(SysaStatus::NotFound, format!("{key} is not set")),
        }
    })
}

/// Read a file-mode directive; `NOT_FOUND` when absent or not a mode.
#[no_mangle]
pub extern "C" fn sysa_config_map_get_mode(
    map: *const SysaConfigMap,
    key: *const c_char,
    out: *mut u32,
) -> SysaStatus {
    ffi_try!(SysaStatus::Internal, {
        if map.is_null() || out.is_null() {
            return crate::error::fail(SysaStatus::InvalidArgument, "argument is null");
        }
        let key = match unsafe { require_str(key, "key") } {
            Ok(key) => key,
            Err(status) => return status,
        };
        match config::get_mode(&unsafe { &*map }.map, key) {
            Some(value) => {
                unsafe { *out = value };
                SysaStatus::Ok
            }
            None => crate::error::fail(SysaStatus::NotFound, format!("{key} is not set")),
        }
    })
}

/// Serialize one raw JSON value; `NOT_FOUND` when the key is absent.
#[no_mangle]
pub extern "C" fn sysa_config_map_get_json(
    map: *const SysaConfigMap,
    key: *const c_char,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> SysaStatus {
    ffi_try!(SysaStatus::Internal, {
        if map.is_null() {
            return crate::error::fail(SysaStatus::InvalidArgument, "map is null");
        }
        let key = match unsafe { require_str(key, "key") } {
            Ok(key) => key,
            Err(status) => return status,
        };
        let Some(value) = unsafe { &*map }.map.get(key) else {
            return crate::error::fail(SysaStatus::NotFound, format!("{key} is not set"));
        };
        let bytes = match serde_json::to_vec(value) {
            Ok(bytes) => bytes,
            Err(error) => return crate::error::report(error.into()),
        };
        crate::ffi::check(write_out_bytes(out, out_len, bytes))
    })
}

/// Release a map returned by this library.
#[no_mangle]
pub extern "C" fn sysa_config_map_free(map: *mut SysaConfigMap) {
    ffi_try!((), {
        if !map.is_null() {
            drop(unsafe { Box::from_raw(map) });
        }
    })
}
