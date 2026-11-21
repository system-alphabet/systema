//! Worker lifecycle over the C surface, driven by a fake System A.

use std::ffi::{c_char, c_void, CStr, CString};
use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use prost::Message;
use systema_shared::controller::UnitStatus;
use systema_shared::ipc::{
    frame_stream, make_envelope, recv_envelope, send_envelope, EnvelopeFramed, IPC_VERSION,
    METHOD_VERSION, METHOD_VERSION_RESULT,
};
use systema_shared::proto::{
    IpcVersionRequest, IpcVersionResult, MethodCall, MethodResult, RegisterAck, UnitStateUpdate,
    UnitStateUpdateAck,
};
use systema_shared_capi::controller::SystemaController;
use systema_shared_capi::status::SystemaStatus;
use systema_shared_capi::unit_status::{
    systema_shared_unit_status_array_alloc, systema_shared_unit_status_free,
    systema_shared_unit_status_new, SystemaUnitStatus,
};
use systema_shared_capi::worker::{
    systema_shared_worker_free, systema_shared_worker_new,
    systema_shared_worker_publish_unit_states, systema_shared_worker_set_controller,
    systema_shared_worker_set_on_method_result, systema_shared_worker_set_socket_path,
    systema_shared_worker_start, systema_shared_worker_stop, systema_shared_worker_wait,
    SystemaWorker,
};

static START_CALLS: AtomicUsize = AtomicUsize::new(0);
static STOP_CALLS: AtomicUsize = AtomicUsize::new(0);
static METHOD_RESULTS: Mutex<Vec<String>> = Mutex::new(Vec::new());

type Log = Arc<Mutex<Vec<String>>>;

fn push(log: &Log, entry: impl Into<String>) {
    log.lock().unwrap().push(entry.into());
}

fn wait_for(log: &Log, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if log
            .lock()
            .unwrap()
            .iter()
            .any(|entry| entry.contains(needle))
        {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("{needle:?} never appeared in {:?}", log.lock().unwrap());
}

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
    _userdata: *mut c_void,
    _unit_name: *const c_char,
    out: *mut *mut SystemaUnitStatus,
) -> SystemaStatus {
    let status = demo_status();
    if status.is_null() {
        return SystemaStatus::Internal;
    }
    unsafe { *out = status };
    SystemaStatus::Ok
}

unsafe extern "C" fn start_cb(
    _userdata: *mut c_void,
    _unit_name: *const c_char,
    _config: *const u8,
    _config_len: usize,
    _invocation_id: *const c_char,
) -> SystemaStatus {
    START_CALLS.fetch_add(1, Ordering::SeqCst);
    SystemaStatus::Ok
}

unsafe extern "C" fn stop_cb(_userdata: *mut c_void, _unit_name: *const c_char) -> SystemaStatus {
    STOP_CALLS.fetch_add(1, Ordering::SeqCst);
    SystemaStatus::Ok
}

unsafe extern "C" fn reload_cb(
    _userdata: *mut c_void,
    _unit_name: *const c_char,
    _config: *const u8,
    _config_len: usize,
) -> SystemaStatus {
    SystemaStatus::Ok
}

unsafe extern "C" fn sync_state_cb(
    _userdata: *mut c_void,
    out: *mut *mut *mut SystemaUnitStatus,
    count: *mut usize,
) -> SystemaStatus {
    let mut array: *mut *mut SystemaUnitStatus = ptr::null_mut();
    if systema_shared_unit_status_array_alloc(1, &mut array) != SystemaStatus::Ok {
        return SystemaStatus::Internal;
    }
    let status = demo_status();
    if status.is_null() {
        return SystemaStatus::Internal;
    }
    unsafe {
        *array = status;
        *out = array;
        *count = 1;
    }
    SystemaStatus::Ok
}

unsafe extern "C" fn on_method_result(
    _userdata: *mut c_void,
    method: *const c_char,
    unit_name: *const c_char,
    success: u8,
    _unit_failure: u8,
    _error: *const c_char,
    queued: u8,
) {
    let method = unsafe { CStr::from_ptr(method) }
        .to_string_lossy()
        .into_owned();
    let unit_name = unsafe { CStr::from_ptr(unit_name) }
        .to_string_lossy()
        .into_owned();
    METHOD_RESULTS
        .lock()
        .unwrap()
        .push(format!("{method}:{unit_name}:{success}:{queued}"));
}

fn controller() -> SystemaController {
    SystemaController {
        userdata: ptr::null_mut(),
        status: Some(status_cb),
        start: Some(start_cb),
        stop: Some(stop_cb),
        restart: Some(start_cb),
        reload: Some(reload_cb),
        sync_state: Some(sync_state_cb),
    }
}

fn socket_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("systema-capi-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

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
                let update = UnitStateUpdate::decode(env.payload.as_slice()).unwrap();
                let names: Vec<String> = update
                    .units
                    .iter()
                    .map(|unit| unit.unit_name.clone())
                    .collect();
                push(
                    &log,
                    format!("state_update:{}:{}", names.join(","), update.full_snapshot),
                );
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
                push(&log, "ready");
                let call = MethodCall {
                    method: "start".to_string(),
                    unit_name: "demo.service".to_string(),
                    args: Vec::new(),
                    invocation_id: String::new(),
                };
                send_envelope(
                    &mut framed,
                    &make_envelope(7, "system-a", "worker", "method.call", call).unwrap(),
                )
                .await
                .unwrap();
            }
            "method.result" => {
                let result = MethodResult::decode(env.payload.as_slice()).unwrap();
                push(&log, format!("result:{}:{}", result.method, result.success));
                if result.success {
                    let status = UnitStatus::decode_from(&result.result).expect("unit status");
                    assert_eq!(status.unit_name, "demo.service");
                    assert_eq!(status.main_pid, 42);
                }
            }
            "worker.exit" => {
                push(&log, "exit");
                return;
            }
            other => push(&log, format!("unexpected:{other}")),
        }
    }
}

#[test]
fn start_requires_a_socket_and_a_controller() {
    let worker_id = CString::new("system-t-1").unwrap();
    let unit_type = CString::new("service").unwrap();
    let types = [unit_type.as_ptr()];
    let worker: *mut SystemaWorker =
        systema_shared_worker_new(worker_id.as_ptr(), types.as_ptr(), types.len());
    assert!(!worker.is_null());

    assert_eq!(
        systema_shared_worker_start(worker),
        SystemaStatus::InvalidArgument
    );

    let path = CString::new("/tmp/systema-capi-missing.sock").unwrap();
    assert_eq!(
        systema_shared_worker_set_socket_path(worker, path.as_ptr()),
        SystemaStatus::Ok
    );
    assert_eq!(
        systema_shared_worker_start(worker),
        SystemaStatus::InvalidArgument
    );

    assert_eq!(systema_shared_worker_free(worker), SystemaStatus::Ok);
}

#[test]
fn worker_runs_a_full_lifecycle() {
    let dir = socket_dir("worker");
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

    let worker_id = CString::new("system-t-1").unwrap();
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
    let table = controller();
    assert_eq!(
        systema_shared_worker_set_controller(worker, &table),
        SystemaStatus::Ok
    );
    assert_eq!(
        systema_shared_worker_set_on_method_result(worker, Some(on_method_result), ptr::null_mut()),
        SystemaStatus::Ok
    );
    assert_eq!(systema_shared_worker_start(worker), SystemaStatus::Ok);

    wait_for(&log, "ready");
    wait_for(&log, "result:start:true");

    let status = demo_status();
    assert!(!status.is_null());
    let units = [status as *const SystemaUnitStatus];
    assert_eq!(
        systema_shared_worker_publish_unit_states(worker, units.as_ptr(), units.len(), 0),
        SystemaStatus::Ok
    );
    wait_for(&log, "state_update:demo.service:false");
    systema_shared_unit_status_free(status);

    let reason = CString::new("SIGTERM").unwrap();
    assert_eq!(
        systema_shared_worker_stop(worker, reason.as_ptr()),
        SystemaStatus::Ok
    );
    let mut run = SystemaStatus::Internal;
    assert_eq!(
        systema_shared_worker_wait(worker, &mut run),
        SystemaStatus::Ok
    );
    assert_eq!(run, SystemaStatus::Ok);
    server.join().unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let results = METHOD_RESULTS.lock().unwrap().clone();
        if results
            .iter()
            .any(|entry| entry.starts_with("start:demo.service:1"))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no method-result hook call: {results:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(systema_shared_worker_free(worker), SystemaStatus::Ok);
    assert!(log.lock().unwrap().iter().any(|entry| entry == "exit"));
}

#[test]
fn freeing_a_running_worker_exits_gracefully() {
    let dir = socket_dir("worker-free");
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

    let worker_id = CString::new("system-t-2").unwrap();
    let unit_type = CString::new("service").unwrap();
    let types = [unit_type.as_ptr()];
    let worker: *mut SystemaWorker =
        systema_shared_worker_new(worker_id.as_ptr(), types.as_ptr(), types.len());
    let socket = CString::new(path.to_str().unwrap()).unwrap();
    systema_shared_worker_set_socket_path(worker, socket.as_ptr());
    let table = controller();
    systema_shared_worker_set_controller(worker, &table);
    assert_eq!(systema_shared_worker_start(worker), SystemaStatus::Ok);

    wait_for(&log, "ready");
    assert_eq!(systema_shared_worker_free(worker), SystemaStatus::Ok);
    server.join().unwrap();
    assert!(log.lock().unwrap().iter().any(|entry| entry == "exit"));
}
