//! The data layer: ConfigMap, UnitIR and UnitStatus over the C surface.

use std::ffi::{c_char, CStr, CString};
use std::ptr;

use systema_shared::controller::UnitStatus as RustUnitStatus;
use systema_shared::ir::{TemplateInfo, UnitIR, UnitType};
use systema_shared_capi::config_map::{
    sysa_config_map_free, sysa_config_map_get_bool, sysa_config_map_get_i32,
    sysa_config_map_get_json, sysa_config_map_get_mode, sysa_config_map_get_string,
    sysa_config_map_get_strings, sysa_config_map_get_u32, sysa_config_map_parse_json,
    sysa_config_map_to_json, SysaConfigMap,
};
use systema_shared_capi::status::SysaStatus;
use systema_shared_capi::unit_ir::{
    sysa_unit_ir_aliases, sysa_unit_ir_config, sysa_unit_ir_description, sysa_unit_ir_free,
    sysa_unit_ir_from_json, sysa_unit_ir_id, sysa_unit_ir_is_template,
    sysa_unit_ir_is_template_instance, sysa_unit_ir_owner, sysa_unit_ir_set_owner,
    sysa_unit_ir_slice, sysa_unit_ir_source_format, sysa_unit_ir_source_path,
    sysa_unit_ir_template_of, sysa_unit_ir_template_parameters, sysa_unit_ir_to_json,
    sysa_unit_ir_unit_type, SysaUnitIR,
};
use systema_shared_capi::unit_status::{
    sysa_unit_status_active_state, sysa_unit_status_decode, sysa_unit_status_encode,
    sysa_unit_status_extension_count, sysa_unit_status_extension_key,
    sysa_unit_status_extension_value, sysa_unit_status_free, sysa_unit_status_invocation_id,
    sysa_unit_status_main_pid, sysa_unit_status_new, sysa_unit_status_set_extension,
    sysa_unit_status_sub_state, sysa_unit_status_unit_name, SysaUnitStatus,
};
use systema_shared_capi::{sysa_bytes_free, sysa_string_free, SysaStringList};

fn take_string(pointer: *mut c_char) -> Option<String> {
    if pointer.is_null() {
        None
    } else {
        let value = unsafe { CStr::from_ptr(pointer) }
            .to_string_lossy()
            .into_owned();
        sysa_string_free(pointer);
        Some(value)
    }
}

fn take_bytes(pointer: *mut u8, len: usize) -> Vec<u8> {
    let data = unsafe { std::slice::from_raw_parts(pointer, len) }.to_vec();
    sysa_bytes_free(pointer, len);
    data
}

fn borrow(pointer: *const c_char) -> String {
    unsafe { CStr::from_ptr(pointer) }
        .to_string_lossy()
        .into_owned()
}

#[test]
fn config_map_reads_typed_directives() {
    let json =
        br#"{"ExecStart":"/bin/true","Timeout":5,"Flag":true,"Mask":"0644","Wants":["a","b"]}"#;
    let mut map: *mut SysaConfigMap = ptr::null_mut();
    let status = sysa_config_map_parse_json(json.as_ptr(), json.len(), &mut map);
    assert_eq!(status, SysaStatus::Ok);
    assert!(!map.is_null());

    let key = CString::new("ExecStart").unwrap();
    let mut text = ptr::null_mut();
    assert_eq!(
        sysa_config_map_get_string(map, key.as_ptr(), &mut text),
        SysaStatus::Ok
    );
    assert_eq!(take_string(text).as_deref(), Some("/bin/true"));

    let timeout = CString::new("Timeout").unwrap();
    let mut number = 0i32;
    assert_eq!(
        sysa_config_map_get_i32(map, timeout.as_ptr(), &mut number),
        SysaStatus::Ok
    );
    assert_eq!(number, 5);

    let mut unsigned = 0u32;
    assert_eq!(
        sysa_config_map_get_u32(map, timeout.as_ptr(), &mut unsigned),
        SysaStatus::Ok
    );
    assert_eq!(unsigned, 5);

    let flag = CString::new("Flag").unwrap();
    let mut enabled = 0u8;
    assert_eq!(
        sysa_config_map_get_bool(map, flag.as_ptr(), &mut enabled),
        SysaStatus::Ok
    );
    assert_eq!(enabled, 1);

    let mask = CString::new("Mask").unwrap();
    let mut mode = 0u32;
    assert_eq!(
        sysa_config_map_get_mode(map, mask.as_ptr(), &mut mode),
        SysaStatus::Ok
    );
    assert_eq!(mode, 0o644);

    let wants = CString::new("Wants").unwrap();
    let mut list: *mut SysaStringList = ptr::null_mut();
    assert_eq!(
        sysa_config_map_get_strings(map, wants.as_ptr(), &mut list),
        SysaStatus::Ok
    );
    assert_eq!(systema_shared_capi::sysa_string_list_len(list), 2);
    let first = systema_shared_capi::sysa_string_list_get(list, 0);
    assert_eq!(borrow(first), "a");
    systema_shared_capi::sysa_string_list_free(list);

    let mut out = ptr::null_mut();
    let mut out_len = 0usize;
    assert_eq!(
        sysa_config_map_get_json(map, flag.as_ptr(), &mut out, &mut out_len),
        SysaStatus::Ok
    );
    assert_eq!(take_bytes(out, out_len), b"true");

    let mut json_out = ptr::null_mut();
    let mut json_len = 0usize;
    assert_eq!(
        sysa_config_map_to_json(map, &mut json_out, &mut json_len),
        SysaStatus::Ok
    );
    let round_trip: serde_json::Value =
        serde_json::from_slice(&take_bytes(json_out, json_len)).unwrap();
    assert_eq!(round_trip["ExecStart"], "/bin/true");

    sysa_config_map_free(map);
}

#[test]
fn missing_directives_report_not_found() {
    let json = br#"{}"#;
    let mut map: *mut SysaConfigMap = ptr::null_mut();
    assert_eq!(
        sysa_config_map_parse_json(json.as_ptr(), json.len(), &mut map),
        SysaStatus::Ok
    );
    let key = CString::new("Missing").unwrap();
    let mut text = ptr::null_mut();
    assert_eq!(
        sysa_config_map_get_string(map, key.as_ptr(), &mut text),
        SysaStatus::NotFound
    );
    assert!(text.is_null());
    sysa_config_map_free(map);
}

fn ir_json(ir: &UnitIR) -> Vec<u8> {
    serde_json::to_vec(ir).unwrap()
}

#[test]
fn unit_ir_round_trips_and_reports_its_fields() {
    let ir = UnitIR {
        id: "demo.service".to_string(),
        unit_type: Some(UnitType::Service),
        description: Some("Demo".to_string()),
        source_format: Some("systemd".to_string()),
        source_path: Some("/etc/systemd/system/demo.service".to_string()),
        aliases: vec!["demo-alias.service".to_string()],
        slice: Some("system.slice".to_string()),
        template: None,
        ..Default::default()
    };
    let json = ir_json(&ir);
    let mut handle: *mut SysaUnitIR = ptr::null_mut();
    assert_eq!(
        sysa_unit_ir_from_json(json.as_ptr(), json.len(), &mut handle),
        SysaStatus::Ok
    );
    assert!(!handle.is_null());

    let mut out = ptr::null_mut();
    assert_eq!(sysa_unit_ir_id(handle, &mut out), SysaStatus::Ok);
    assert_eq!(take_string(out).as_deref(), Some("demo.service"));

    let mut unit_type = ptr::null_mut();
    assert_eq!(
        sysa_unit_ir_unit_type(handle, &mut unit_type),
        SysaStatus::Ok
    );
    assert_eq!(take_string(unit_type).as_deref(), Some("service"));

    let mut description = ptr::null_mut();
    sysa_unit_ir_description(handle, &mut description);
    assert_eq!(take_string(description).as_deref(), Some("Demo"));

    let mut format = ptr::null_mut();
    sysa_unit_ir_source_format(handle, &mut format);
    assert_eq!(take_string(format).as_deref(), Some("systemd"));

    let mut path = ptr::null_mut();
    sysa_unit_ir_source_path(handle, &mut path);
    assert_eq!(
        take_string(path).as_deref(),
        Some("/etc/systemd/system/demo.service")
    );

    let mut slice = ptr::null_mut();
    sysa_unit_ir_slice(handle, &mut slice);
    assert_eq!(take_string(slice).as_deref(), Some("system.slice"));

    // Owner: unset by default, settable, and clearable with NULL.
    let mut missing_owner = ptr::null_mut();
    assert_eq!(
        sysa_unit_ir_owner(handle, &mut missing_owner),
        SysaStatus::NotFound
    );
    assert!(missing_owner.is_null());

    let custom = CString::new("custom-worker").unwrap();
    assert_eq!(
        sysa_unit_ir_set_owner(handle, custom.as_ptr()),
        SysaStatus::Ok
    );
    let mut owner = ptr::null_mut();
    assert_eq!(sysa_unit_ir_owner(handle, &mut owner), SysaStatus::Ok);
    assert_eq!(take_string(owner).as_deref(), Some("custom-worker"));

    assert_eq!(sysa_unit_ir_set_owner(handle, ptr::null()), SysaStatus::Ok);
    let mut cleared = ptr::null_mut();
    assert_eq!(
        sysa_unit_ir_owner(handle, &mut cleared),
        SysaStatus::NotFound
    );
    assert!(cleared.is_null());

    let mut aliases: *mut SysaStringList = ptr::null_mut();
    assert_eq!(sysa_unit_ir_aliases(handle, &mut aliases), SysaStatus::Ok);
    assert_eq!(systema_shared_capi::sysa_string_list_len(aliases), 1);
    systema_shared_capi::sysa_string_list_free(aliases);

    assert_eq!(sysa_unit_ir_is_template(handle), 0);
    assert_eq!(sysa_unit_ir_is_template_instance(handle), 0);

    let mut out = ptr::null_mut();
    let mut out_len = 0usize;
    assert_eq!(
        sysa_unit_ir_to_json(handle, &mut out, &mut out_len),
        SysaStatus::Ok
    );
    let decoded: UnitIR = serde_json::from_slice(&take_bytes(out, out_len)).unwrap();
    assert_eq!(decoded.id, ir.id);
    assert_eq!(decoded.unit_type, ir.unit_type);
    assert_eq!(decoded.description, ir.description);

    sysa_unit_ir_free(handle);
}

#[test]
fn unit_ir_reports_template_metadata_and_config() {
    let mut config = serde_json::Map::new();
    config.insert(
        "ExecStart".to_string(),
        serde_json::Value::String("/bin/getty %i".to_string()),
    );
    let ir = UnitIR {
        id: "getty@.service".to_string(),
        unit_type: Some(UnitType::Service),
        config: Some(config),
        template: Some(TemplateInfo {
            definition: true,
            of: None,
            parameters: Vec::new(),
        }),
        ..Default::default()
    };
    let json = ir_json(&ir);
    let mut handle: *mut SysaUnitIR = ptr::null_mut();
    assert_eq!(
        sysa_unit_ir_from_json(json.as_ptr(), json.len(), &mut handle),
        SysaStatus::Ok
    );

    assert_eq!(sysa_unit_ir_is_template(handle), 1);
    assert_eq!(sysa_unit_ir_is_template_instance(handle), 0);

    let mut parameters: *mut SysaStringList = ptr::null_mut();
    assert_eq!(
        sysa_unit_ir_template_parameters(handle, &mut parameters),
        SysaStatus::Ok
    );
    assert_eq!(systema_shared_capi::sysa_string_list_len(parameters), 0);
    systema_shared_capi::sysa_string_list_free(parameters);

    let mut map: *mut SysaConfigMap = ptr::null_mut();
    assert_eq!(sysa_unit_ir_config(handle, &mut map), SysaStatus::Ok);
    let key = CString::new("ExecStart").unwrap();
    let mut value = ptr::null_mut();
    assert_eq!(
        sysa_config_map_get_string(map, key.as_ptr(), &mut value),
        SysaStatus::Ok
    );
    assert_eq!(take_string(value).as_deref(), Some("/bin/getty %i"));
    sysa_config_map_free(map);

    sysa_unit_ir_free(handle);
}

#[test]
fn unit_ir_reports_template_instances() {
    let ir = UnitIR {
        id: "getty@tty3.service".to_string(),
        unit_type: Some(UnitType::Service),
        template: Some(TemplateInfo {
            definition: false,
            of: Some("getty@.service".to_string()),
            parameters: vec!["tty3".to_string()],
        }),
        ..Default::default()
    };
    let json = ir_json(&ir);
    let mut handle: *mut SysaUnitIR = ptr::null_mut();
    assert_eq!(
        sysa_unit_ir_from_json(json.as_ptr(), json.len(), &mut handle),
        SysaStatus::Ok
    );
    assert_eq!(sysa_unit_ir_is_template(handle), 0);
    assert_eq!(sysa_unit_ir_is_template_instance(handle), 1);

    let mut template = ptr::null_mut();
    assert_eq!(
        sysa_unit_ir_template_of(handle, &mut template),
        SysaStatus::Ok
    );
    assert_eq!(take_string(template).as_deref(), Some("getty@.service"));

    let mut parameters: *mut SysaStringList = ptr::null_mut();
    sysa_unit_ir_template_parameters(handle, &mut parameters);
    assert_eq!(systema_shared_capi::sysa_string_list_len(parameters), 1);
    let first = systema_shared_capi::sysa_string_list_get(parameters, 0);
    assert_eq!(borrow(first), "tty3");
    systema_shared_capi::sysa_string_list_free(parameters);

    sysa_unit_ir_free(handle);
}

#[test]
fn unit_status_encodes_and_decodes() {
    let name = CString::new("demo.service").unwrap();
    let active = CString::new("active").unwrap();
    let sub = CString::new("running").unwrap();
    let invocation = CString::new("inv-1").unwrap();
    let status: *mut SysaUnitStatus = sysa_unit_status_new(
        name.as_ptr(),
        active.as_ptr(),
        sub.as_ptr(),
        42,
        invocation.as_ptr(),
    );
    assert!(!status.is_null());

    let key = CString::new("LogNamespace").unwrap();
    let value = CString::new("demo").unwrap();
    assert_eq!(
        sysa_unit_status_set_extension(status, key.as_ptr(), value.as_ptr()),
        SysaStatus::Ok
    );
    assert_eq!(sysa_unit_status_extension_count(status), 1);
    assert_eq!(
        borrow(sysa_unit_status_extension_key(status, 0)),
        "LogNamespace"
    );
    assert_eq!(borrow(sysa_unit_status_extension_value(status, 0)), "demo");

    assert_eq!(borrow(sysa_unit_status_unit_name(status)), "demo.service");
    assert_eq!(borrow(sysa_unit_status_active_state(status)), "active");
    assert_eq!(borrow(sysa_unit_status_sub_state(status)), "running");
    assert_eq!(borrow(sysa_unit_status_invocation_id(status)), "inv-1");
    assert_eq!(sysa_unit_status_main_pid(status), 42);

    let mut bytes = ptr::null_mut();
    let mut len = 0usize;
    assert_eq!(
        sysa_unit_status_encode(status, &mut bytes, &mut len),
        SysaStatus::Ok
    );
    let payload = take_bytes(bytes, len);

    let expected = RustUnitStatus::decode_from(&payload).expect("valid protobuf");
    assert_eq!(expected.unit_name, "demo.service");
    assert_eq!(
        expected.extensions.get("LogNamespace").map(String::as_str),
        Some("demo")
    );

    let mut decoded: *mut SysaUnitStatus = ptr::null_mut();
    assert_eq!(
        sysa_unit_status_decode(payload.as_ptr(), payload.len(), &mut decoded),
        SysaStatus::Ok
    );
    assert_eq!(borrow(sysa_unit_status_unit_name(decoded)), "demo.service");
    assert_eq!(sysa_unit_status_extension_count(decoded), 1);

    sysa_unit_status_free(decoded);
    sysa_unit_status_free(status);
}
