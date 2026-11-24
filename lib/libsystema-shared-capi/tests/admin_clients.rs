//! The finder, staging-admin and unit-state-admin clients against an
//! in-process fake System A.

use std::ffi::{c_char, c_void, CStr, CString};
use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::{Path, PathBuf};
use std::ptr;
use std::thread;

use prost::Message;
use systema_shared::ipc::{
    frame_stream, make_envelope, recv_envelope, send_envelope, EnvelopeFramed, IPC_VERSION,
    METHOD_VERSION, METHOD_VERSION_RESULT,
};
use systema_shared::proto::{
    AdminStagingOp, AdminStagingResult, CommitUnits, IpcVersionRequest, IpcVersionResult,
    RegisterUnits, StagingAreaEntry, StagingQuery, StagingQueryResult, UnitRegistrationAck,
    UnitStateEntry, UnitStateEof, UnitStateListRequest,
};
use systema_shared_capi::finder::{
    sysa_finder_commit_units, sysa_finder_free, sysa_finder_new, sysa_finder_query_staging,
    sysa_finder_register_units, SysaFinder,
};
use systema_shared_capi::staging_admin::{
    sysa_staging_admin_commit, sysa_staging_admin_free, sysa_staging_admin_list,
    sysa_staging_admin_new, sysa_staging_admin_query, sysa_staging_admin_query_all,
    sysa_staging_admin_query_by_name, sysa_staging_admin_query_by_uid, sysa_staging_list_free,
    sysa_staging_list_len, sysa_staging_list_name, sysa_staging_list_uid, SysaStagingAdmin,
    SysaStagingList,
};
use systema_shared_capi::status::SysaStatus;
use systema_shared_capi::sysa_bytes_free;
use systema_shared_capi::unitstate_admin::{
    sysa_unitstate_admin_free, sysa_unitstate_admin_list, sysa_unitstate_admin_new,
    SysaUnitStateAdmin,
};

fn socket_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("systema-capi-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn spawn_server<F, Fut>(path: &Path, serve: F) -> thread::JoinHandle<()>
where
    F: FnOnce(tokio::net::UnixListener) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + 'static,
{
    let listener = StdUnixListener::bind(path).unwrap();
    listener.set_nonblocking(true).unwrap();
    thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let listener = tokio::net::UnixListener::from_std(listener).unwrap();
            serve(listener).await;
        });
    })
}

async fn handshake(framed: &mut EnvelopeFramed) -> systema_shared::proto::Envelope {
    let first = recv_envelope(framed)
        .await
        .unwrap()
        .expect("a version request arrives first");
    assert_eq!(first.method, METHOD_VERSION);
    let request = IpcVersionRequest::decode(first.payload.as_slice()).unwrap();
    assert_eq!(request.version, IPC_VERSION);
    let result = IpcVersionResult {
        version: IPC_VERSION,
        compatible: true,
        message: String::new(),
    };
    send_envelope(
        framed,
        &make_envelope(
            first.request_id,
            "system-a",
            "client",
            METHOD_VERSION_RESULT,
            result,
        )
        .unwrap(),
    )
    .await
    .unwrap();
    recv_envelope(framed)
        .await
        .unwrap()
        .expect("the operation follows the handshake")
}

fn take_bytes(pointer: *mut u8, len: usize) -> Vec<u8> {
    let data = unsafe { std::slice::from_raw_parts(pointer, len) }.to_vec();
    sysa_bytes_free(pointer, len);
    data
}

async fn serve_finder(listener: tokio::net::UnixListener) {
    for _ in 0..3 {
        let (stream, _) = listener.accept().await.unwrap();
        let mut framed = frame_stream(stream);
        let env = handshake(&mut framed).await;
        match env.method.as_str() {
            "finder.register_units" => {
                let register = RegisterUnits::decode(env.payload.as_slice()).unwrap();
                assert_eq!(register.name, "area");
                assert_eq!(register.units_json, b"{}".to_vec());
                let ack = UnitRegistrationAck {
                    success: true,
                    message: "registered".to_string(),
                    unit_count: 1,
                };
                send_envelope(
                    &mut framed,
                    &make_envelope(env.request_id, "system-a", "system-f", "finder.ack", ack)
                        .unwrap(),
                )
                .await
                .unwrap();
            }
            "finder.commit_units" => {
                let commit = CommitUnits::decode(env.payload.as_slice()).unwrap();
                assert_eq!(commit.name, "area");
                let ack = UnitRegistrationAck {
                    success: true,
                    message: "committed".to_string(),
                    unit_count: 1,
                };
                send_envelope(
                    &mut framed,
                    &make_envelope(env.request_id, "system-a", "system-f", "finder.ack", ack)
                        .unwrap(),
                )
                .await
                .unwrap();
            }
            "staging.query" => {
                let query = StagingQuery::decode(env.payload.as_slice()).unwrap();
                assert_eq!(query.name, "area");
                let result = StagingQueryResult {
                    success: true,
                    message: String::new(),
                    units_json: b"{\"demo.service\":{}}".to_vec(),
                    unit_count: 1,
                };
                send_envelope(
                    &mut framed,
                    &make_envelope(
                        env.request_id,
                        "system-a",
                        "system-f",
                        "staging.query_result",
                        result,
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
            }
            other => panic!("unexpected finder request {other}"),
        }
    }
}

#[test]
fn finder_operations_round_trip() {
    let dir = socket_dir("finder");
    let path = dir.join("allocator.sock");
    let server = spawn_server(&path, serve_finder);

    let path_text = CString::new(path.to_str().unwrap()).unwrap();
    let finder: *mut SysaFinder = sysa_finder_new(path_text.as_ptr());
    assert!(!finder.is_null());

    let name = CString::new("area").unwrap();
    let mut out = ptr::null_mut();
    let mut len = 0usize;
    assert_eq!(
        sysa_finder_register_units(finder, name.as_ptr(), b"{}".as_ptr(), 2, &mut out, &mut len),
        SysaStatus::Ok
    );
    let ack = UnitRegistrationAck::decode(take_bytes(out, len).as_slice()).unwrap();
    assert!(ack.success);

    let mut out = ptr::null_mut();
    let mut len = 0usize;
    assert_eq!(
        sysa_finder_commit_units(finder, name.as_ptr(), &mut out, &mut len),
        SysaStatus::Ok
    );
    let ack = UnitRegistrationAck::decode(take_bytes(out, len).as_slice()).unwrap();
    assert!(ack.success);

    let mut out = ptr::null_mut();
    let mut len = 0usize;
    assert_eq!(
        sysa_finder_query_staging(finder, name.as_ptr(), &mut out, &mut len),
        SysaStatus::Ok
    );
    let result = StagingQueryResult::decode(take_bytes(out, len).as_slice()).unwrap();
    assert!(result.success);
    assert_eq!(result.unit_count, 1);

    sysa_finder_free(finder);
    server.join().unwrap();
}

async fn reply_staging(framed: &mut EnvelopeFramed, request_id: u64, op: &str, entries: bool) {
    let result = AdminStagingResult {
        success: true,
        message: op.to_string(),
        entries: if entries {
            vec![StagingAreaEntry {
                uid: 1000,
                name: "area".to_string(),
                unit_count: 1,
                units_json: b"{}".to_vec(),
            }]
        } else {
            Vec::new()
        },
    };
    send_envelope(
        framed,
        &make_envelope(request_id, "system-a", "", "admin.staging.result", result).unwrap(),
    )
    .await
    .unwrap();
}

async fn serve_staging(listener: tokio::net::UnixListener) {
    let expected = ["list", "by_uid", "by_name", "by_id", "all", "commit"];
    for op in expected {
        let (stream, _) = listener.accept().await.unwrap();
        let mut framed = frame_stream(stream);
        let env = handshake(&mut framed).await;
        assert_eq!(env.method, "admin.staging");
        let request = AdminStagingOp::decode(env.payload.as_slice()).unwrap();
        assert_eq!(request.op, op);
        match op {
            "by_uid" => assert_eq!(request.uid, 1000),
            "by_name" => assert_eq!(request.name, "area"),
            "by_id" | "commit" => {
                assert_eq!(request.uid, 1000);
                assert_eq!(request.name, "area");
            }
            _ => {}
        }
        reply_staging(&mut framed, env.request_id, op, true).await;
    }
}

#[test]
fn staging_admin_operations_round_trip() {
    let dir = socket_dir("staging");
    let path = dir.join("control.socket");
    let server = spawn_server(&path, serve_staging);

    let path_text = CString::new(path.to_str().unwrap()).unwrap();
    let admin: *mut SysaStagingAdmin = sysa_staging_admin_new(path_text.as_ptr());
    assert!(!admin.is_null());

    let mut list: *mut SysaStagingList = ptr::null_mut();
    assert_eq!(sysa_staging_admin_list(admin, &mut list), SysaStatus::Ok);
    assert_eq!(sysa_staging_list_len(list), 1);
    assert_eq!(sysa_staging_list_uid(list, 0), 1000);
    let name = sysa_staging_list_name(list, 0);
    assert_eq!(unsafe { CStr::from_ptr(name) }.to_str().unwrap(), "area");
    sysa_staging_list_free(list);

    let name = CString::new("area").unwrap();
    let mut out = ptr::null_mut();
    let mut len = 0usize;
    assert_eq!(
        sysa_staging_admin_query_by_uid(admin, 1000, &mut out, &mut len),
        SysaStatus::Ok
    );
    let _ = AdminStagingResult::decode(take_bytes(out, len).as_slice()).unwrap();

    let mut out = ptr::null_mut();
    let mut len = 0usize;
    assert_eq!(
        sysa_staging_admin_query_by_name(admin, name.as_ptr(), &mut out, &mut len),
        SysaStatus::Ok
    );
    let _ = AdminStagingResult::decode(take_bytes(out, len).as_slice()).unwrap();

    let mut out = ptr::null_mut();
    let mut len = 0usize;
    assert_eq!(
        sysa_staging_admin_query(admin, 1000, name.as_ptr(), &mut out, &mut len),
        SysaStatus::Ok
    );
    let _ = AdminStagingResult::decode(take_bytes(out, len).as_slice()).unwrap();

    let mut out = ptr::null_mut();
    let mut len = 0usize;
    assert_eq!(
        sysa_staging_admin_query_all(admin, &mut out, &mut len),
        SysaStatus::Ok
    );
    let _ = AdminStagingResult::decode(take_bytes(out, len).as_slice()).unwrap();

    let mut out = ptr::null_mut();
    let mut len = 0usize;
    assert_eq!(
        sysa_staging_admin_commit(admin, 1000, name.as_ptr(), &mut out, &mut len),
        SysaStatus::Ok
    );
    let _ = AdminStagingResult::decode(take_bytes(out, len).as_slice()).unwrap();

    sysa_staging_admin_free(admin);
    server.join().unwrap();
}

async fn serve_unitstate(listener: tokio::net::UnixListener) {
    let (stream, _) = listener.accept().await.unwrap();
    let mut framed = frame_stream(stream);
    let env = handshake(&mut framed).await;
    assert_eq!(env.method, "admin.unitstate");
    UnitStateListRequest::decode(env.payload.as_slice()).unwrap();

    for (name, json) in [
        ("a.service", &b"{\"active\":true}"[..]),
        ("b.service", &b"{}"[..]),
    ] {
        let entry = UnitStateEntry {
            name: name.to_string(),
            json: json.to_vec(),
        };
        send_envelope(
            &mut framed,
            &make_envelope(0, "system-a", "", "admin.unitstate.entry", entry).unwrap(),
        )
        .await
        .unwrap();
    }
    let eof = UnitStateEof {
        total: 2,
        message: String::new(),
    };
    send_envelope(
        &mut framed,
        &make_envelope(0, "system-a", "", "admin.unitstate.eof", eof).unwrap(),
    )
    .await
    .unwrap();
}

unsafe extern "C" fn collect(
    userdata: *mut c_void,
    name: *const c_char,
    json: *const u8,
    json_len: usize,
) -> SysaStatus {
    let sink = unsafe { &mut *(userdata as *mut Vec<(String, Vec<u8>)>) };
    let name = unsafe { CStr::from_ptr(name) }
        .to_string_lossy()
        .into_owned();
    let json = unsafe { std::slice::from_raw_parts(json, json_len) }.to_vec();
    sink.push((name, json));
    SysaStatus::Ok
}

#[test]
fn unitstate_admin_streams_entries() {
    let dir = socket_dir("unitstate");
    let path = dir.join("control.socket");
    let server = spawn_server(&path, serve_unitstate);

    let path_text = CString::new(path.to_str().unwrap()).unwrap();
    let admin: *mut SysaUnitStateAdmin = sysa_unitstate_admin_new(path_text.as_ptr());
    assert!(!admin.is_null());

    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    let mut total = 0u32;
    let status = sysa_unitstate_admin_list(
        admin,
        Some(collect),
        &mut entries as *mut _ as *mut c_void,
        &mut total,
    );
    assert_eq!(status, SysaStatus::Ok);
    assert_eq!(total, 2);
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].0, "a.service");
    assert_eq!(entries[0].1, b"{\"active\":true}".to_vec());
    assert_eq!(entries[1].0, "b.service");

    sysa_unitstate_admin_free(admin);
    server.join().unwrap();
}
