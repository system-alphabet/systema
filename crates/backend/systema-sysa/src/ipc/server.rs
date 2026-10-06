//! IPC server for System A.
//!
//! Listens on a Unix socket at `SOCKET_PATH`. Each System Worker connects,
//! sends a `WorkerRegistration`, then receives dispatched `method.call`
//! envelopes and sends back `method.result` / `event.publish` envelopes.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use anyhow::{Context, Result};
use parking_lot::Mutex;
use prost::Message as ProstMessage;
use tokio::net::UnixListener;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use sysa::controller::UnitStatus;
use sysa::event_bus::Event;
use sysa::ipc::{frame_stream, make_envelope, recv_envelope, send_envelope};
use sysa::proto::{
    AdminStagingOp, AdminStagingResult, CgroupMetricsUpdate, CommitUnits, Envelope, EventSubscribe,
    EventUnsubscribe, MethodResult, PathFired, RegisterAck, RegisterUnits, StagingAreaEntry,
    StagingQuery, StagingQueryResult, TimerFired, UnitDefineResult, UnitRegistrationAck,
    UnitStateUpdate, UnitStateUpdateAck, UnitSyncReport, WorkerExit, WorkerRegistration,
};

use crate::events::load_unit_sync;
use crate::event::{replay_active_units, WorkerEventForwarder};
use crate::state::{next_request_id, AllocatorHandle, CachedUnitState, WorkerEntry};

/// Shared fdpass channel map: worker_id → UnixStream (for SCM_RIGHTS).
pub type FdPassMap = Arc<Mutex<HashMap<String, Arc<UnixStream>>>>;

/// Run the IPC server — accepts System Worker & Finder connections indefinitely.
pub async fn run(allocator: AllocatorHandle) -> Result<()> {
    if let Some(parent) = std::path::Path::new(sysa::paths::instance().ipc_socket_path).parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    if let Some(parent) = std::path::Path::new(sysa::paths::instance().systema_fdpass_sock).parent()
    {
        tokio::fs::create_dir_all(parent).await?;
    }

    if tokio::net::UnixStream::connect(sysa::paths::instance().ipc_socket_path)
        .await
        .is_ok()
    {
        warn!(
            "Another allocator is already listening on {}. Exiting.",
            sysa::paths::instance().ipc_socket_path
        );
        return Ok(());
    }

    let _ = tokio::fs::remove_file(sysa::paths::instance().ipc_socket_path).await;
    let _ = tokio::fs::remove_file(sysa::paths::instance().systema_fdpass_sock).await;

    let listener = UnixListener::bind(sysa::paths::instance().ipc_socket_path)?;
    info!(
        "IPC server listening on {}",
        sysa::paths::instance().ipc_socket_path
    );

    let fdpass_listener = UnixListener::bind(sysa::paths::instance().systema_fdpass_sock)?;
    info!(
        "FD-Pass server listening on {}",
        sysa::paths::instance().systema_fdpass_sock
    );

    // IPC channels are up — announce allocator readiness on the notify
    // channel so SysAInit can proceed past the first startup phase.
    sysa::notify::broadcast(&[("MANAGER_READY", "1"), ("STATUS", "ipc-ready")]);

    let fdpass_map: FdPassMap = Arc::new(Mutex::new(HashMap::new()));
    // Queue of workers waiting for a listener fd: requesters in FIFO order.
    // A service worker pushes itself before asking the socket worker for an
    // fd; when the fd arrives over a socket-worker fdpass channel, it is
    // forwarded to the front of this queue.
    let pending_fd: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));

    let fpm = fdpass_map.clone();
    let pf = pending_fd.clone();
    tokio::spawn(async move {
        if let Err(e) = run_fdpass_acceptor(fdpass_listener, fpm, pf).await {
            error!("FD-Pass acceptor error: {}", e);
        }
    });

    // Control-port bus (one-to-many): System Wrapper bridge flavors and
    // control-plane tooling connect here.  Independent of the workload
    // plane on `allocator.sock`.
    let control_alloc = allocator.clone();
    tokio::spawn(async move {
        if let Err(e) = crate::ipc::control::run_control_listener(control_alloc).await {
            error!("Control-port bus error: {e}");
        }
    });

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let alloc = allocator.clone();
                let pf = pending_fd.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_worker(stream, alloc, pf).await {
                        error!("Worker connection error: {}", e);
                    }
                });
            }
            Err(e) => {
                error!("Accept error: {}", e);
            }
        }
    }
}

async fn run_fdpass_acceptor(
    listener: UnixListener,
    fdpass_map: FdPassMap,
    pending_fd: Arc<Mutex<VecDeque<String>>>,
) -> Result<()> {
    use tokio::io::AsyncReadExt;

    loop {
        let (stream, _) = listener.accept().await?;
        let fpm = fdpass_map.clone();
        let pending = pending_fd.clone();
        tokio::spawn(async move {
            let mut stream = stream;
            let mut buf = [0u8; 128];
            let n = match stream.read(&mut buf).await {
                Ok(0) => return,
                Ok(n) => n,
                Err(e) => {
                    warn!("fdpass read error: {}", e);
                    return;
                }
            };
            let worker_id = String::from_utf8_lossy(&buf[..n]).trim().to_string();
            if worker_id.is_empty() {
                warn!("fdpass connection with empty worker_id");
                return;
            }
            let shared = Arc::new(stream);
            fpm.lock().insert(worker_id.clone(), shared.clone());
            info!("fdpass channel registered for '{}'", worker_id);

            // Receive listener fds sent by socket workers (SCM_RIGHTS) and
            // forward each one to the worker at the front of the pending
            // request queue (the service worker that asked for it).
            loop {
                match sysa::ipc::recv_fd(shared.as_ref()).await {
                    Ok(fd) => {
                        let requester = pending.lock().pop_front();
                        match requester {
                            Some(requester) => {
                                let dest = fpm.lock().get(&requester).cloned();
                                if let Some(dest) = dest {
                                    if let Err(e) = sysa::ipc::send_fd(dest.as_ref(), fd).await {
                                        warn!(
                                            "fdpass: forwarding fd {} to '{}' failed: {}",
                                            fd, requester, e
                                        );
                                    } else {
                                        info!("fdpass: forwarded fd {} to '{}'", fd, requester);
                                    }
                                    // Our copy of the received fd is no longer
                                    // needed (the receiver got its own).
                                    unsafe {
                                        libc::close(fd);
                                    }
                                } else {
                                    warn!(
                                        "fdpass: requester '{}' has no fdpass channel; closing fd {}",
                                        requester, fd
                                    );
                                    unsafe {
                                        libc::close(fd);
                                    }
                                }
                            }
                            None => {
                                warn!(
                                    "fdpass: received fd {} with no pending request; closing",
                                    fd
                                );
                                unsafe {
                                    libc::close(fd);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        debug!("fdpass: recv error on '{}' channel: {}", worker_id, e);
                        break;
                    }
                }
            }
        });
    }
}

/// Extract the peer PID and UID from a Unix stream.
///
/// Linux exposes the full `SO_PEERCRED` struct (PID + UID).  The BSDs share
/// `getpeereid`, which only reports the effective UID/GID — the PID is lost,
/// so it is reported as 0.
pub(crate) fn peer_cred(stream: &UnixStream) -> Result<(u32, u32)> {
    use std::os::unix::io::AsRawFd;
    let fd = stream.as_raw_fd();

    // Linux: struct ucred { pid, uid, gid } via SO_PEERCRED.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        unsafe {
            let mut cred: libc::ucred = std::mem::zeroed();
            let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
            let ret = libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                &mut cred as *mut _ as *mut libc::c_void,
                &mut len,
            );
            if ret < 0 {
                let e = std::io::Error::last_os_error();
                anyhow::bail!(sysa::l10n::fmt(
                    sysa::l10n::t_("SO_PEERCRED failed: {e}"),
                    &[("e", &e.to_string())]
                ));
            }
            Ok((cred.pid as u32, cred.uid as u32))
        }
    }

    // BSDs / macOS: getpeereid(fd, &euid, &egid).
    #[cfg(any(
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "dragonfly",
        target_os = "macos",
        target_os = "netbsd"
    ))]
    {
        let mut euid: libc::uid_t = 0;
        let mut egid: libc::gid_t = 0;
        let ret = unsafe { libc::getpeereid(fd, &mut euid, &mut egid) };
        if ret < 0 {
            let e = std::io::Error::last_os_error();
            anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_("getpeereid failed: {e}"),
                &[("e", &e.to_string())]
            ));
        }
        Ok((0, euid as u32))
    }
}

/// The system UID (the UID running the allocator).
pub(crate) fn system_uid() -> u32 {
    unsafe { libc::getuid() }
}

async fn handle_worker(
    stream: tokio::net::UnixStream,
    allocator: AllocatorHandle,
    pending_fd: Arc<Mutex<VecDeque<String>>>,
) -> Result<()> {
    let (_client_pid, client_uid) =
        peer_cred(&stream).context(sysa::l10n::t_("failed to get peer credentials"))?;
    let mut framed = frame_stream(stream);

    let env = recv_envelope(&mut framed).await?.ok_or_else(|| {
        anyhow::anyhow!(sysa::l10n::t_("Client disconnected before registration"))
    })?;

    match env.method.as_str() {
        "worker.register" => handle_worker_session(framed, env, allocator, pending_fd).await,
        "finder.register_units" => handle_finder_register(framed, env, allocator, client_uid).await,
        "finder.commit_units" => handle_finder_commit(framed, env, allocator, client_uid).await,
        "staging.query" => handle_finder_query(framed, env, allocator, client_uid).await,
        other => {
            anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_("Expected 'worker.register', 'finder.register_units', 'finder.commit_units', or 'staging.query', got '{method}'"),
                &[("method", other)],
            ))
        }
    }
}

async fn handle_worker_session(
    mut framed: sysa::ipc::EnvelopeFramed,
    env: Envelope,
    allocator: AllocatorHandle,
    pending_fd: Arc<Mutex<VecDeque<String>>>,
) -> Result<()> {
    let reg = WorkerRegistration::decode(env.payload.as_slice())?;
    let worker_id = reg.worker_id.clone();
    let unit_types = reg.unit_types.clone();

    info!(
        "Worker '{}' registered, handles: {:?}",
        worker_id, unit_types
    );

    // Send RegisterAck.
    let ack = RegisterAck {
        accepted: true,
        message: "Welcome".to_string(),
    };
    let ack_env = make_envelope(next_request_id(), "system-a", &worker_id, "worker.ack", ack)?;
    send_envelope(&mut framed, &ack_env).await?;

    // Create the envelope channel for this worker.
    let (envelope_tx, mut envelope_rx) = mpsc::channel::<bytes::Bytes>(64);
    // Clone kept for the WorkerEventForwarder, which shares the worker's
    // outgoing envelope channel.
    let forward_tx = envelope_tx.clone();

    // Register the worker.
    {
        let mut state = allocator.write();
        state.workers.insert(
            worker_id.clone(),
            WorkerEntry {
                worker_id: worker_id.clone(),
                unit_types: unit_types.clone(),
                supports_unit_define: reg.supports_unit_define,
                ready: false,
                envelope_tx,
            },
        );
    }

    let inner = framed.into_inner();
    let (reader, writer) = tokio::io::split(inner);
    let writer_stream = {
        use tokio_util::codec::LengthDelimitedCodec;
        let codec = LengthDelimitedCodec::builder()
            .max_frame_length(16 * 1024 * 1024)
            .new_codec();
        tokio_util::codec::FramedWrite::new(writer, codec)
    };
    let reader_stream = {
        use tokio_util::codec::LengthDelimitedCodec;
        let codec = LengthDelimitedCodec::builder()
            .max_frame_length(16 * 1024 * 1024)
            .new_codec();
        tokio_util::codec::FramedRead::new(reader, codec)
    };

    let _alloc_for_send = allocator.clone();
    let alloc_for_recv = allocator.clone();
    let worker_id_recv = worker_id.clone();
    let worker_id_send = worker_id.clone();
    let pending_fd_recv = pending_fd.clone();

    // Sender task: reads pre-encoded envelopes from envelope_rx.
    let sender = async move {
        use futures::SinkExt;
        let mut writer = writer_stream;
        while let Some(bytes) = envelope_rx.recv().await {
            if let Err(e) = writer.send(bytes).await {
                warn!("Sender error for worker '{}': {}", worker_id_send, e);
                break;
            }
        }
        Ok::<_, anyhow::Error>(())
    };

    // Receiver task: reads method.result and event.publish from socket.
    let receiver = async move {
        use futures::StreamExt;
        let mut reader = reader_stream;

        // Per-connection subscription to unit events.  `sub_id` is the
        // current EventBus subscriber ID (None when not subscribed).
        let mut subscription = WorkerSubscription::default();
        let mut sub_id: Option<u64> = None;

        loop {
            match reader.next().await {
                None => {
                    info!("Worker '{}' disconnected", worker_id_recv);
                    break;
                }
                Some(Err(e)) => {
                    warn!("Worker '{}' read error: {}", worker_id_recv, e);
                    break;
                }
                Some(Ok(bytes)) => {
                    let env = match Envelope::decode(bytes.freeze()) {
                        Ok(e) => e,
                        Err(e) => {
                            warn!(
                                "Failed to decode envelope from worker '{}': {} — disconnecting",
                                worker_id_recv, e
                            );
                            break;
                        }
                    };
                    debug!(
                        "IPC envelope received from worker '{}': method={} source={} target={} payload_len={}",
                        worker_id_recv, env.method, env.source, env.target, env.payload.len()
                    );

                    if env.method == "method.result" {
                        let qid = env.request_id;
                        let is_task = alloc_for_recv.read().task_kinds.contains_key(&qid);
                        if is_task {
                            let result = match MethodResult::decode(env.payload.as_slice()) {
                                Ok(r) => r,
                                Err(e) => {
                                    warn!(
                                        "Failed to decode MethodResult from worker '{}': {} — disconnecting",
                                        worker_id_recv, e
                                    );
                                    break;
                                }
                            };
                            use crate::scheduler::handle_task_result;
                            let kind = alloc_for_recv
                                .read()
                                .task_kinds
                                .get(&qid)
                                .copied()
                                .unwrap_or(crate::state::JobKind::Start);
                            handle_task_result(
                                alloc_for_recv.clone(),
                                qid,
                                result.success,
                                &result.error,
                                &result.unit_name,
                                kind,
                            );
                            // After the job is updated, check whether this
                            // unit's Start/Restart job ended in Failed (e.g.
                            // via dependency failure in `fail_dependents`).
                            // If so, the unit did NOT successfully start and
                            // the cache must reflect "inactive" regardless of
                            // what the worker reports — the worker's
                            // "active" status is stale (the worker started
                            // the resource but the job-level transaction
                            // failed).
                            {
                                let mut state = alloc_for_recv.write();
                                if unit_has_failed_start_job(&state, &result.unit_name) {
                                    revert_cache_to_inactive(&mut state, &result.unit_name);
                                } else if !result.result.is_empty() {
                                    if let Some(unit_status) =
                                        UnitStatus::decode_from(&result.result)
                                    {
                                        let entry = state
                                            .unit_states
                                            .entry(result.unit_name.clone())
                                            .or_default();
                                        apply_state_to_cache(entry, &unit_status);
                                    }
                                } else {
                                    drop(state);
                                    update_cache_on_task_result(
                                        &alloc_for_recv,
                                        &result.unit_name,
                                        kind,
                                        result.success,
                                    );
                                }
                            }
                        } else {
                            warn!(
                                "method.result from worker '{}' with unknown request_id {} — ignoring",
                                worker_id_recv, qid
                            );
                        }
                        continue;
                    }

                    // A service worker asks for the listener fd of a socket
                    // unit (socket activation). Forward the request to the
                    // socket worker and remember the requester: the fd that
                    // arrives over the socket worker's fdpass channel will be
                    // forwarded to it (FIFO).
                    if env.method == "socket.request_fd" {
                        let unit_name = String::from_utf8_lossy(&env.payload).to_string();
                        let requester = worker_id_recv.clone();
                        if unit_name.is_empty() {
                            warn!(
                                "socket.request_fd from '{}' with empty unit name",
                                requester
                            );
                            continue;
                        }
                        let target = {
                            let state = alloc_for_recv.read();
                            state
                                .workers
                                .values()
                                .find(|w| w.unit_types.iter().any(|t| t == "socket"))
                                .map(|w| w.worker_id.clone())
                        };
                        let Some(sock_wid) = target else {
                            warn!(
                                "socket.request_fd for '{}' from '{}' but no socket worker registered",
                                unit_name, requester
                            );
                            continue;
                        };
                        pending_fd_recv.lock().push_back(requester.clone());
                        let mut buf = bytes::BytesMut::new();
                        let fwd = Envelope {
                            request_id: next_request_id(),
                            source: "system-a".to_string(),
                            target: sock_wid.clone(),
                            method: "socket.request_fd".to_string(),
                            payload: env.payload.clone(),
                        };
                        if let Err(e) = fwd.encode(&mut buf) {
                            warn!(
                                "Failed to encode socket.request_fd forward for '{}': {}",
                                unit_name, e
                            );
                            pending_fd_recv.lock().pop_back();
                            continue;
                        }
                        let sock_tx = {
                            let state = alloc_for_recv.read();
                            state.workers.get(&sock_wid).map(|w| w.envelope_tx.clone())
                        };
                        let delivered = match sock_tx {
                            Some(tx) => tx.send(buf.freeze()).await.is_ok(),
                            None => false,
                        };
                        if !delivered {
                            warn!(
                                "Failed to deliver socket.request_fd for '{}' to '{}'",
                                unit_name, sock_wid
                            );
                            pending_fd_recv.lock().pop_back();
                            continue;
                        }
                        info!(
                            "Forwarded socket.request_fd for '{}' from '{}' to '{}'",
                            unit_name, requester, sock_wid
                        );
                        continue;
                    }

                    if env.method == "unit.define_result" {
                        let qid = env.request_id;
                        let result = match UnitDefineResult::decode(env.payload.as_slice()) {
                            Ok(r) => r,
                            Err(e) => UnitDefineResult {
                                success: false,
                                error: sysa::l10n::fmt(
                                    sysa::l10n::t_("decode failed: {e}"),
                                    &[("e", &e.to_string())],
                                ),
                                units_json: vec![],
                            },
                        };
                        let mut state = alloc_for_recv.write();
                        if let Some(tx) = state.unit_define_txs.remove(&qid) {
                            let _ = tx.send(result);
                        } else {
                            drop(state);
                            debug!(
                                "unit.define_result from worker '{}' with unknown request_id {} — ignoring",
                                worker_id_recv, qid
                            );
                        }
                        continue;
                    }

                    if env.method == "unit.state_update" {
                        let update = match UnitStateUpdate::decode(env.payload.as_slice()) {
                            Ok(u) => u,
                            Err(e) => {
                                warn!(
                                    "UnitStateUpdate decode from worker '{}': {} — disconnecting",
                                    worker_id_recv, e
                                );
                                break;
                            }
                        };
                        let worker_id_evt = worker_id_recv.clone();
                        handle_state_update(
                            alloc_for_recv.clone(),
                            &worker_id_evt,
                            env.request_id,
                            update,
                        )
                        .await;
                        continue;
                    }

                    if env.method == "unit.sync_report" {
                        let report = match UnitSyncReport::decode(env.payload.as_slice()) {
                            Ok(r) => r,
                            Err(e) => {
                                warn!(
                                    "UnitSyncReport decode from worker '{}': {} — disconnecting",
                                    worker_id_recv, e
                                );
                                break;
                            }
                        };
                        let worker_id_evt = worker_id_recv.clone();
                        match report.snapshot {
                            Some(snapshot) => {
                                handle_state_update(
                                    alloc_for_recv.clone(),
                                    &worker_id_evt,
                                    env.request_id,
                                    snapshot,
                                )
                                .await
                            }
                            None => warn!(
                                "Worker '{}' replied to unit.sync_request without a snapshot",
                                worker_id_evt
                            ),
                        }
                        continue;
                    }

                    if env.method == "timer.fired" {
                        let fired = match TimerFired::decode(env.payload.as_slice()) {
                            Ok(f) => f,
                            Err(e) => {
                                warn!(
                                    "TimerFired decode from worker '{}': {} — disconnecting",
                                    worker_id_recv, e
                                );
                                break;
                            }
                        };
                        let target = fired.target_unit.clone();
                        info!(
                            "Timer '{}' fired (elapse={}) — triggering '{}'",
                            fired.timer_unit, fired.elapse_epoch, target
                        );

                        // Ensure the target unit is loaded before enqueueing.
                        if !alloc_for_recv.read().units.contains_key(&target) {
                            let alloc2 = alloc_for_recv.clone();
                            let name2 = target.clone();
                            let loaded = tokio::task::spawn_blocking(move || {
                                load_unit_sync(&alloc2, &name2)
                            })
                            .await;
                            match loaded {
                                Ok(Ok(())) => {}
                                Ok(Err(e)) => {
                                    warn!(
                                        "Failed to load timer-triggered unit '{}': {}",
                                        target, e
                                    );
                                    continue;
                                }
                                Err(e) => {
                                    warn!(
                                        "Failed to load timer-triggered unit '{}': {}",
                                        target, e
                                    );
                                    continue;
                                }
                            }
                        }

                        match crate::scheduler::enqueue_start_with_mode(
                            alloc_for_recv.clone(),
                            &target,
                            crate::state::JobMode::Replace,
                        )
                        .await
                        {
                            Ok(job_id) => {
                                info!("Enqueued job {job_id} for timer-triggered '{target}'");
                            }
                            Err(e) => {
                                warn!("Cannot start timer-triggered unit '{}': {}", target, e);
                            }
                        }
                        continue;
                    }

                    if env.method == "path.fired" {
                        let fired = match PathFired::decode(env.payload.as_slice()) {
                            Ok(f) => f,
                            Err(e) => {
                                warn!(
                                    "PathFired decode from worker '{}': {} — disconnecting",
                                    worker_id_recv, e
                                );
                                break;
                            }
                        };
                        let target = fired.target_unit.clone();
                        info!(
                            "Path '{}' fired (epoch={}) — triggering '{}'",
                            fired.path_unit, fired.fired_epoch, target
                        );

                        // Ensure the target unit is loaded before enqueueing.
                        if !alloc_for_recv.read().units.contains_key(&target) {
                            let alloc2 = alloc_for_recv.clone();
                            let name2 = target.clone();
                            let loaded = tokio::task::spawn_blocking(move || {
                                load_unit_sync(&alloc2, &name2)
                            })
                            .await;
                            match loaded {
                                Ok(Ok(())) => {}
                                Ok(Err(e)) => {
                                    warn!("Failed to load path-triggered unit '{}': {}", target, e);
                                    continue;
                                }
                                Err(e) => {
                                    warn!("Failed to load path-triggered unit '{}': {}", target, e);
                                    continue;
                                }
                            }
                        }

                        match crate::scheduler::enqueue_start_with_mode(
                            alloc_for_recv.clone(),
                            &target,
                            crate::state::JobMode::Replace,
                        )
                        .await
                        {
                            Ok(job_id) => {
                                info!("Enqueued job {job_id} for path-triggered '{target}'");
                            }
                            Err(e) => {
                                warn!("Cannot start path-triggered unit '{}': {}", target, e);
                            }
                        }
                        continue;
                    }

                    if env.method == "socket.fired" {
                        // Payload is the raw socket unit name (UTF-8), the
                        // same convention as `socket.request_fd`.
                        let socket_unit = String::from_utf8_lossy(&env.payload).to_string();
                        let target = resolve_socket_service(&alloc_for_recv, &socket_unit);
                        info!("Socket '{}' fired — triggering '{}'", socket_unit, target);

                        // Skip when the service is already active or coming
                        // up: the activation monitor keeps firing on every
                        // readable edge and a redundant start job would just
                        // fail with EALREADY.
                        let busy = {
                            let state = alloc_for_recv.read();
                            state
                                .unit_states
                                .get(&target)
                                .map(|c| {
                                    matches!(
                                        c.active_state.as_str(),
                                        "active" | "activating" | "reloading"
                                    )
                                })
                                .unwrap_or(false)
                        };
                        if busy {
                            debug!(
                                "Socket-triggered service '{}' already active — ignoring",
                                target
                            );
                            continue;
                        }

                        // Ensure the target unit is loaded before enqueueing.
                        if !alloc_for_recv.read().units.contains_key(&target) {
                            let alloc2 = alloc_for_recv.clone();
                            let name2 = target.clone();
                            let loaded = tokio::task::spawn_blocking(move || {
                                load_unit_sync(&alloc2, &name2)
                            })
                            .await;
                            match loaded {
                                Ok(Ok(())) => {}
                                Ok(Err(e)) => {
                                    warn!(
                                        "Failed to load socket-triggered unit '{}': {}",
                                        target, e
                                    );
                                    continue;
                                }
                                Err(e) => {
                                    warn!(
                                        "Failed to load socket-triggered unit '{}': {}",
                                        target, e
                                    );
                                    continue;
                                }
                            }
                        }

                        match crate::scheduler::enqueue_start_with_mode(
                            alloc_for_recv.clone(),
                            &target,
                            crate::state::JobMode::Replace,
                        )
                        .await
                        {
                            Ok(job_id) => {
                                info!("Enqueued job {job_id} for socket-triggered '{target}'");
                            }
                            Err(e) => {
                                warn!("Cannot start socket-triggered unit '{}': {}", target, e);
                            }
                        }
                        continue;
                    }

                    if env.method == "cgroup.metrics" {
                        let update = match CgroupMetricsUpdate::decode(env.payload.as_slice()) {
                            Ok(u) => u,
                            Err(e) => {
                                warn!(
                                    "CgroupMetricsUpdate decode from worker '{}': {} — disconnecting",
                                    worker_id_recv, e
                                );
                                break;
                            }
                        };
                        // Fire-and-forget: System R is the only authority on
                        // cgroup metrics, so the snapshot is accepted without
                        // ownership checks and cached opaquely for the control
                        // plane to serve.
                        if !update.units.is_empty() {
                            let mut event_metas: Vec<Event> = Vec::new();
                            {
                                let mut state = alloc_for_recv.write();
                                for unit in update.units {
                                    let name = unit.unit_name.clone();
                                    let mut buf = bytes::BytesMut::new();
                                    let _ = unit.encode(&mut buf);
                                    event_metas.push(Event {
                                        topic: sysa::event_bus::EventTopic::UnitMetrics,
                                        unit_name: name.clone(),
                                        worker_id: worker_id_recv.clone(),
                                        timestamp: tokio::time::Instant::now(),
                                        data: buf.freeze(),
                                    });
                                    state.cgroup_metrics.insert(name, unit);
                                }
                            }
                            let bus = alloc_for_recv.read().event_bus.clone();
                            for ev in event_metas {
                                bus.read().await.dispatch(&ev).await;
                            }
                        }
                        continue;
                    }

                    if env.method == "event.subscribe" {
                        let req = match EventSubscribe::decode(env.payload.as_slice()) {
                            Ok(r) => r,
                            Err(e) => {
                                warn!(
                                    "EventSubscribe decode from worker '{}': {} — ignoring",
                                    worker_id_recv, e
                                );
                                continue;
                            }
                        };
                        if req.unit_names.is_empty() {
                            subscription.all = true;
                        } else {
                            for name in &req.unit_names {
                                subscription.units.insert(name.clone());
                            }
                        }
                        info!(
                            "Worker '{}' subscribed to unit events (all={}, units={:?})",
                            worker_id_recv, subscription.all, subscription.units
                        );
                        sub_id = apply_worker_subscription(
                            &alloc_for_recv,
                            &worker_id_recv,
                            &forward_tx,
                            &subscription,
                            sub_id,
                        )
                        .await;
                        continue;
                    }

                    if env.method == "event.unsubscribe" {
                        let req = match EventUnsubscribe::decode(env.payload.as_slice()) {
                            Ok(r) => r,
                            Err(e) => {
                                warn!(
                                    "EventUnsubscribe decode from worker '{}': {} — ignoring",
                                    worker_id_recv, e
                                );
                                continue;
                            }
                        };
                        if req.unit_names.is_empty() {
                            subscription = WorkerSubscription::default();
                        } else {
                            for name in &req.unit_names {
                                subscription.units.remove(name);
                            }
                        }
                        info!(
                            "Worker '{}' unsubscribed from unit events (all={}, units={:?})",
                            worker_id_recv, subscription.all, subscription.units
                        );
                        sub_id = apply_worker_subscription(
                            &alloc_for_recv,
                            &worker_id_recv,
                            &forward_tx,
                            &subscription,
                            sub_id,
                        )
                        .await;
                        continue;
                    }

                    if env.method == "worker.ready" {
                        let ready_worker = worker_id_recv.clone();
                        let mut state = alloc_for_recv.write();
                        if let Some(entry) = state.workers.get_mut(&ready_worker) {
                            entry.ready = true;
                        }
                        let ready_count = state.workers.values().filter(|w| w.ready).count();
                        drop(state);
                        info!("Worker '{ready_worker}' is ready ({ready_count} worker(s) ready)");
                        sysa::notify::broadcast(&[("WORKER_READY", &ready_worker)]);
                        sysa::notify::broadcast(&[(
                            "STATUS",
                            &format!("workers-ready={ready_count}"),
                        )]);
                        continue;
                    }

                    if env.method == "worker.exit" {
                        // Polite goodbye: the worker flushed everything it
                        // had queued and now waits for us to close.  Breaking
                        // out ends both tasks and drops the socket, which is
                        // the acknowledgement — no reply envelope exists.
                        let reason = WorkerExit::decode(env.payload.as_slice())
                            .map(|r| r.reason)
                            .unwrap_or_default();
                        let why = if reason.is_empty() {
                            "no reason given"
                        } else {
                            &reason
                        };
                        info!(
                            "Worker '{worker_id_recv}' is exiting ({why}) — closing its connection"
                        );
                        break;
                    }

                    warn!(
                        "Unknown method from worker '{}': {} — disconnecting",
                        worker_id_recv, env.method
                    );
                    break;
                }
            }
        }

        if let Some(id) = sub_id {
            let bus = alloc_for_recv.read().event_bus.clone();
            bus.write().await.unsubscribe(id);
            info!("Worker '{}' unsubscribed from unit events", worker_id_recv);
        }

        Ok::<_, anyhow::Error>(())
    };

    tokio::select! {
        res = sender => {
            if let Err(e) = res { warn!("Sender error for worker: {}", e); }
        }
        res = receiver => {
            if let Err(e) = res { warn!("Receiver error for worker: {}", e); }
        }
    }

    // Deregister the worker and cancel any pending jobs.
    {
        let mut state = allocator.write();
        state.workers.remove(&worker_id);

        use crate::state::{JobCompletion, JobResultKind, JobStatus};
        let stale: Vec<JobCompletion> = state
            .jobs
            .values_mut()
            .filter(|j| matches!(j.status, JobStatus::Running))
            .map(|j| {
                j.status = JobStatus::Cancelled;
                JobCompletion {
                    job_id: j.id,
                    unit_name: j.unit_name.clone(),
                    result: JobResultKind::Cancelled,
                }
            })
            .collect();

        if let Some(ref tx) = state.job_completion_tx {
            for completion in stale {
                let _ = tx.send(completion);
            }
        }
    }
    info!("Worker '{}' deregistered", worker_id);

    Ok(())
}

/// Tracks a worker connection's unit-event subscription.
///
/// `event.subscribe` is additive, `event.unsubscribe` subtractive; an empty
/// unit list on subscribe means "all units", on unsubscribe means "everything".
#[derive(Debug, Default)]
struct WorkerSubscription {
    all: bool,
    units: HashSet<String>,
}

/// Resolve the service unit a socket unit activates.
///
/// An explicit `[Socket] Service=` directive wins; otherwise the name is
/// derived following the systemd convention "foo.socket" -> "foo.service".
/// Falls back to plain derivation when the socket unit is unknown.
fn resolve_socket_service(allocator: &AllocatorHandle, socket_unit: &str) -> String {
    let derived = socket_unit.replace(".socket", ".service");
    let state = allocator.read();
    state
        .units
        .get(socket_unit)
        .and_then(|ir| ir.socket.as_ref())
        .filter(|sc| !sc.service.is_empty())
        .map(|sc| sc.service.clone())
        .unwrap_or(derived)
}

/// (Re)register the EventBus subscriber for a worker connection.
///
/// Removes the previous subscriber (if any) and registers a fresh
/// [`WorkerEventForwarder`] matching the given subscription, then replays the
/// currently active units so the worker converges without waiting for the
/// next transition.  Returns `None` when the subscription is empty.
async fn apply_worker_subscription(
    allocator: &AllocatorHandle,
    worker_id: &str,
    forward_tx: &tokio::sync::mpsc::Sender<bytes::Bytes>,
    subscription: &WorkerSubscription,
    previous: Option<u64>,
) -> Option<u64> {
    let bus = allocator.read().event_bus.clone();
    let mut bus = bus.write().await;

    if let Some(id) = previous {
        bus.unsubscribe(id);
    }

    if !subscription.all && subscription.units.is_empty() {
        return None;
    }

    let forwarder = WorkerEventForwarder::new(
        worker_id,
        forward_tx.clone(),
        subscription.all,
        subscription.units.iter().cloned().collect(),
        allocator.clone(),
    );
    let id = bus.subscribe(Arc::new(forwarder));
    // Release the bus before replaying: dispatch holds a bus read lock while
    // subscribers read the allocator, so we must not read allocator state
    // while holding the bus write lock.
    drop(bus);

    replay_active_units(
        allocator,
        worker_id,
        forward_tx,
        subscription.all,
        &subscription.units,
    );

    Some(id)
}

async fn handle_finder_register(
    mut framed: sysa::ipc::EnvelopeFramed,
    env: Envelope,
    allocator: AllocatorHandle,
    client_uid: u32,
) -> Result<()> {
    let result = try_finder_register(env, allocator, client_uid).await;
    let ack = match &result {
        Ok(ack) => ack.clone(),
        Err(e) => {
            warn!("Finder register (UID={client_uid}) failed: {e}");
            UnitRegistrationAck {
                success: false,
                message: e.to_string(),
                unit_count: 0,
            }
        }
    };
    let ack_env = make_envelope(next_request_id(), "system-a", "system-f", "finder.ack", ack)?;
    send_envelope(&mut framed, &ack_env).await?;
    info!("Finder register session complete — disconnecting");
    Ok(())
}

async fn try_finder_register(
    env: Envelope,
    allocator: AllocatorHandle,
    uid: u32,
) -> Result<UnitRegistrationAck> {
    let reg_msg = RegisterUnits::decode(env.payload.as_slice())?;
    let name = reg_msg.name;
    info!("Finder (UID={uid}) registering units (name='{name}')");

    let units: std::collections::HashMap<String, systema_sysf::ir::UnitIR> =
        serde_json::from_slice(&reg_msg.units_json).map_err(|e| {
            anyhow::anyhow!(sysa::l10n::fmt(
                sysa::l10n::t_("Failed to deserialize UnitIR JSON: {error}"),
                &[("error", &e.to_string())],
            ))
        })?;

    let mut state = allocator.write();
    match state.init_staging_area(uid, &name, units) {
        Ok(count) => {
            drop(state);
            Ok(UnitRegistrationAck {
                success: true,
                message: sysa::l10n::fmt(
                    sysa::l10n::t_("{count} units staged for UID {uid}."),
                    &[("count", &count.to_string()), ("uid", &uid.to_string())],
                ),
                unit_count: count,
            })
        }
        Err(msg) => {
            drop(state);
            warn!("{msg}");
            Ok(UnitRegistrationAck {
                success: false,
                message: msg,
                unit_count: 0,
            })
        }
    }
}

async fn handle_finder_commit(
    mut framed: sysa::ipc::EnvelopeFramed,
    env: Envelope,
    allocator: AllocatorHandle,
    client_uid: u32,
) -> Result<()> {
    let result = try_finder_commit(env, allocator, client_uid).await;
    let ack = match &result {
        Ok(ack) => ack.clone(),
        Err(e) => {
            warn!("Finder commit (UID={client_uid}) failed: {e}");
            UnitRegistrationAck {
                success: false,
                message: e.to_string(),
                unit_count: 0,
            }
        }
    };
    let ack_env = make_envelope(next_request_id(), "system-a", "system-f", "finder.ack", ack)?;
    send_envelope(&mut framed, &ack_env).await?;
    info!("Finder commit session complete — disconnecting");
    Ok(())
}

async fn try_finder_commit(
    env: Envelope,
    allocator: AllocatorHandle,
    client_uid: u32,
) -> Result<UnitRegistrationAck> {
    let commit_msg = CommitUnits::decode(env.payload.as_slice())?;
    let name = &commit_msg.name;
    let target_uid = if commit_msg.uid == 0 {
        client_uid
    } else {
        commit_msg.uid
    };
    if client_uid != 0 && target_uid != client_uid {
        return Ok(UnitRegistrationAck {
            success: false,
            message: sysa::l10n::fmt(
                sysa::l10n::t_(
                    "permission denied (UID {client_uid}): may only commit own staging areas",
                ),
                &[("client_uid", &client_uid.to_string())],
            ),
            unit_count: 0,
        });
    }
    info!("Finder (UID={client_uid}) committing staging area (uid={target_uid}, name='{name}')");

    let count = {
        let mut state = allocator.write();
        match state.commit_staging(target_uid, name) {
            Ok(count) => count,
            Err(msg) => {
                drop(state);
                warn!("{msg}");
                return Ok(UnitRegistrationAck {
                    success: false,
                    message: msg,
                    unit_count: 0,
                });
            }
        }
    };
    // Boot-time units reach System A exclusively through staging commits,
    // which historically skipped `unit_add_default_dependencies()`: services
    // then lacked the implicit Requires+After=sysinit.target edge, so e.g.
    // systemd-user-sessions.service could race systemd-tmpfiles-setup.service
    // (the /run/nologin bug).  Enqueue a FromCommit request so the
    // ReloadTask applies default dependencies and syncs workers; the pass
    // is idempotent (set inserts).
    if let Some(tx) = allocator.read().reload_tx.as_ref() {
        let _ = tx.try_send(crate::reload_task::ReloadRequest::FromCommit);
    }

    Ok(UnitRegistrationAck {
        success: true,
        message: sysa::l10n::fmt(
            sysa::l10n::t_("{count} units committed for UID {uid}."),
            &[
                ("count", &count.to_string()),
                ("uid", &target_uid.to_string()),
            ],
        ),
        unit_count: count,
    })
}

async fn handle_finder_query(
    mut framed: sysa::ipc::EnvelopeFramed,
    env: Envelope,
    allocator: AllocatorHandle,
    client_uid: u32,
) -> Result<()> {
    let result = try_finder_query(env, allocator, client_uid).await;
    let ack = match &result {
        Ok(ack) => ack.clone(),
        Err(e) => {
            warn!("Staging query (UID={client_uid}) failed: {e}");
            StagingQueryResult {
                success: false,
                message: e.to_string(),
                units_json: vec![],
                unit_count: 0,
            }
        }
    };
    let ack_env = make_envelope(
        next_request_id(),
        "system-a",
        "system-f",
        "staging.query_result",
        ack,
    )?;
    send_envelope(&mut framed, &ack_env).await?;
    Ok(())
}

async fn try_finder_query(
    env: Envelope,
    allocator: AllocatorHandle,
    client_uid: u32,
) -> Result<StagingQueryResult> {
    let query_msg = StagingQuery::decode(env.payload.as_slice())?;
    let name = &query_msg.name;
    let target_uid = if query_msg.uid == 0 {
        client_uid
    } else {
        query_msg.uid
    };
    if client_uid != 0 && target_uid != client_uid {
        return Ok(StagingQueryResult {
            success: false,
            message: sysa::l10n::fmt(
                sysa::l10n::t_(
                    "permission denied (UID {client_uid}): may only query own staging areas",
                ),
                &[("client_uid", &client_uid.to_string())],
            ),
            units_json: vec![],
            unit_count: 0,
        });
    }
    let state = allocator.read();
    match state.get_staging_area(target_uid, name) {
        Some(area) => {
            let count = area.units.len() as u32;
            let json = serde_json::to_vec(&area.units).map_err(|e| {
                anyhow::anyhow!(sysa::l10n::fmt(
                    sysa::l10n::t_("Failed to serialize staging units: {e}"),
                    &[("e", &e.to_string())]
                ))
            })?;
            Ok(StagingQueryResult {
                success: true,
                message: String::new(),
                units_json: json,
                unit_count: count,
            })
        }
        None => {
            let msg = sysa::l10n::fmt(
                sysa::l10n::t_("no staging area for UID {target_uid} with name '{name}'"),
                &[
                    ("target_uid", &target_uid.to_string()),
                    ("name", &name.to_string()),
                ],
            );
            warn!("{msg}");
            Ok(StagingQueryResult {
                success: false,
                message: msg,
                units_json: vec![],
                unit_count: 0,
            })
        }
    }
}

/// Build the `admin.staging` reply over the allocator's staging-area state.
///
/// The `admin.*` API was moved to the control port ([`crate::ipc::control`]);
/// only this pure builder remains in the allocator module, shared with the
/// control session handler.
pub(crate) fn build_admin_result(op: &AdminStagingOp, allocator: &AllocatorHandle) -> AdminStagingResult {
    let state = allocator.read();
    match op.op.as_str() {
        "list" => {
            let entries: Vec<StagingAreaEntry> = state
                .list_staging_areas()
                .into_iter()
                .map(|(uid, name)| {
                    let count = state
                        .staging_areas
                        .get(&(uid, name.clone()))
                        .map(|a| a.units.len() as u32)
                        .unwrap_or(0);
                    StagingAreaEntry {
                        uid,
                        name,
                        unit_count: count,
                        units_json: vec![],
                    }
                })
                .collect();
            AdminStagingResult {
                success: true,
                message: String::new(),
                entries,
            }
        }
        "by_uid" => {
            let areas = state.get_staging_areas_by_uid(op.uid);
            if areas.is_empty() {
                AdminStagingResult {
                    success: false,
                    message: sysa::l10n::fmt(
                        sysa::l10n::t_("no staging area for UID {uid}"),
                        &[("uid", &(op.uid).to_string())],
                    ),
                    entries: vec![],
                }
            } else {
                let entries = areas
                    .into_iter()
                    .map(|a| {
                        let json = serde_json::to_vec(&a.units).unwrap_or_default();
                        StagingAreaEntry {
                            uid: a.uid,
                            name: a.name.clone(),
                            unit_count: a.units.len() as u32,
                            units_json: json,
                        }
                    })
                    .collect();
                AdminStagingResult {
                    success: true,
                    message: String::new(),
                    entries,
                }
            }
        }
        "by_id" => match state.get_staging_area(op.uid, &op.name) {
            Some(area) => {
                let json = serde_json::to_vec(&area.units).unwrap_or_default();
                AdminStagingResult {
                    success: true,
                    message: String::new(),
                    entries: vec![StagingAreaEntry {
                        uid: area.uid,
                        name: area.name.clone(),
                        unit_count: area.units.len() as u32,
                        units_json: json,
                    }],
                }
            }
            None => AdminStagingResult {
                success: false,
                message: sysa::l10n::fmt(
                    sysa::l10n::t_("no staging area for UID {uid} with name '{name}'"),
                    &[
                        ("uid", &(op.uid).to_string()),
                        ("name", &(op.name).to_string()),
                    ],
                ),
                entries: vec![],
            },
        },
        "by_name" => {
            let areas = state.get_staging_areas_by_name(&op.name);
            if areas.is_empty() {
                AdminStagingResult {
                    success: false,
                    message: sysa::l10n::fmt(
                        sysa::l10n::t_("no staging area with name '{name}'"),
                        &[("name", &(op.name).to_string())],
                    ),
                    entries: vec![],
                }
            } else {
                let entries = areas
                    .iter()
                    .map(|a| {
                        let json = serde_json::to_vec(&a.units).unwrap_or_default();
                        StagingAreaEntry {
                            uid: a.uid,
                            name: a.name.clone(),
                            unit_count: a.units.len() as u32,
                            units_json: json,
                        }
                    })
                    .collect();
                AdminStagingResult {
                    success: true,
                    message: String::new(),
                    entries,
                }
            }
        }
        "all" => {
            let entries: Vec<StagingAreaEntry> = state
                .all_staging_areas()
                .values()
                .map(|a| {
                    let json = serde_json::to_vec(&a.units).unwrap_or_default();
                    StagingAreaEntry {
                        uid: a.uid,
                        name: a.name.clone(),
                        unit_count: a.units.len() as u32,
                        units_json: json,
                    }
                })
                .collect();
            AdminStagingResult {
                success: true,
                message: String::new(),
                entries,
            }
        }
        other => AdminStagingResult {
            success: false,
            message: sysa::l10n::fmt(
                sysa::l10n::t_("unknown admin staging op '{other}'"),
                &[("other", &other.to_string())],
            ),
            entries: vec![],
        },
    }
}

/// Current time in microseconds since the Unix epoch (D-Bus type `t`).
fn now_usec() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// Merge a worker-reported `UnitStatus` into the runtime cache, tracking
/// state-transition timestamps (systemd `ActiveEnterTimestamp` /
/// `InactiveEnterTimestamp`) and never regressing a known invocation ID.
fn apply_state_to_cache(entry: &mut CachedUnitState, status: &UnitStatus) {
    let prev_active = entry.active_state == "active";
    let prev_inactive = matches!(entry.active_state.as_str(), "inactive" | "failed");
    let now_active = status.active_state == "active";
    let now_inactive = matches!(status.active_state.as_str(), "inactive" | "failed");

    let now = now_usec();
    if now_active && !prev_active {
        entry.active_enter_timestamp = now;
        entry.inactive_enter_timestamp = 0;
    }
    if now_inactive && !prev_inactive {
        entry.inactive_enter_timestamp = now;
        entry.active_enter_timestamp = 0;
    }

    entry.active_state = status.active_state.clone();
    entry.sub_state = status.sub_state.clone();
    entry.main_pid = status.main_pid;
    entry.extensions = status.extensions.clone();
    if !status.invocation_id.is_empty() {
        entry.invocation_id = status.invocation_id.clone();
    } else if now_inactive {
        entry.invocation_id.clear();
    }
}

/// Revert a unit's cached runtime state to `inactive`.
///
/// Called when a Start/Restart job ends in `Failed` (either directly or via
/// dependency failure) so that the cache reflects the reality that the unit
/// did not successfully start.  This prevents the `unit_states` entry from
/// remaining `active` after a failed Start job.
fn revert_cache_to_inactive(state: &mut crate::state::AllocatorState, unit_name: &str) {
    let now = now_usec();
    let entry = state.unit_states.entry(unit_name.to_string()).or_default();
    if !matches!(entry.active_state.as_str(), "inactive" | "failed") {
        entry.active_enter_timestamp = 0;
    }
    entry.active_state = "inactive".to_string();
    entry.sub_state.clear();
    entry.invocation_id.clear();
    if entry.inactive_enter_timestamp == 0 {
        entry.inactive_enter_timestamp = now;
    }
}

/// Whether the unit currently has a terminal-Failed Start/Restart job.
///
/// Used to guard the `method.result` cache path: a worker may report a unit
/// as `active` (e.g. a target worker that already set the target state) even
/// though the unit's job failed (e.g. via dependency failure).  The stale
/// worker status must not overwrite the failed-job reality back to `active`.
fn unit_has_failed_start_job(state: &crate::state::AllocatorState, unit_name: &str) -> bool {
    state.jobs.values().any(|j| {
        j.unit_name == unit_name
            && matches!(
                j.kind,
                crate::state::JobKind::Start | crate::state::JobKind::Restart
            )
            && matches!(j.status, crate::state::JobStatus::Failed(_))
    })
}

/// Optimistically update the runtime cache when a task completes, based on
/// what we know the new state should be.
fn update_cache_on_task_result(
    allocator: &AllocatorHandle,
    unit_name: &str,
    kind: crate::state::JobKind,
    success: bool,
) {
    if !success {
        return;
    }
    let mut state = allocator.write();
    let entry = state.unit_states.entry(unit_name.to_string()).or_default();
    match kind {
        crate::state::JobKind::Start | crate::state::JobKind::Restart => {
            entry.active_state = "active".to_string();
            entry.sub_state = match kind {
                crate::state::JobKind::Start => "start".to_string(),
                _ => entry.sub_state.clone(),
            };
            if entry.active_enter_timestamp == 0 {
                entry.active_enter_timestamp = now_usec();
            }
            entry.inactive_enter_timestamp = 0;
        }
        crate::state::JobKind::Stop => {
            entry.active_state = "inactive".to_string();
            entry.sub_state = "dead".to_string();
            entry.invocation_id.clear();
            entry.active_enter_timestamp = 0;
            if entry.inactive_enter_timestamp == 0 {
                entry.inactive_enter_timestamp = now_usec();
            }
        }
        crate::state::JobKind::Reload => {}
        // Nop jobs never dispatch to a worker, so no state update can be
        // attributed to them; keep the match exhaustive defensively.
        crate::state::JobKind::Nop => {}
    }
}

/// Handle a `unit.state_update` push (or `unit.sync_report` snapshot) from
/// a worker.
///
/// This handler is deliberately side-effect-free with respect to the
/// allocator's job/desired tables: it only updates the `unit_states` cache,
/// then notifies in-process subscribers (e.g. the restart-policy handler)
/// through the event bus.  State management beyond that belongs to the
/// workers.
///
/// Ownership: incremental updates are rejected when the unit is owned by a
/// different worker (ownership is bound when a job is dispatched, or when a
/// worker first reports an unowned unit); unknown or unowned units are
/// accepted and claimed by the reporting worker.  Full snapshots (sent on
/// worker (re)connect and in response to `unit.sync_request`) are accepted
/// unconditionally.
async fn handle_state_update(
    allocator: AllocatorHandle,
    sender: &str,
    request_id: u64,
    update: UnitStateUpdate,
) {
    use sysa::event_bus::EventTopic;

    let mut ignored: Vec<String> = Vec::new();
    let mut dispatched: Vec<Event> = Vec::new();

    for status in &update.units {
        // Ownership validation for incremental updates.  Reject only when
        // the unit is owned by a *different* worker; units without an owner
        // are accepted and claimed by the first worker that reports them
        // (e.g. mounts discovered from /proc/self/mountinfo at boot, or the
        // companion of an automount that was mounted by a kernel trigger).
        if !update.full_snapshot {
            let owner = allocator.read().unit_owners.get(&status.unit_name).cloned();
            match owner {
                Some(owner) if owner != sender => {
                    warn!(
                        "unit.state_update for {} from '{}' ignored (owned by '{}')",
                        status.unit_name, sender, owner
                    );
                    ignored.push(status.unit_name.clone());
                    continue;
                }
                Some(_) => {}
                None => {
                    allocator
                        .write()
                        .unit_owners
                        .insert(status.unit_name.clone(), sender.to_string());
                }
            }
        }

        // Cache update only.
        {
            let mut state = allocator.write();
            let dead = status.active_state == "inactive" && status.sub_state == "dead";
            let status_c = UnitStatus::from_proto(status.clone());
            let dispatched_id = state.invocation_ids.get(&status_c.unit_name).cloned();
            let entry = state
                .unit_states
                .entry(status_c.unit_name.clone())
                .or_default();
            apply_state_to_cache(entry, &status_c);
            if dead {
                // Ownership and the invocation ID end with the unit's life.
                state.unit_owners.remove(&status_c.unit_name);
                state.invocation_ids.remove(&status_c.unit_name);
            } else if entry.active_state == "active" && entry.invocation_id.is_empty() {
                // Units that report no invocation ID (e.g. mounts discovered
                // from mountinfo) get one from System A, mirroring systemd's
                // assignment of an ID to every activation.
                let id = dispatched_id.unwrap_or_else(crate::state::generate_invocation_id);
                entry.invocation_id = id.clone();
                state.invocation_ids.insert(status_c.unit_name.clone(), id);
            }
        }
        debug!(
            "unit.state_update: {} active={} sub={} (full_snapshot={})",
            status.unit_name, status.active_state, status.sub_state, update.full_snapshot
        );

        // Notify in-process subscribers on incremental transitions only.
        if !update.full_snapshot {
            // Login-session accounting: keep the per-UID session set of
            // `session-*.scope` units (those whose `Slice=` is a
            // `user-<UID>.slice`) in sync with the reported scope state, and
            // forward the UID's new session count to System R so each user's
            // `user-<UID>.slice` follows its sessions.
            let slice = allocator
                .read()
                .units
                .get(&status.unit_name)
                .map(|u| u.unit.slice.clone())
                .unwrap_or_default();
            let forwarded = {
                let mut state = allocator.write();
                crate::state::track_session(
                    &mut state.user_sessions,
                    &status.unit_name,
                    &slice,
                    status.active_state == "active",
                )
            };
            if let Some((uid, count)) = forwarded {
                send_user_session_update(allocator.clone(), uid, count).await;
            }

            let mut status_buf = Vec::new();
            let _ = status.encode(&mut status_buf);
            dispatched.push(Event {
                topic: EventTopic::UnitStateChange,
                unit_name: status.unit_name.clone(),
                worker_id: sender.to_string(),
                timestamp: tokio::time::Instant::now(),
                data: bytes::Bytes::from(status_buf),
            });

            // Control-plane lifecycle: a unit's state changed → push its
            // full snapshot to control-port subscribers (the System Wrapper
            // bridge updates its mirror's Unit interface).
            let snap_buf = {
                let state = allocator.read();
                let mut buf = bytes::BytesMut::new();
                let _ = crate::snapshot::unit_snapshot(&state, &status.unit_name)
                    .encode(&mut buf);
                buf
            };
            dispatched.push(Event {
                topic: EventTopic::UnitChanged,
                unit_name: status.unit_name.clone(),
                worker_id: sender.to_string(),
                timestamp: tokio::time::Instant::now(),
                data: bytes::Bytes::from(snap_buf),
            });
        }
    }

    let ack = UnitStateUpdateAck {
        accepted: ignored.is_empty(),
        ignored_units: ignored,
        message: String::new(),
    };
    // The ACK must echo the request_id of the envelope it answers: the
    // worker's reader resolves its pending publish by that ID.
    let ack_env = match make_envelope(request_id, "system-a", sender, "unit.state_update_ack", ack)
    {
        Ok(env) => env,
        Err(e) => {
            warn!("Failed to build unit.state_update_ack for '{sender}': {e}");
            return;
        }
    };
    let mut buf = bytes::BytesMut::new();
    if ack_env.encode(&mut buf).is_err() {
        warn!("Failed to encode unit.state_update_ack for '{sender}'");
        return;
    }
    let worker_tx = {
        let state = allocator.read();
        state.workers.get(sender).map(|w| w.envelope_tx.clone())
    };
    match worker_tx {
        Some(tx) => {
            // Bounded: a worker that stops reading its envelope channel
            // must not wedge the single-threaded System A runtime forever.
            match tokio::time::timeout(std::time::Duration::from_secs(2), tx.send(buf.freeze()))
                .await
            {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    warn!("Failed to send unit.state_update_ack to '{}': {e}", sender);
                }
                Err(_) => {
                    warn!("Timed out sending unit.state_update_ack to '{}'", sender);
                }
            }
        }
        None => warn!("unit.state_update_ack: worker '{sender}' not registered"),
    }

    for ev in dispatched {
        let bus = allocator.read().event_bus.clone();
        bus.read().await.dispatch(&ev).await;
    }
}

/// The System R worker owns the cgroup hierarchy for user slices.
const SYSTEM_R_WORKER_ID: &str = "system-r-1";

/// Forward a user's absolute login-session count to System R
/// (`user.sessions`), which creates or releases the user's `user-<UID>.slice`
/// on the 0 ↔ N transitions.  Fire-and-forget: a missing or disconnected
/// System R is only logged.
async fn send_user_session_update(allocator: AllocatorHandle, uid: u32, count: usize) {
    let env = match make_envelope(
        next_request_id(),
        "system-a",
        SYSTEM_R_WORKER_ID,
        "user.sessions",
        sysa::proto::UserSessionEvent {
            uid,
            sessions: count as u32,
        },
    ) {
        Ok(env) => env,
        Err(e) => {
            warn!("Failed to build user.sessions for uid {uid}: {e}");
            return;
        }
    };
    let mut buf = bytes::BytesMut::new();
    if env.encode(&mut buf).is_err() {
        warn!("Failed to encode user.sessions for uid {uid}");
        return;
    }
    let worker_tx = {
        let state = allocator.read();
        state
            .workers
            .get(SYSTEM_R_WORKER_ID)
            .map(|w| w.envelope_tx.clone())
    };
    match worker_tx {
        Some(tx) => {
            if tx.send(buf.freeze()).await.is_err() {
                warn!("Failed to send user.sessions to '{SYSTEM_R_WORKER_ID}'");
            }
        }
        None => debug!("System R not connected; user.sessions for uid {uid} dropped"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::state::AllocatorState;
    use crate::unit::types::UnitFile;

    #[test]
    fn resolve_socket_service_prefers_directive() {
        use crate::unit::types::SocketSection;

        let state = Arc::new(parking_lot::RwLock::new(AllocatorState::new()));
        {
            let mut state = state.write();
            // Explicit Service= directive.
            let mut explicit = UnitFile::new("a.socket");
            explicit.socket = Some(SocketSection {
                service: "elsewhere.service".to_string(),
                ..Default::default()
            });
            // No directive: falls back to name derivation.
            let mut plain = UnitFile::new("b.socket");
            plain.socket = Some(SocketSection {
                listen_netlink: vec!["kobject-uevent".to_string()],
                ..Default::default()
            });
            state.units.insert("a.socket".to_string(), explicit);
            state.units.insert("b.socket".to_string(), plain);
        }

        assert_eq!(
            resolve_socket_service(&state, "a.socket"),
            "elsewhere.service"
        );
        assert_eq!(resolve_socket_service(&state, "b.socket"), "b.service");
        // Unknown socket unit: pure derivation.
        assert_eq!(
            resolve_socket_service(&state, "ghost.socket"),
            "ghost.service"
        );
    }

    #[test]
    fn revert_cache_to_inactive_clears_active_entry() {
        let mut state = crate::state::AllocatorState::new();
        let entry = state
            .unit_states
            .entry("poweroff.target".to_string())
            .or_default();
        entry.active_state = "active".to_string();
        entry.sub_state = "running".to_string();
        entry.invocation_id = "some-uuid".to_string();
        entry.active_enter_timestamp = 123;
        entry.inactive_enter_timestamp = 0;

        revert_cache_to_inactive(&mut state, "poweroff.target");

        let entry = state.unit_states.get("poweroff.target").unwrap();
        assert_eq!(entry.active_state, "inactive");
        assert_eq!(entry.sub_state, "");
        assert_eq!(entry.invocation_id, "");
        assert_eq!(entry.active_enter_timestamp, 0);
        assert_ne!(entry.inactive_enter_timestamp, 0);
    }

    #[test]
    fn revert_cache_to_inactive_preserves_failed_state_but_sets_inactive() {
        // A failed start should leave the unit inactive (not "failed" cached
        // state from the worker).
        let mut state = crate::state::AllocatorState::new();
        let entry = state
            .unit_states
            .entry("foo.service".to_string())
            .or_default();
        entry.active_state = "failed".to_string();

        revert_cache_to_inactive(&mut state, "foo.service");

        assert_eq!(
            state.unit_states.get("foo.service").unwrap().active_state,
            "inactive"
        );
    }

    #[test]
    fn unit_has_failed_start_job_detects_failed_start() {
        use crate::state::{Job, JobKind, JobStatus};

        let mut state = crate::state::AllocatorState::new();
        state.jobs.insert(
            1,
            Job {
                id: 1,
                unit_name: "poweroff.target".to_string(),
                kind: JobKind::Start,
                status: JobStatus::Failed(
                    "dependency failed: systemd-poweroff.service".to_string(),
                ),
                timeout_abort: None,
            },
        );

        assert!(unit_has_failed_start_job(&state, "poweroff.target"));
        // A failed Restart job is also a failed start-type job.
        state.jobs.insert(
            4,
            Job {
                id: 4,
                unit_name: "reloadable.service".to_string(),
                kind: JobKind::Restart,
                status: JobStatus::Failed("boom".to_string()),
                timeout_abort: None,
            },
        );
        assert!(unit_has_failed_start_job(&state, "reloadable.service"));
        // Other unit / other kinds / non-terminal jobs are not flagged.
        assert!(!unit_has_failed_start_job(
            &state,
            "systemd-poweroff.service"
        ));
        state.jobs.insert(
            2,
            Job {
                id: 2,
                unit_name: "other.service".to_string(),
                kind: JobKind::Start,
                status: JobStatus::Running,
                timeout_abort: None,
            },
        );
        assert!(!unit_has_failed_start_job(&state, "other.service"));
    }

    /// A worker's `worker.exit` goodbye is recognised (no "Unknown method"
    /// warning), answered by closing the connection — which *is* the ack —
    /// and followed by deregistration.
    #[tokio::test]
    async fn worker_exit_is_recognised_and_closes_the_connection() {
        // Captures this thread's log records; the session is polled on this
        // very thread (never spawned), so what it says lands in here.
        struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Capture {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let log_buf = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let buf_for_writer = log_buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_writer(move || Capture(buf_for_writer.clone()))
            .finish();
        let _log_guard = tracing::subscriber::set_default(subscriber);

        let allocator: AllocatorHandle = Arc::new(parking_lot::RwLock::new(AllocatorState::new()));
        let pending_fd = Arc::new(Mutex::new(VecDeque::new()));

        let (server, client) = tokio::net::UnixStream::pair().unwrap();
        let reg = WorkerRegistration {
            worker_id: "system-t-1".to_string(),
            unit_types: vec!["timer".to_string()],
            supports_unit_define: false,
        };
        let reg_env = make_envelope(7, "system-t-1", "system-a", "worker.register", reg).unwrap();

        let session =
            handle_worker_session(frame_stream(server), reg_env, allocator.clone(), pending_fd);
        tokio::pin!(session);

        let mut client = frame_stream(client);
        let ack = tokio::select! {
            ack = recv_envelope(&mut client) => ack,
            res = &mut session => panic!("session ended before acknowledging: {res:?}"),
        };
        let ack = ack
            .expect("registration ack must be readable")
            .expect("System A must acknowledge the registration");
        assert_eq!(ack.method, "worker.ack");
        assert!(
            allocator.read().workers.contains_key("system-t-1"),
            "worker must be registered after the handshake"
        );

        // The goodbye, carrying the reason SysAInit's SIGTERM produced.
        let exit = make_envelope(
            0,
            "system-t-1",
            "system-a",
            "worker.exit",
            WorkerExit {
                reason: "SIGTERM".to_string(),
            },
        )
        .unwrap();
        send_envelope(&mut client, &exit).await.unwrap();

        // System A answers by hanging up; the session deregisters on the way
        // out.
        let (eof, session_res) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let (eof, res) = tokio::join!(recv_envelope(&mut client), &mut session);
            (eof, res)
        })
        .await
        .expect("System A must close the connection after worker.exit");
        let eof = eof.expect("reading the goodbye must not fail");
        assert!(eof.is_none(), "expected EOF, got {eof:?}");
        session_res.expect("session must end cleanly after worker.exit");
        assert!(
            !allocator.read().workers.contains_key("system-t-1"),
            "worker must be deregistered after worker.exit"
        );

        let text = String::from_utf8_lossy(&log_buf.lock().unwrap()).to_string();
        assert!(
            text.contains("is exiting (SIGTERM)"),
            "System A must recognise worker.exit; log was: {text}"
        );
        assert!(
            !text.contains("Unknown method"),
            "worker.exit must not look like an unknown method; log was: {text}"
        );
    }
}
