//! Reentrancy: a callback must not deadlock on a blocking entry point, while
//! fire-and-forget work still succeeds from inside a callback.

use std::ffi::{c_char, c_void, CString};
use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use prost::Message;
use systema_shared::ipc::{
    frame_stream, make_envelope, recv_envelope, send_envelope, EnvelopeFramed, IPC_VERSION,
    METHOD_VERSION, METHOD_VERSION_RESULT,
};
use systema_shared::proto::{
    IpcVersionRequest, IpcVersionResult, MethodCall, RegisterAck, UnitStateUpdate,
    UnitStateUpdateAck,
};
use systema_shared_capi::controller::SystemaController;
use systema_shared_capi::finder::{
    systema_shared_finder_free, systema_shared_finder_new, systema_shared_finder_register_units,
};
use systema_shared_capi::status::SystemaStatus;
use systema_shared_capi::unit_status::{
    systema_shared_unit_status_free, systema_shared_unit_status_new, SystemaUnitStatus,
};
use systema_shared_capi::worker::{
    systema_shared_worker_free, systema_shared_worker_new,
    systema_shared_worker_publish_unit_states, systema_shared_worker_set_controller,
    systema_shared_worker_set_socket_path, systema_shared_worker_start, systema_shared_worker_stop,
    systema_shared_worker_wait, SystemaWorker,
};

static PUBLISH_STATUS: AtomicI32 = AtomicI32::new(-1);
static BLOCKING_STATUS: AtomicI32 = AtomicI32::new(-1);

fn demo_status() -> *mut SystemaUnitStatus {
    let name = CString::new("demo.service").unwrap();
    let active = CString::new("active").unwrap();
    let sub = CString::new("running").unwrap();
    let invocation = CString::new("inv-1").unwrap();
    systema_shared_unit_status_new(
        name.as_ptr(),
        active.as_ptr(),
        sub.as_ptr(),
        42,
        invocation.as_ptr(),
    )
}

unsafe extern "C" fn status_cb(
    userdata: *mut c_void,
    _unit_name: *const c_char,
    out: *mut *mut SystemaUnitStatus,
) -> SystemaStatus {
    let worker = userdata as *mut SystemaWorker;

    let status = demo_status();
    let units = [status as *const SystemaUnitStatus];
    let publish = systema_shared_worker_publish_unit_states(worker, units.as_ptr(), 1, 0);
    systema_shared_unit_status_free(status);
    PUBLISH_STATUS.store(publish as i32, Ordering::SeqCst);

    let path = CString::new("/tmp/systema-capi-reentrant.sock").unwrap();
    let finder = systema_shared_finder_new(path.as_ptr());
    let name = CString::new("area").unwrap();
    let mut bytes = ptr::null_mut();
    let mut len = 0usize;
    let blocking = systema_shared_finder_register_units(
        finder,
        name.as_ptr(),
        ptr::null(),
        0,
        &mut bytes,
        &mut len,
    );
    systema_shared_finder_free(finder);
    BLOCKING_STATUS.store(blocking as i32, Ordering::SeqCst);

    unsafe { *out = demo_status() };
    SystemaStatus::Ok
}

unsafe extern "C" fn ok_cb(_userdata: *mut c_void, _unit_name: *const c_char) -> SystemaStatus {
    SystemaStatus::Ok
}

unsafe extern "C" fn ok_config_cb(
    _userdata: *mut c_void,
    _unit_name: *const c_char,
    _config: *const u8,
    _config_len: usize,
) -> SystemaStatus {
    SystemaStatus::Ok
}

unsafe extern "C" fn ok_start_cb(
    _userdata: *mut c_void,
    _unit_name: *const c_char,
    _config: *const u8,
    _config_len: usize,
    _invocation_id: *const c_char,
) -> SystemaStatus {
    SystemaStatus::Ok
}

fn wait_for_value(slot: &AtomicI32, expected: i32) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if slot.load(Ordering::SeqCst) == expected {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!(
        "expected {expected} but observed {}",
        slot.load(Ordering::SeqCst)
    );
}

fn socket_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("systema-capi-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

type Log = Arc<Mutex<Vec<String>>>;

async fn handshake(framed: &mut EnvelopeFramed) {
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
}

async fn serve_worker(listener: tokio::net::UnixListener, log: Log) {
    let (stream, _) = listener.accept().await.unwrap();
    let mut framed = frame_stream(stream);
    handshake(&mut framed).await;

    let register = recv_envelope(&mut framed).await.unwrap().unwrap();
    assert_eq!(register.method, "worker.register");
    let ack = RegisterAck {
        accepted: true,
        message: "Welcome".to_string(),
    };
    send_envelope(
        &mut framed,
        &make_envelope(register.request_id, "system-a", "worker", "worker.ack", ack).unwrap(),
    )
    .await
    .unwrap();

    while let Some(env) = recv_envelope(&mut framed).await.unwrap() {
        match env.method.as_str() {
            "unit.state_update" => {
                UnitStateUpdate::decode(env.payload.as_slice()).unwrap();
                let ack = UnitStateUpdateAck {
                    accepted: true,
                    ignored_units: Vec::new(),
                    message: String::new(),
                };
                send_envelope(
                    &mut framed,
                    &make_envelope(
                        env.request_id,
                        "system-a",
                        "worker",
                        "unit.state_update_ack",
                        ack,
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
            }
            "worker.ready" => {
                log.lock().unwrap().push("ready".to_string());
                let call = MethodCall {
                    method: "status".to_string(),
                    unit_name: "demo.service".to_string(),
                    args: Vec::new(),
                    invocation_id: String::new(),
                };
                send_envelope(
                    &mut framed,
                    &make_envelope(9, "system-a", "worker", "method.call", call).unwrap(),
                )
                .await
                .unwrap();
            }
            "worker.exit" => return,
            _ => {}
        }
    }
}

fn wait_for_ready(log: &Log) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if log.lock().unwrap().iter().any(|entry| entry == "ready") {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("the worker never became ready");
}

#[test]
fn callbacks_can_publish_but_not_block() {
    let dir = socket_dir("reentrancy");
    let path = dir.join("allocator.sock");
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let listener = StdUnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let server_log = log.clone();
    let server = thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let listener = tokio::net::UnixListener::from_std(listener).unwrap();
            serve_worker(listener, server_log).await;
        });
    });

    let worker_id = CString::new("system-r-1").unwrap();
    let unit_type = CString::new("service").unwrap();
    let types = [unit_type.as_ptr()];
    let worker: *mut SystemaWorker =
        systema_shared_worker_new(worker_id.as_ptr(), types.as_ptr(), types.len());
    assert!(!worker.is_null());

    let socket = CString::new(path.to_str().unwrap()).unwrap();
    assert_eq!(
        systema_shared_worker_set_socket_path(worker, socket.as_ptr()),
        SystemaStatus::Ok
    );
    let table = SystemaController {
        userdata: worker as *mut c_void,
        status: Some(status_cb),
        start: Some(ok_start_cb),
        stop: Some(ok_cb),
        restart: Some(ok_start_cb),
        reload: Some(ok_config_cb),
        sync_state: None,
    };
    assert_eq!(
        systema_shared_worker_set_controller(worker, &table),
        SystemaStatus::Ok
    );
    assert_eq!(systema_shared_worker_start(worker), SystemaStatus::Ok);

    wait_for_ready(&log);
    wait_for_value(&PUBLISH_STATUS, SystemaStatus::Ok as i32);
    wait_for_value(&BLOCKING_STATUS, SystemaStatus::Reentrant as i32);

    let reason = CString::new("SIGTERM").unwrap();
    systema_shared_worker_stop(worker, reason.as_ptr());
    let mut run = SystemaStatus::Internal;
    assert_eq!(
        systema_shared_worker_wait(worker, &mut run),
        SystemaStatus::Ok
    );
    assert_eq!(run, SystemaStatus::Ok);
    server.join().unwrap();
    assert_eq!(systema_shared_worker_free(worker), SystemaStatus::Ok);
}
