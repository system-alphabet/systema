//! The control-port client against an in-process fake System A.

use std::ffi::{CStr, CString};
use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::{Arc, Mutex};
use std::thread;

use prost::Message;
use systema_shared::ipc::{
    frame_stream, make_envelope, recv_envelope, send_envelope, EnvelopeFramed, IPC_VERSION,
    METHOD_VERSION, METHOD_VERSION_RESULT,
};
use systema_shared::proto::{
    EnqueueJobRequest, EnqueueJobResult, EnqueueJobUnitResult, IpcVersionRequest, IpcVersionResult,
    JobEvent, ListJobsResult, ListSnapshotsResult, ManagerHelloRequest, ManagerHelloResult,
    SimpleManagerResult, UnitSnapshot,
};
use systema_shared_c::manager::{
    sysa_control_client_call, sysa_control_client_enqueue, sysa_control_client_flavor,
    sysa_control_client_free, sysa_control_client_list_snapshots, sysa_control_client_new,
    sysa_control_client_next_event, sysa_control_client_reload,
    sysa_control_client_reload_or_restart, sysa_control_client_restart, sysa_control_client_start,
    sysa_control_client_stop, sysa_control_client_try_restart, SysaControlClient,
    SysaControlEventKind,
};
use systema_shared_c::status::SysaStatus;
use systema_shared_c::sysa_bytes_free;

fn socket_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("systema-c-{tag}-{}", std::process::id()));
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

fn take_bytes(pointer: *mut u8, len: usize) -> Vec<u8> {
    let data = unsafe { std::slice::from_raw_parts(pointer, len) }.to_vec();
    sysa_bytes_free(pointer, len);
    data
}

async fn reply(framed: &mut EnvelopeFramed, request_id: u64, method: &str, payload: impl Message) {
    send_envelope(
        framed,
        &make_envelope(request_id, "system-a", "", method, payload).unwrap(),
    )
    .await
    .unwrap();
}

async fn serve_manager(listener: tokio::net::UnixListener) {
    let (stream, _) = listener.accept().await.unwrap();
    let mut framed = frame_stream(stream);

    let first = recv_envelope(&mut framed)
        .await
        .unwrap()
        .expect("a version request arrives first");
    assert_eq!(first.method, METHOD_VERSION);
    let request = IpcVersionRequest::decode(first.payload.as_slice()).unwrap();
    assert_eq!(request.version, IPC_VERSION);
    reply(
        &mut framed,
        first.request_id,
        METHOD_VERSION_RESULT,
        IpcVersionResult {
            version: IPC_VERSION,
            compatible: true,
            message: String::new(),
        },
    )
    .await;

    while let Some(env) = recv_envelope(&mut framed).await.unwrap() {
        match env.method.as_str() {
            "manager.hello" => {
                let hello = ManagerHelloRequest::decode(env.payload.as_slice()).unwrap();
                assert_eq!(hello.worker_id, "system-w-1");
                reply(
                    &mut framed,
                    env.request_id,
                    "manager.hello.result",
                    ManagerHelloResult {
                        success: true,
                        message: "ok".to_string(),
                    },
                )
                .await;
            }
            "manager.enqueue" => {
                let request = EnqueueJobRequest::decode(env.payload.as_slice()).unwrap();
                assert_eq!(request.names, ["sshd.service", "nginx.service"]);
                assert_eq!(request.job_type, "start");
                assert_eq!(request.mode, "replace");
                let results: Vec<_> = request
                    .names
                    .iter()
                    .enumerate()
                    .map(|(index, name)| EnqueueJobUnitResult {
                        success: true,
                        message: String::new(),
                        job_id: 7 + index as u64,
                        unit_name: name.clone(),
                    })
                    .collect();
                reply(
                    &mut framed,
                    env.request_id,
                    "manager.enqueue.result",
                    EnqueueJobResult {
                        success: true,
                        message: String::new(),
                        job_id: 7,
                        unit_name: request.names[0].clone(),
                        results,
                    },
                )
                .await;
                for (method, result) in [("job.new", ""), ("job.completed", "done")] {
                    reply(
                        &mut framed,
                        0,
                        method,
                        JobEvent {
                            job_id: 7,
                            unit_name: request.names[0].clone(),
                            result: result.to_string(),
                        },
                    )
                    .await;
                }
            }
            "manager.list_snapshots" => {
                reply(
                    &mut framed,
                    env.request_id,
                    "manager.list_snapshots.result",
                    ListSnapshotsResult {
                        success: true,
                        message: String::new(),
                        units: vec![UnitSnapshot {
                            name: "sshd.service".to_string(),
                            kind: "service".to_string(),
                            active_state: "active".to_string(),
                            ..Default::default()
                        }],
                    },
                )
                .await;
            }
            "manager.list_jobs" => {
                reply(
                    &mut framed,
                    env.request_id,
                    "manager.list_jobs.result",
                    ListJobsResult {
                        success: true,
                        message: String::new(),
                        jobs: Vec::new(),
                    },
                )
                .await;
                // Last request in the script: close our side so the client
                // reader observes EOF and the test can join this thread.
                return;
            }
            other => {
                reply(
                    &mut framed,
                    env.request_id,
                    "manager.error",
                    SimpleManagerResult {
                        success: false,
                        message: format!("no handler for {other}"),
                    },
                )
                .await;
            }
        }
    }
}

#[test]
fn control_client_round_trips_and_streams_events() {
    let dir = socket_dir("manager");
    let path = dir.join("control.socket");
    let server = spawn_server(&path, serve_manager);

    let path_text = CString::new(path.to_str().unwrap()).unwrap();
    let flavor = CString::new("systemd").unwrap();
    let worker = CString::new("system-w-1").unwrap();
    let mut client: *mut SysaControlClient = ptr::null_mut();
    assert_eq!(
        sysa_control_client_new(
            path_text.as_ptr(),
            flavor.as_ptr(),
            worker.as_ptr(),
            &mut client
        ),
        SysaStatus::Ok
    );
    assert!(!client.is_null());

    let reported = unsafe { CStr::from_ptr(sysa_control_client_flavor(client)) };
    assert_eq!(reported.to_str().unwrap(), "systemd");

    let name = CString::new("sshd.service").unwrap();
    let second_name = CString::new("nginx.service").unwrap();
    let names = [name.as_ptr(), second_name.as_ptr()];
    let job_type = CString::new("start").unwrap();
    let mode = CString::new("replace").unwrap();
    let mut out = ptr::null_mut();
    let mut len = 0usize;
    assert_eq!(
        sysa_control_client_enqueue(
            client,
            names.as_ptr(),
            names.len(),
            job_type.as_ptr(),
            mode.as_ptr(),
            0,
            &mut out,
            &mut len,
        ),
        SysaStatus::Ok
    );
    let result = EnqueueJobResult::decode(take_bytes(out, len).as_slice()).unwrap();
    assert!(result.success);
    assert_eq!(result.results.len(), 2);

    let mut kind = SysaControlEventKind::UnitChanged;
    let mut out = ptr::null_mut();
    let mut len = 0usize;
    assert_eq!(
        sysa_control_client_next_event(client, 5_000, &mut kind, &mut out, &mut len),
        SysaStatus::Ok
    );
    assert_eq!(kind, SysaControlEventKind::JobNew);
    let _ = take_bytes(out, len);

    let mut kind = SysaControlEventKind::UnitChanged;
    let mut out = ptr::null_mut();
    let mut len = 0usize;
    assert_eq!(
        sysa_control_client_next_event(client, 5_000, &mut kind, &mut out, &mut len),
        SysaStatus::Ok
    );
    assert_eq!(kind, SysaControlEventKind::JobCompleted);
    let _ = JobEvent::decode(take_bytes(out, len).as_slice()).unwrap();

    let mut out = ptr::null_mut();
    let mut len = 0usize;
    assert_eq!(
        sysa_control_client_list_snapshots(client, &mut out, &mut len),
        SysaStatus::Ok
    );
    let snapshots = ListSnapshotsResult::decode(take_bytes(out, len).as_slice()).unwrap();
    assert_eq!(snapshots.units.len(), 1);
    assert_eq!(snapshots.units[0].name, "sshd.service");

    // With no further events, a bounded wait reports a timeout.
    let mut kind = SysaControlEventKind::UnitChanged;
    let mut out = ptr::null_mut();
    let mut len = 0usize;
    assert_eq!(
        sysa_control_client_next_event(client, 50, &mut kind, &mut out, &mut len),
        SysaStatus::Timeout
    );

    // The generic entry point carries an arbitrary request.  The fake server
    // exits after answering this one, so the client reader sees EOF.
    let method = CString::new("manager.list_jobs").unwrap();
    let mut out = ptr::null_mut();
    let mut len = 0usize;
    assert_eq!(
        sysa_control_client_call(client, method.as_ptr(), ptr::null(), 0, &mut out, &mut len,),
        SysaStatus::Ok
    );
    let jobs = ListJobsResult::decode(take_bytes(out, len).as_slice()).unwrap();
    assert!(jobs.jobs.is_empty());

    sysa_control_client_free(client);
    server.join().unwrap();
}

#[test]
fn convenience_methods_send_expected_job_types() {
    let dir = socket_dir("manager-jobs");
    let path = dir.join("control.socket");
    let recorded = Arc::new(Mutex::new(Vec::<EnqueueJobRequest>::new()));
    let server_recorded = Arc::clone(&recorded);

    let server = spawn_server(&path, move |listener| async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut framed = frame_stream(stream);

        let first = recv_envelope(&mut framed)
            .await
            .unwrap()
            .expect("a version request arrives first");
        assert_eq!(first.method, METHOD_VERSION);
        reply(
            &mut framed,
            first.request_id,
            METHOD_VERSION_RESULT,
            IpcVersionResult {
                version: IPC_VERSION,
                compatible: true,
                message: String::new(),
            },
        )
        .await;

        while let Some(env) = recv_envelope(&mut framed).await.unwrap() {
            match env.method.as_str() {
                "manager.hello" => {
                    reply(
                        &mut framed,
                        env.request_id,
                        "manager.hello.result",
                        ManagerHelloResult {
                            success: true,
                            message: "ok".to_string(),
                        },
                    )
                    .await;
                }
                "manager.enqueue" => {
                    let request = EnqueueJobRequest::decode(env.payload.as_slice()).unwrap();
                    let finished = {
                        let mut jobs = server_recorded.lock().unwrap();
                        jobs.push(request.clone());
                        jobs.len() == 6
                    };
                    let results = request
                        .names
                        .iter()
                        .map(|name| EnqueueJobUnitResult {
                            success: true,
                            message: String::new(),
                            job_id: 1,
                            unit_name: name.clone(),
                        })
                        .collect();
                    reply(
                        &mut framed,
                        env.request_id,
                        "manager.enqueue.result",
                        EnqueueJobResult {
                            success: true,
                            message: String::new(),
                            job_id: 1,
                            unit_name: request.names[0].clone(),
                            results,
                        },
                    )
                    .await;
                    if finished {
                        return;
                    }
                }
                other => panic!("unexpected request {other}"),
            }
        }
    });

    let path_text = CString::new(path.to_str().unwrap()).unwrap();
    let flavor = CString::new("systemd").unwrap();
    let mut client: *mut SysaControlClient = ptr::null_mut();
    assert_eq!(
        sysa_control_client_new(
            path_text.as_ptr(),
            flavor.as_ptr(),
            ptr::null(),
            &mut client
        ),
        SysaStatus::Ok
    );

    let name = CString::new("sshd.service").unwrap();
    let names = [name.as_ptr()];
    let run = |call: extern "C" fn(
        *const SysaControlClient,
        *const *const std::ffi::c_char,
        usize,
        *mut *mut u8,
        *mut usize,
    ) -> SysaStatus| {
        let mut out = ptr::null_mut();
        let mut len = 0usize;
        assert_eq!(
            call(client, names.as_ptr(), names.len(), &mut out, &mut len),
            SysaStatus::Ok
        );
        let reply = EnqueueJobResult::decode(take_bytes(out, len).as_slice()).unwrap();
        assert!(reply.success);
    };

    run(sysa_control_client_start);
    run(sysa_control_client_stop);
    run(sysa_control_client_restart);
    run(sysa_control_client_reload);
    run(sysa_control_client_try_restart);
    run(sysa_control_client_reload_or_restart);

    sysa_control_client_free(client);
    server.join().unwrap();

    let jobs = recorded.lock().unwrap();
    let observed: Vec<(&str, bool)> = jobs
        .iter()
        .map(|job| (job.job_type.as_str(), job.reload_if_possible))
        .collect();
    assert_eq!(
        observed,
        vec![
            ("start", false),
            ("stop", false),
            ("restart", false),
            ("reload", false),
            ("try-restart", false),
            ("reload-or-restart", true),
        ]
    );
    assert!(jobs.iter().all(|job| job.mode == "replace"));
    assert!(jobs.iter().all(|job| job.names == ["sshd.service"]));
}
