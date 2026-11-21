//! C-visible `UnitIR`: JSON round-trip plus accessors for the common fields.

use std::ffi::c_char;

use systema_shared::ir::UnitIR;

use crate::ffi::{require_bytes, write_out_bytes};
use crate::status::SystemaStatus;

/// Opaque, library-owned `UnitIR`.
pub struct SystemaUnitIR {
    ir: UnitIR,
}

/// Parse a unit IR from JSON.
#[no_mangle]
pub extern "C" fn systema_shared_unit_ir_from_json(
    data: *const u8,
    len: usize,
    out: *mut *mut SystemaUnitIR,
) -> SystemaStatus {
    ffi_try!(SystemaStatus::Internal, {
        if out.is_null() {
            return crate::error::fail(SystemaStatus::InvalidArgument, "out is null");
        }
        let bytes = match unsafe { require_bytes(data, len, "data") } {
            Ok(bytes) => bytes,
            Err(status) => return status,
        };
        match serde_json::from_slice::<UnitIR>(bytes) {
            Ok(ir) => {
                unsafe { *out = Box::into_raw(Box::new(SystemaUnitIR { ir })) };
                SystemaStatus::Ok
            }
            Err(error) => {
                unsafe { *out = std::ptr::null_mut() };
                crate::error::report(error.into())
            }
        }
    })
}

/// Serialize the IR back to JSON.
#[no_mangle]
pub extern "C" fn systema_shared_unit_ir_to_json(
    ir: *const SystemaUnitIR,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> SystemaStatus {
    ffi_try!(SystemaStatus::Internal, {
        if ir.is_null() {
            return crate::error::fail(SystemaStatus::InvalidArgument, "ir is null");
        }
        match serde_json::to_vec(&unsafe { &*ir }.ir) {
            Ok(bytes) => crate::ffi::check(write_out_bytes(out, out_len, bytes)),
            Err(error) => crate::error::report(error.into()),
        }
    })
}

fn with_ir<T>(ir: *const SystemaUnitIR, read: impl FnOnce(&UnitIR) -> T) -> Option<T> {
    if ir.is_null() {
        crate::error::fail(SystemaStatus::InvalidArgument, "ir is null");
        return None;
    }
    Some(read(&unsafe { &*ir }.ir))
}

fn optional_string(value: Option<&str>, out: *mut *mut c_char, what: &str) -> SystemaStatus {
    if out.is_null() {
        return crate::error::fail(SystemaStatus::InvalidArgument, "out is null");
    }
    match value {
        Some(value) => {
            unsafe { *out = crate::ffi::into_c_string(value) };
            SystemaStatus::Ok
        }
        None => {
            unsafe { *out = std::ptr::null_mut() };
            crate::error::fail(SystemaStatus::NotFound, format!("{what} is not set"))
        }
    }
}

fn string_field(
    ir: *const SystemaUnitIR,
    out: *mut *mut c_char,
    what: &str,
    read: impl FnOnce(&UnitIR) -> Option<String>,
) -> SystemaStatus {
    if ir.is_null() {
        return crate::error::fail(SystemaStatus::InvalidArgument, "ir is null");
    }
    let value = read(&unsafe { &*ir }.ir);
    optional_string(value.as_deref(), out, what)
}

/// Copy the unit ID (free with `systema_shared_string_free`).
#[no_mangle]
pub extern "C" fn systema_shared_unit_ir_id(
    ir: *const SystemaUnitIR,
    out: *mut *mut c_char,
) -> SystemaStatus {
    ffi_try!(SystemaStatus::Internal, {
        string_field(ir, out, "id", |ir| Some(ir.id.clone()))
    })
}

/// Copy the unit type name (free with `systema_shared_string_free`).
#[no_mangle]
pub extern "C" fn systema_shared_unit_ir_unit_type(
    ir: *const SystemaUnitIR,
    out: *mut *mut c_char,
) -> SystemaStatus {
    ffi_try!(SystemaStatus::Internal, {
        string_field(ir, out, "unit_type", |ir| {
            ir.unit_type
                .as_ref()
                .map(|unit_type| unit_type.as_str().to_string())
        })
    })
}

/// Copy the description (free with `systema_shared_string_free`).
#[no_mangle]
pub extern "C" fn systema_shared_unit_ir_description(
    ir: *const SystemaUnitIR,
    out: *mut *mut c_char,
) -> SystemaStatus {
    ffi_try!(SystemaStatus::Internal, {
        string_field(ir, out, "description", |ir| ir.description.clone())
    })
}

/// Copy the source format (free with `systema_shared_string_free`).
#[no_mangle]
pub extern "C" fn systema_shared_unit_ir_source_format(
    ir: *const SystemaUnitIR,
    out: *mut *mut c_char,
) -> SystemaStatus {
    ffi_try!(SystemaStatus::Internal, {
        string_field(ir, out, "source_format", |ir| ir.source_format.clone())
    })
}

/// Copy the source path (free with `systema_shared_string_free`).
#[no_mangle]
pub extern "C" fn systema_shared_unit_ir_source_path(
    ir: *const SystemaUnitIR,
    out: *mut *mut c_char,
) -> SystemaStatus {
    ffi_try!(SystemaStatus::Internal, {
        string_field(ir, out, "source_path", |ir| ir.source_path.clone())
    })
}

/// Copy the pinned owner worker (free with `systema_shared_string_free`);
/// `NOT_FOUND` when type-based routing applies.
#[no_mangle]
pub extern "C" fn systema_shared_unit_ir_owner(
    ir: *const SystemaUnitIR,
    out: *mut *mut c_char,
) -> SystemaStatus {
    ffi_try!(SystemaStatus::Internal, {
        string_field(ir, out, "owner", |ir| ir.owner.clone())
    })
}

/// Pin the owning worker, or clear the pin with a `NULL` value.
///
/// The owner is a worker id as announced in `worker.register`.  With a pin,
/// every task and event for this unit goes to that worker even when it does
/// not advertise the unit's type; clearing it restores type-based routing.
#[no_mangle]
pub extern "C" fn systema_shared_unit_ir_set_owner(
    ir: *mut SystemaUnitIR,
    owner: *const c_char,
) -> SystemaStatus {
    ffi_try!(SystemaStatus::Internal, {
        if ir.is_null() {
            return crate::error::fail(SystemaStatus::InvalidArgument, "ir is null");
        }
        let owner = if owner.is_null() {
            None
        } else {
            match unsafe { crate::ffi::require_str(owner, "owner") } {
                Ok(owner) => Some(owner.to_string()),
                Err(status) => return status,
            }
        };
        unsafe { &mut *ir }.ir.owner = owner;
        SystemaStatus::Ok
    })
}

/// Copy the alias target (free with `systema_shared_string_free`).
#[no_mangle]
pub extern "C" fn systema_shared_unit_ir_alias_of(
    ir: *const SystemaUnitIR,
    out: *mut *mut c_char,
) -> SystemaStatus {
    ffi_try!(SystemaStatus::Internal, {
        string_field(ir, out, "alias_of", |ir| ir.alias_of.clone())
    })
}

/// Copy the slice name (free with `systema_shared_string_free`).
#[no_mangle]
pub extern "C" fn systema_shared_unit_ir_slice(
    ir: *const SystemaUnitIR,
    out: *mut *mut c_char,
) -> SystemaStatus {
    ffi_try!(SystemaStatus::Internal, {
        string_field(ir, out, "slice", |ir| ir.slice.clone())
    })
}

/// Copy the template this IR belongs to (free with
/// `systema_shared_string_free`).
#[no_mangle]
pub extern "C" fn systema_shared_unit_ir_template_of(
    ir: *const SystemaUnitIR,
    out: *mut *mut c_char,
) -> SystemaStatus {
    ffi_try!(SystemaStatus::Internal, {
        string_field(ir, out, "template_of", |ir| {
            ir.template_of().map(str::to_string)
        })
    })
}

/// Copy the reported alias list.
#[no_mangle]
pub extern "C" fn systema_shared_unit_ir_aliases(
    ir: *const SystemaUnitIR,
    out: *mut *mut crate::ffi::SystemaStringList,
) -> SystemaStatus {
    ffi_try!(SystemaStatus::Internal, {
        if out.is_null() {
            return crate::error::fail(SystemaStatus::InvalidArgument, "out is null");
        }
        match with_ir(ir, |ir| ir.aliases.clone()) {
            Some(aliases) => {
                unsafe { *out = crate::ffi::into_string_list(aliases) };
                SystemaStatus::Ok
            }
            None => SystemaStatus::InvalidArgument,
        }
    })
}

/// Copy the type-specific directive map; `NOT_FOUND` when the IR carries none.
#[no_mangle]
pub extern "C" fn systema_shared_unit_ir_config(
    ir: *const SystemaUnitIR,
    out: *mut *mut crate::config_map::SystemaConfigMap,
) -> SystemaStatus {
    ffi_try!(SystemaStatus::Internal, {
        if out.is_null() {
            return crate::error::fail(SystemaStatus::InvalidArgument, "out is null");
        }
        match with_ir(ir, |ir| ir.config.clone()) {
            Some(Some(map)) => {
                unsafe {
                    *out = Box::into_raw(Box::new(crate::config_map::SystemaConfigMap::new(map)))
                };
                SystemaStatus::Ok
            }
            Some(None) => {
                unsafe { *out = std::ptr::null_mut() };
                crate::error::fail(SystemaStatus::NotFound, "config is not reported")
            }
            None => SystemaStatus::InvalidArgument,
        }
    })
}

/// Whether the IR is a template definition.
#[no_mangle]
pub extern "C" fn systema_shared_unit_ir_is_template(ir: *const SystemaUnitIR) -> u8 {
    ffi_try!(0, {
        match with_ir(ir, UnitIR::is_template) {
            Some(value) => u8::from(value),
            None => 0,
        }
    })
}

/// Whether the IR is an instance of a template.
#[no_mangle]
pub extern "C" fn systema_shared_unit_ir_is_template_instance(ir: *const SystemaUnitIR) -> u8 {
    ffi_try!(0, {
        match with_ir(ir, UnitIR::is_template_instance) {
            Some(value) => u8::from(value),
            None => 0,
        }
    })
}

/// Copy the template parameter list (empty when the IR has no template).
#[no_mangle]
pub extern "C" fn systema_shared_unit_ir_template_parameters(
    ir: *const SystemaUnitIR,
    out: *mut *mut crate::ffi::SystemaStringList,
) -> SystemaStatus {
    ffi_try!(SystemaStatus::Internal, {
        if out.is_null() {
            return crate::error::fail(SystemaStatus::InvalidArgument, "out is null");
        }
        match with_ir(ir, |ir| {
            ir.template_parameters()
                .iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>()
        }) {
            Some(values) => {
                unsafe { *out = crate::ffi::into_string_list(values) };
                SystemaStatus::Ok
            }
            None => SystemaStatus::InvalidArgument,
        }
    })
}

/// Release an IR returned by this library.
#[no_mangle]
pub extern "C" fn systema_shared_unit_ir_free(ir: *mut SystemaUnitIR) {
    ffi_try!((), {
        if !ir.is_null() {
            drop(unsafe { Box::from_raw(ir) });
        }
    })
}
