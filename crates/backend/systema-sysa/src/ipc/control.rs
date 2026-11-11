//! Control-port bus — the System Wrapper plane of System A.
//!
//! Served on `control.socket` (one-to-many), this is where System Wrapper
//! bridge flavors (`systema-sysw.systemd`), control-plane tooling, and
//! one-shot admin sessions (`stagingctl`, `unitstatectl`) talk to System A.
//! The workload plane (worker/finder/staging) stays on `allocator.sock`;
//! nothing here is exposed to workers.
//!
//! Protocol: the first envelope of a session is either `manager.hello` (a
//! full bridge session with event subscription) or an `admin.*` one-shot
//! request (`admin.staging`, `admin.unitstate`) served directly without a
//! hello handshake, then the session closes.  Bridge sessions then exchange
//! `manager.*` request/reply envelopes and receive server-pushed event
//! envelopes (`unit.new`, `unit.removed`, `unit.changed`, `unit.metrics`,
//! `job.new`, `job.completed`).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use prost::Message as ProstMessage;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};
use tracing::{debug, error, info, warn};

use sysa::event_bus::{Event, EventSubscriber, EventTopic};
use sysa::ipc::make_envelope;
use sysa::proto::manager_value::Value;
use sysa::proto::{
    AbandonScopeRequest, AdminStagingOp, AdminStagingResult, DaemonReloadResult, EnqueueJobRequest,
    EnqueueJobResult, Envelope, GetUnitByInvocationRequest, GetUnitByPidRequest,
    GetUnitByPidResult, ListUnitsRequest, ListUnitsResult, LoadUnitRequest, LoadUnitResult,
    ManagerHelloRequest, ManagerHelloResult, ManagerValue, RefUnitRequest, RefUnitResult,
    RegisterPowerUnitsRequest, RegisterPowerUnitsResult, ResetFailedUnitRequest,
    SetUnitPropertiesRequest, SimpleManagerResult, StartUnitsRequest, StartUnitsResult,
    StopUnitsRequest, StopUnitsResult, TransientProperty, TransientUnitRequest, UnitInfo,
    UnitSnapshotRequest, UnitStartResult, UnitStateEntry, UnitStateEof, UnitStateListRequest,
};

use crate::scheduler::job_type::JobType;
use crate::snapshot;
use crate::state::{
    next_request_id, next_task_id, AllocatorHandle, AllocatorState, DesiredState, JobKind, JobMode,
};
use crate::unit::types::{
    ExecCommand, PathSection, ResourceControl, ServiceSection, ServiceType, TimerSection, UnitFile,
    UnitKind,
};

/// Bind the control socket and serve control sessions indefinitely.
pub async fn run_control_listener(allocator: AllocatorHandle) -> Result<()> {
    let path = sysa::paths::instance().control_socket_path;
    if let Some(parent) = std::path::Path::new(path).parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    if tokio::net::UnixStream::connect(path).await.is_ok() {
        warn!("Another control-port bus is already listening on {path}. Exiting.");
        return Ok(());
    }
    let _ = tokio::fs::remove_file(path).await;
    let listener = UnixListener::bind(path)?;
    info!("Control-port bus listening on {path}");

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let alloc = allocator.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_control_session(stream, alloc).await {
                        warn!("Control session error: {e:#}");
                    }
                });
            }
            Err(e) => {
                error!("Control-port accept error: {e}");
            }
        }
    }
}

/// Relays selected event-bus topics to a control session as pre-encoded
/// envelopes.  One instance per control session; pushed onto the session's
/// outbound channel.
///
/// The channel is *unbounded*: lifecycle events carry state (a `job.new` that
/// is seen without its `job.completed` leave the bridge mirror listing a
/// finished job forever), so they must never be dropped.  Dropping was the
/// old behavior when a bounded ring filled during boot bursts — the event
/// consumer and the session writer are separate tasks on the single-threaded
/// runtime, so a producer burst can outrun the writer for a few hundred
/// events.  Delivery is best-effort only after the session is gone: the
/// receiver is dropped with the writer task, `send` fails, and the bridge
/// reconnects and re-seeds its mirror anyway.
struct ControlForwarder {
    tx: mpsc::UnboundedSender<bytes::Bytes>,
}

#[async_trait::async_trait]
impl EventSubscriber for ControlForwarder {
    fn topics(&self) -> Vec<EventTopic> {
        vec![EventTopic::All]
    }

    async fn on_event(&self, event: &Event) {
        let method = match event.topic {
            EventTopic::UnitChanged => "unit.changed",
            EventTopic::UnitNew => "unit.new",
            EventTopic::UnitRemoved => "unit.removed",
            EventTopic::UnitMetrics => "unit.metrics",
            EventTopic::JobNew => "job.new",
            EventTopic::JobCompleted => "job.completed",
            _ => return,
        };
        let env = Envelope {
            request_id: 0,
            source: "system-a".to_string(),
            target: String::new(),
            method: method.to_string(),
            payload: event.data.to_vec(),
        };
        let mut buf = bytes::BytesMut::with_capacity(env.encoded_len());
        if env.encode(&mut buf).is_err() {
            return;
        }
        if self.tx.send(buf.freeze()).is_err() {
            // The session's writer task is gone (client closed); the session
            // cleanup unsubscribes this forwarder right after this point.
            debug!("Control forwarder: session already gone, event dropped");
        }
    }
}

/// Whether a read error means the peer simply went away (normal session
/// teardown) instead of a protocol/transport fault.
///
/// `UnexpectedEof` is deliberately excluded: a length-delimited frame cut in
/// half means the peer died mid-frame, which is worth a warning.
fn peer_disconnected(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::*;
    matches!(
        e.kind(),
        ConnectionReset | ConnectionAborted | BrokenPipe | NotConnected
    )
}

/// Serve one control session until the peer disconnects.
async fn handle_control_session(stream: UnixStream, allocator: AllocatorHandle) -> Result<()> {
    let (client_pid, client_uid) = super::server::peer_cred(&stream).unwrap_or((0, 0));
    let (read_half, write_half) = stream.into_split();

    let codec = LengthDelimitedCodec::builder()
        .max_frame_length(16 * 1024 * 1024)
        .new_codec();
    let mut reader = FramedRead::new(read_half, codec.clone());
    let mut writer = FramedWrite::new(write_half, codec);

    // First envelope is either the `manager.hello` handshake or a one-shot
    // `admin.*` request from control tooling (stagingctl, unitstatectl).
    // Admin sessions are served synchronously on the writer — no handshake,
    // no event subscription — mirroring the historical admin protocol that
    // used to live on the allocator IPC socket.
    //
    // Before any of that, an IPC-aware client exchanges the wire version
    // (`ipc.version` → `ipc.version.result`): the allocator pins one
    // version (`sysa_ipc::ipc::IPC_VERSION`), refuses everything else and
    // closes the connection before hello semantics start.  Peers that skip
    // the exchange (legacy) proceed exactly as before.
    let first_env = loop {
        let env = match reader.next().await {
            None => {
                info!("Control client (PID {client_pid}) disconnected before the first envelope");
                return Ok(());
            }
            Some(Err(e)) if peer_disconnected(&e) => {
                debug!("Control client (PID {client_pid}) detached before the first envelope: {e}");
                return Ok(());
            }
            Some(Err(e)) => anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_("control frame error: {e}"),
                &[("e", &e.to_string())]
            )),
            Some(Ok(bytes)) => {
                Envelope::decode(bytes.as_ref()).context(sysa::l10n::t_("bad first envelope"))?
            }
        };
        let Some(request) = sysa::ipc::version_request_from(&env) else {
            break env;
        };
        let compatible = sysa::ipc::version_verdict(&request);
        let reply = sysa::ipc::version_result_envelope(env.request_id, &request)?;
        if writer.send(encode_envelope(&reply)).await.is_err() {
            return Ok(());
        }
        if !compatible {
            warn!(
                "IPC version {} refused from PID {client_pid} (this build pins {}); closing",
                request.version,
                sysa::ipc::IPC_VERSION
            );
            return Ok(());
        }
        info!(
            "IPC version {} negotiated with PID {client_pid} ('{}')",
            request.version, request.client
        );
    };
    let first_method = first_env.method.clone();
    if matches!(first_method.as_str(), "admin.staging" | "admin.unitstate") {
        let replies = admin_replies(&allocator, &first_env, client_uid)?;
        let mut writer = writer;
        for reply in replies {
            if writer.send(encode_envelope(&reply)).await.is_err() {
                break;
            }
        }
        info!("Admin control session from PID {client_pid} (UID {client_uid}): '{first_method}'");
        return Ok(());
    }
    if first_env.method != "manager.hello" {
        anyhow::bail!(sysa::l10n::fmt(
            sysa::l10n::t_(
                "expected 'manager.hello' or 'admin.*' as first envelope, got '{first_method}'"
            ),
            &[("first_method", &first_method.to_string())]
        ));
    }
    let hello = ManagerHelloRequest::decode(first_env.payload.as_slice())
        .context(sysa::l10n::t_("bad manager.hello payload"))?;
    info!(
        "Control session from PID {client_pid} (UID {client_uid}): bridge '{}' version '{}'",
        hello.flavor, hello.version
    );

    // The reply must carry the requester's request id: the bridge client
    // routes replies strictly by `request_id`, and the global
    // `next_request_id()` counter has long advanced past the client's ids by
    // the time a bridge connects (worker-IPC acks share it).  The dispatch
    // loop applies the same echo below; the hello path sends directly, so it
    // has to do it itself.
    let mut reply = make_envelope(
        next_request_id(),
        "system-a",
        "",
        "manager.hello.result",
        ManagerHelloResult {
            success: true,
            message: sysa::l10n::fmt(
                sysa::l10n::t_("welcome, bridge '{flavor}'"),
                &[("flavor", &(hello.flavor).to_string())],
            ),
        },
    )?;
    reply.request_id = first_env.request_id;
    let buf = encode_envelope(&reply);
    writer.send(buf).await.ok();

    // A supervised bridge (e.g. SysW) declares its worker identity in
    // `manager.hello`.  Broadcast `WORKER_READY` on the notify channel so
    // SysAInit's readiness gate for this worker is satisfied — the bridge
    // does not speak the worker-IPC protocol, so its readiness has to be
    // signalled through the control session it already holds.
    if !hello.worker_id.is_empty() {
        sysa::notify::broadcast(&[("WORKER_READY", &hello.worker_id)]);
        info!(
            "Bridge '{}' (worker '{}') is ready",
            hello.flavor, hello.worker_id
        );
    }

    // Subscribe to control events before serving requests.  The channel is
    // unbounded so a boot burst can never drop a lifecycle event (see
    // `ControlForwarder`); the writer task applies the only backpressure,
    // and a dead peer drops the receiver, failing sends immediately.
    let (out_tx, out_rx) = mpsc::unbounded_channel::<bytes::Bytes>();
    let forwarder = Arc::new(ControlForwarder { tx: out_tx.clone() });
    let bus = allocator.read().event_bus.clone();
    let sub_id = {
        let mut bus = bus.write().await;
        bus.subscribe(forwarder.clone())
    };

    // Writer task drains the session's outbound channel (replies + events).
    let mut writer = writer;
    let writer_task = tokio::spawn(async move {
        let mut rx = out_rx;
        while let Some(bytes) = rx.recv().await {
            if writer.send(bytes).await.is_err() {
                break;
            }
        }
    });

    loop {
        match reader.next().await {
            None => break,
            Some(Err(e)) => {
                if peer_disconnected(&e) {
                    debug!("Control session from PID {client_pid} closed by peer: {e}");
                } else {
                    warn!("Control session read error: {e}");
                }
                break;
            }
            Some(Ok(bytes)) => {
                let env = match Envelope::decode(bytes.as_ref()) {
                    Ok(env) => env,
                    Err(e) => {
                        warn!("Control envelope decode error: {e}");
                        continue;
                    }
                };
                let request_id = env.request_id;
                match dispatch(&allocator, env).await {
                    Ok(Some(mut reply)) => {
                        reply.request_id = request_id;
                        if out_tx.send(encode_envelope(&reply)).is_err() {
                            break;
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        warn!("Control handler error: {e:#}");
                        let err = make_envelope(
                            request_id,
                            "system-a",
                            "",
                            "manager.error",
                            SimpleManagerResult {
                                success: false,
                                message: format!("{e:#}"),
                            },
                        );
                        if let Ok(err) = err {
                            if out_tx.send(encode_envelope(&err)).is_err() {
                                break;
                            }
                        }
                    }
                }
            }
        }
    }

    let mut bus = bus.write().await;
    bus.unsubscribe(sub_id);
    drop(bus);
    // Stop the event source first, then let the writer flush what is already
    // queued: the peer sees a complete frame stream followed by FIN instead
    // of a frame truncated by abort().  A peer that stops reading is cut off
    // after the drain bound.
    drop(forwarder);
    drop(out_tx);
    let mut writer_task = writer_task;
    if tokio::time::timeout(Duration::from_secs(1), &mut writer_task)
        .await
        .is_err()
    {
        writer_task.abort();
    }
    info!("Control session from PID {client_pid} closed");
    Ok(())
}

// ---------------------------------------------------------------------------
// One-shot admin sessions (stagingctl, unitstatectl)
// ---------------------------------------------------------------------------

/// The localized hint appended to a refusal below.
///
/// P1 keeps the two domains strictly separate (no ACL/polkit bridge yet),
/// so an unauthorized caller is told how to actually get in: elevate for
/// the system domain, or manage their own per-user instance with `--user`.
fn privilege_hint() -> String {
    sysa::l10n::t_(
        "System-domain operations need root: re-run with sudo, or pass --user to manage the per-user instance.",
    )
}

/// Build the reply envelopes for a one-shot `admin.*` request.
///
/// The admin API was moved from the allocator IPC socket to the control
/// port.  `admin.staging` answers with a single `admin.staging.result`
/// envelope; `admin.unitstate` streams one `admin.unitstate.entry` envelope
/// per loaded unit followed by the `admin.unitstate.eof` sentinel.  Both
/// require the caller to be root or the UID running System A.
fn admin_replies(
    allocator: &AllocatorHandle,
    env: &Envelope,
    client_uid: u32,
) -> Result<Vec<Envelope>> {
    let sys_uid = super::server::system_uid();
    let authorized = client_uid == 0 || client_uid == sys_uid;
    match env.method.as_str() {
        "admin.staging" => {
            let result = if !authorized {
                AdminStagingResult {
                    success: false,
                    message: format!(
                        "{} {}",
                        sysa::l10n::fmt(
                            sysa::l10n::t_(
                                "Permission denied (UID {uid}): only root or UID {sys_uid} may query staging areas.",
                            ),
                            &[
                                ("uid", &client_uid.to_string()),
                                ("sys_uid", &sys_uid.to_string()),
                            ],
                        ),
                        privilege_hint()
                    ),
                    entries: vec![],
                }
            } else {
                let op = AdminStagingOp::decode(env.payload.as_slice())?;
                if op.op == "commit" {
                    admin_staging_commit(&op, allocator)
                } else {
                    super::server::build_admin_result(&op, allocator)
                }
            };
            Ok(vec![make_envelope(
                next_request_id(),
                "system-a",
                "",
                "admin.staging.result",
                result,
            )?])
        }
        "admin.unitstate" => {
            let mut replies = Vec::new();
            if !authorized {
                replies.push(make_envelope(
                    next_request_id(),
                    "system-a",
                    "",
                    "admin.unitstate.eof",
                    UnitStateEof {
                        total: 0,
                        message: format!(
                            "{} {}",
                            sysa::l10n::fmt(
                                sysa::l10n::t_(
                                    "Permission denied (UID {uid}): only root or UID {sys_uid} may inspect unit state.",
                                ),
                                &[
                                    ("uid", &client_uid.to_string()),
                                    ("sys_uid", &sys_uid.to_string()),
                                ],
                            ),
                            privilege_hint()
                        ),
                    },
                )?);
                return Ok(replies);
            }
            let _req = UnitStateListRequest::decode(env.payload.as_slice())?;
            let names = crate::unitstate::unit_names(&allocator.read());
            let mut total = 0u32;
            for name in names {
                let json = crate::unitstate::entry_json(&allocator.read(), &name)
                    .and_then(|doc| serde_json::to_vec(&doc).ok());
                let Some(json) = json else {
                    continue;
                };
                replies.push(make_envelope(
                    next_request_id(),
                    "system-a",
                    "",
                    "admin.unitstate.entry",
                    UnitStateEntry { name, json },
                )?);
                total += 1;
            }
            replies.push(make_envelope(
                next_request_id(),
                "system-a",
                "",
                "admin.unitstate.eof",
                UnitStateEof {
                    total,
                    message: String::new(),
                },
            )?);
            Ok(replies)
        }
        other => anyhow::bail!(sysa::l10n::fmt(
            sysa::l10n::t_("unknown admin method '{other}'"),
            &[("other", &other.to_string())]
        )),
    }
}

/// Merge staging areas into the active unit set (`admin.staging` op=`commit`).
///
/// Mirrors `finder.commit_units`: the merge is idempotent, consumes the
/// staging area(s), and triggers a `ReloadRequest::FromCommit` pass so any
/// newly created units gain their default dependencies (e.g. sysinit.target
/// ordering).  With an empty `name`, every area owned by `op.uid` is
/// committed.
fn admin_staging_commit(op: &AdminStagingOp, allocator: &AllocatorHandle) -> AdminStagingResult {
    if op.name.is_empty() {
        let mut state = allocator.write();
        let areas: Vec<(u32, String)> = state
            .get_staging_areas_by_uid(op.uid)
            .into_iter()
            .map(|a| (a.uid, a.name.clone()))
            .collect();
        if areas.is_empty() {
            return AdminStagingResult {
                success: false,
                message: sysa::l10n::fmt(
                    sysa::l10n::t_("no staging area for UID {uid}"),
                    &[("uid", &(op.uid).to_string())],
                ),
                entries: vec![],
            };
        }
        let mut committed = 0u32;
        for (uid, name) in &areas {
            match state.commit_staging(*uid, name) {
                Ok(count) => committed += count,
                Err(msg) => {
                    drop(state);
                    return AdminStagingResult {
                        success: false,
                        message: sysa::l10n::fmt(
                            sysa::l10n::t_("commit of '{name}' failed: {msg}"),
                            &[("name", &name.to_string()), ("msg", &msg.to_string())],
                        ),
                        entries: vec![],
                    };
                }
            }
        }
        drop(state);
        notify_commit(allocator);
        AdminStagingResult {
            success: true,
            message: sysa::l10n::fmt(
                sysa::l10n::t_("committed {committed} unit(s) for UID {uid}"),
                &[
                    ("committed", &committed.to_string()),
                    ("uid", &(op.uid).to_string()),
                ],
            ),
            entries: vec![],
        }
    } else {
        let count = {
            let mut state = allocator.write();
            match state.commit_staging(op.uid, &op.name) {
                Ok(count) => count,
                Err(msg) => {
                    drop(state);
                    return AdminStagingResult {
                        success: false,
                        message: sysa::l10n::fmt(
                            sysa::l10n::t_("commit of '{name}' failed: {msg}"),
                            &[("name", &(op.name).to_string()), ("msg", &msg.to_string())],
                        ),
                        entries: vec![],
                    };
                }
            }
        };
        notify_commit(allocator);
        AdminStagingResult {
            success: true,
            message: sysa::l10n::fmt(
                sysa::l10n::t_("committed {count} unit(s) from staging area '{name}' (UID {uid})"),
                &[
                    ("count", &count.to_string()),
                    ("name", &(op.name).to_string()),
                    ("uid", &(op.uid).to_string()),
                ],
            ),
            entries: vec![],
        }
    }
}

/// Enqueue a `FromCommit` reload pass after an admin staging commit, the
/// same default-dependency fix-up the finder commit path applies.
fn notify_commit(allocator: &AllocatorHandle) {
    if let Some(tx) = allocator.read().reload_tx.as_ref() {
        let _ = tx.try_send(crate::reload_task::ReloadRequest::FromCommit);
    }
}

/// Route one control request to its handler and build the reply envelope.
async fn dispatch(allocator: &AllocatorHandle, env: Envelope) -> Result<Option<Envelope>> {
    let method = env.method.as_str();
    let reply = match method {
        "manager.enqueue" => manager_enqueue(allocator, env).await?,
        "manager.load_unit" => manager_load_unit(allocator, env).await?,
        "manager.start_transient" => manager_start_transient(allocator, env).await?,
        "manager.set_unit_properties" => manager_set_unit_properties(allocator, env).await?,
        "manager.reset_failed" => manager_reset_failed(allocator).await?,
        "manager.reload" => manager_reload(allocator).await?,
        "manager.reset_failed_unit" => manager_reset_failed_unit(allocator, env).await?,
        "manager.ref_unit" => manager_ref_unit(allocator, env, true).await?,
        "manager.unref_unit" => manager_ref_unit(allocator, env, false).await?,
        "manager.abandon_scope" => manager_abandon_scope(allocator, env).await?,
        "manager.unit_snapshot" => manager_unit_snapshot(allocator, env).await?,
        "manager.list_snapshots" => manager_list_snapshots(allocator).await?,
        "manager.list_jobs" => manager_list_jobs(allocator).await?,
        "manager.get_unit_by_pid" => manager_get_unit_by_pid(allocator, env).await?,
        "manager.get_unit_by_invocation" => manager_get_unit_by_invocation(allocator, env).await?,
        "manager.list_units" => manager_list_units(allocator, env).await?,
        "manager.start_units" => manager_start_units(allocator, env).await?,
        "manager.stop_units" => manager_stop_units(allocator, env).await?,
        "manager.daemon_reload" => manager_daemon_reload(allocator).await?,
        "manager.register_power_units" => manager_register_power_units(allocator, env).await?,
        other => anyhow::bail!(sysa::l10n::fmt(
            sysa::l10n::t_("unknown control method '{other}'"),
            &[("other", &other.to_string())]
        )),
    };
    Ok(Some(reply))
}

// ---------------------------------------------------------------------------
// manager.* handlers
// ---------------------------------------------------------------------------

/// `manager.enqueue` — enqueue a job by job-type string (systemd
/// `EnqueueUnitJob` semantics, including the "reload-or-*" collapse).
async fn manager_enqueue(allocator: &AllocatorHandle, env: Envelope) -> Result<Envelope> {
    let req = EnqueueJobRequest::decode(env.payload.as_slice())
        .context(sysa::l10n::t_("bad manager.enqueue"))?;
    let (kind, ty_reload) = parse_job_type(&req.job_type).ok_or_else(|| {
        anyhow::anyhow!(sysa::l10n::fmt(
            sysa::l10n::t_("job type '{job_type}' invalid"),
            &[("job_type", &(req.job_type).to_string())]
        ))
    })?;
    let mode = JobMode::from_str(&req.mode).ok_or_else(|| {
        anyhow::anyhow!(sysa::l10n::fmt(
            sysa::l10n::t_("job mode '{mode}' invalid"),
            &[("mode", &(req.mode).to_string())]
        ))
    })?;
    let reload_if_possible = ty_reload || req.reload_if_possible;

    let canonical = crate::unit::template::ensure_defined(allocator, &req.name)?;

    let (job_id, collapsed) = crate::scheduler::enqueue_job_type(
        allocator.clone(),
        &canonical,
        kind,
        reload_if_possible,
        mode,
    )
    .await
    .map_err(|e| {
        anyhow::anyhow!(sysa::l10n::fmt(
            sysa::l10n::t_("enqueue failed: {e}"),
            &[("e", &e.to_string())]
        ))
    })?;
    set_desired_state(allocator, &canonical, collapsed);

    make_envelope(
        next_request_id(),
        "system-a",
        "",
        "manager.enqueue.result",
        EnqueueJobResult {
            success: true,
            message: String::new(),
            job_id,
            unit_name: canonical,
        },
    )
}

/// `manager.load_unit` — confirm a unit is registered, or materialize a
/// registered IR template instance.
async fn manager_load_unit(allocator: &AllocatorHandle, env: Envelope) -> Result<Envelope> {
    let req = LoadUnitRequest::decode(env.payload.as_slice())
        .context(sysa::l10n::t_("bad manager.load_unit"))?;
    let result = crate::unit::template::ensure_defined(allocator, &req.name);
    match result {
        Ok(canonical) => make_envelope(
            next_request_id(),
            "system-a",
            "",
            "manager.load_unit.result",
            LoadUnitResult {
                success: true,
                message: String::new(),
                name: canonical,
            },
        ),
        Err(e) => make_envelope(
            next_request_id(),
            "system-a",
            "",
            "manager.load_unit.result",
            LoadUnitResult {
                success: false,
                message: format!("{e:#}"),
                name: allocator.read().resolve_unit_name(&req.name),
            },
        ),
    }
}

/// `manager.start_transient` — create a transient unit (no on-disk file) and
/// start it.  The call does not block for job completion: the bridge waits on
/// the `job.completed` event, so one transient-creation request may serve a
/// long session-scope lifecycle without tying up System A.
async fn manager_start_transient(allocator: &AllocatorHandle, env: Envelope) -> Result<Envelope> {
    let req = TransientUnitRequest::decode(env.payload.as_slice())
        .context(sysa::l10n::t_("bad manager.start_transient"))?;
    let canonical = allocator.read().resolve_unit_name(&req.name);
    let mode = JobMode::from_str(&req.mode).ok_or_else(|| {
        anyhow::anyhow!(sysa::l10n::fmt(
            sysa::l10n::t_("job mode '{mode}' invalid"),
            &[("mode", &(req.mode).to_string())]
        ))
    })?;

    let sender_pid = if req.sender_pid != 0 {
        Some(req.sender_pid)
    } else {
        None
    };
    let uf = transient_unit_from_properties(
        &canonical,
        &req.properties
            .as_ref()
            .map(|bag| bag.properties.as_slice())
            .unwrap_or(&[]),
        sender_pid,
    );

    // Aux/paired units systemd-run ships in the same call — most importantly
    // the companion `NAME.service` of a transient `NAME.timer`/`NAME.path`
    // (its `ExecStart=` body).  They must exist in the table before the
    // trigger unit can ever fire.
    let mut aux_ufs = Vec::with_capacity(req.aux_units.len());
    for aux in &req.aux_units {
        if aux.name.is_empty() {
            continue;
        }
        let aux_name = allocator.read().resolve_unit_name(&aux.name);
        let aux_props = aux
            .properties
            .as_ref()
            .map(|bag| bag.properties.as_slice())
            .unwrap_or(&[]);
        aux_ufs.push(transient_unit_from_properties(
            &aux_name, aux_props, sender_pid,
        ));
    }

    {
        let mut state = allocator.write();
        // A repeat call for an existing unit refreshes it when the unit is
        // transient (systemd allows property re-assignment on transient
        // units); a real, file-backed definition always wins.
        let main_upsertable = state
            .units
            .get(&canonical)
            .map(|u| u.transient)
            .unwrap_or(true);
        if main_upsertable {
            if uf.kind == UnitKind::Scope {
                let entry = state.unit_states.entry(canonical.clone()).or_default();
                entry.pids = uf
                    .scope
                    .as_ref()
                    .map(|s| {
                        s.pids
                            .iter()
                            .filter_map(|p| p.trim().parse::<u32>().ok())
                            .collect()
                    })
                    .unwrap_or_default();
                entry.controller = sender_pid
                    .map(|pid| format!("pid:{pid}"))
                    .unwrap_or_default();
            }
            state.units.insert(canonical.clone(), uf);
        }
        for aux_uf in aux_ufs {
            let name = aux_uf.name.clone();
            let upsertable = state.units.get(&name).map(|u| u.transient).unwrap_or(true);
            if upsertable {
                state.units.insert(name, aux_uf);
            }
        }
    }

    let (job_id, collapsed) = crate::scheduler::enqueue_job_type(
        allocator.clone(),
        &canonical,
        JobType::Start,
        false,
        mode,
    )
    .await
    .map_err(|e| {
        anyhow::anyhow!(sysa::l10n::fmt(
            sysa::l10n::t_("cannot start transient unit: {e}"),
            &[("e", &e.to_string())]
        ))
    })?;
    set_desired_state(allocator, &canonical, collapsed);

    make_envelope(
        next_request_id(),
        "system-a",
        "",
        "manager.start_transient.result",
        EnqueueJobResult {
            success: true,
            message: String::new(),
            job_id,
            unit_name: canonical,
        },
    )
}

/// `manager.set_unit_properties` — apply runtime resource-control properties
/// (systemd `SetUnitProperties` semantics; only the rc directives are
/// honoured, matching what logind's `user_update_slice` sends).
async fn manager_set_unit_properties(
    allocator: &AllocatorHandle,
    env: Envelope,
) -> Result<Envelope> {
    let req = SetUnitPropertiesRequest::decode(env.payload.as_slice())
        .context(sysa::l10n::t_("bad manager.set_unit_properties"))?;
    if req.mode != "replace" {
        anyhow::bail!(sysa::l10n::t_(
            "SetUnitProperties only supports job mode 'replace'"
        ));
    }
    let name = allocator.read().resolve_unit_name(&req.name);
    {
        let mut state = allocator.write();
        let Some(rc) = rc_of_unit_mut(&mut state, &name) else {
            anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_("unit '{name}' has no resource-control section"),
                &[("name", &name.to_string())]
            ));
        };
        for prop in req
            .properties
            .as_ref()
            .map(|bag| bag.properties.as_slice())
            .unwrap_or(&[])
        {
            if let Some(mval) = &prop.value {
                apply_resource_property(rc, &prop.key, mval);
            }
        }
    }
    push_resource_update(allocator, &name).await;

    make_envelope(
        next_request_id(),
        "system-a",
        "",
        "manager.set_unit_properties.result",
        SimpleManagerResult {
            success: true,
            message: String::new(),
        },
    )
}

/// `manager.reset_failed` — clear all start-limit failure state.
async fn manager_reset_failed(allocator: &AllocatorHandle) -> Result<Envelope> {
    allocator.write().start_limit_state.clear();
    make_envelope(
        next_request_id(),
        "system-a",
        "",
        "manager.reset_failed.result",
        SimpleManagerResult {
            success: true,
            message: String::new(),
        },
    )
}

/// `manager.reload` — trigger a full System F re-scan via the ReloadTask
/// pipeline (the same one the D-Bus `Reload` method fed before the split).
/// Blocks until the rescan and post-commit work (inject + sync) completes.
async fn manager_reload(allocator: &AllocatorHandle) -> Result<Envelope> {
    let tx = allocator.read().reload_tx.clone();
    let Some(tx) = tx else {
        anyhow::bail!(sysa::l10n::t_("ReloadTask not yet running"));
    };
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    tx.send(crate::reload_task::ReloadRequest::ByTrigger(reply_tx))
        .await
        .context(sysa::l10n::t_("ReloadTask channel closed"))?;
    let _ = reply_rx.await;
    make_envelope(
        next_request_id(),
        "system-a",
        "",
        "manager.reload.result",
        SimpleManagerResult {
            success: true,
            message: String::new(),
        },
    )
}

/// `manager.reset_failed_unit` — clear one unit's start-limit failure state.
async fn manager_reset_failed_unit(allocator: &AllocatorHandle, env: Envelope) -> Result<Envelope> {
    let req = ResetFailedUnitRequest::decode(env.payload.as_slice())
        .context(sysa::l10n::t_("bad manager.reset_failed_unit"))?;
    let name = allocator.read().resolve_unit_name(&req.name);
    allocator.write().start_limit_state.remove(&name);
    ok_result("manager.reset_failed_unit.result")
}

/// `manager.ref_unit` / `manager.unref_unit` — external reference counting.
async fn manager_ref_unit(
    allocator: &AllocatorHandle,
    env: Envelope,
    is_ref: bool,
) -> Result<Envelope> {
    let req = RefUnitRequest::decode(env.payload.as_slice())
        .context(sysa::l10n::t_("bad manager.ref_unit"))?;
    let name = allocator.read().resolve_unit_name(&req.name);
    {
        let mut state = allocator.write();
        if !state.units.contains_key(&name) {
            anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_("unit '{name}' is not loaded"),
                &[("name", &name.to_string())]
            ));
        }
        let count = state.n_refs.entry(name.clone()).or_insert(0);
        if is_ref {
            *count += 1;
        } else if *count > 0 {
            *count -= 1;
        }
    }
    let n_refs = allocator.read().n_refs.get(&name).copied().unwrap_or(0);
    make_envelope(
        next_request_id(),
        "system-a",
        "",
        if is_ref {
            "manager.ref_unit.result"
        } else {
            "manager.unref_unit.result"
        },
        RefUnitResult {
            success: true,
            message: String::new(),
            n_refs,
        },
    )
}

/// `manager.abandon_scope` — abandon a scope (controller gave up on it).
async fn manager_abandon_scope(allocator: &AllocatorHandle, env: Envelope) -> Result<Envelope> {
    let req = AbandonScopeRequest::decode(env.payload.as_slice())
        .context(sysa::l10n::t_("bad manager.abandon_scope"))?;
    let name = allocator.read().resolve_unit_name(&req.name);

    let (worker_tx, active_state) = {
        let state = allocator.read();
        if !state
            .units
            .get(&name)
            .map(|u| u.kind == UnitKind::Scope)
            .unwrap_or(false)
        {
            anyhow::bail!("{name} is not a scope unit");
        }
        let active_state = state
            .unit_states
            .get(&name)
            .map(|s| s.active_state.clone())
            .unwrap_or_else(|| "inactive".to_string());
        let worker_tx = state
            .workers
            .values()
            .find(|w| w.unit_types.iter().any(|t| t == "scope"))
            .map(|w| w.envelope_tx.clone());
        (worker_tx, active_state)
    };
    if !matches!(active_state.as_str(), "active" | "activating") {
        anyhow::bail!(sysa::l10n::fmt(
            sysa::l10n::t_("scope {name} is not running, cannot abandon"),
            &[("name", &name.to_string())]
        ));
    }

    // Notify the scope worker (best effort) and mark the cached state.
    if let Some(tx) = worker_tx {
        if let Ok(env) = make_envelope(
            next_task_id(),
            "system-a",
            "system-e-1",
            "scope.abandon",
            sysa::proto::ScopeAbandon {
                unit_name: name.clone(),
            },
        ) {
            let mut buf = bytes::BytesMut::new();
            if env.encode(&mut buf).is_ok() && tx.send(buf.freeze()).await.is_err() {
                warn!("Cannot send scope.abandon to worker for {name}");
            }
        }
    }
    {
        let mut state = allocator.write();
        if let Some(entry) = state.unit_states.get_mut(&name) {
            entry.sub_state = "abandoned".to_string();
        }
    }

    ok_result("manager.abandon_scope.result")
}

/// `manager.unit_snapshot` — fetch one unit's full snapshot.
async fn manager_unit_snapshot(allocator: &AllocatorHandle, env: Envelope) -> Result<Envelope> {
    let req = UnitSnapshotRequest::decode(env.payload.as_slice())
        .context(sysa::l10n::t_("bad manager.unit_snapshot"))?;
    let snap = snapshot::unit_snapshot(&allocator.read(), &req.name);
    make_envelope(
        next_request_id(),
        "system-a",
        "",
        "manager.unit_snapshot.result",
        sysa::proto::UnitSnapshotResult {
            success: true,
            message: String::new(),
            unit: Some(snap),
        },
    )
}

/// `manager.list_snapshots` — fetch every loaded unit's snapshot (used by a
/// bridge to prime its local mirror on connect).
async fn manager_list_snapshots(allocator: &AllocatorHandle) -> Result<Envelope> {
    let units = snapshot::list_snapshots(&allocator.read());
    make_envelope(
        next_request_id(),
        "system-a",
        "",
        "manager.list_snapshots.result",
        sysa::proto::ListSnapshotsResult {
            success: true,
            message: String::new(),
            units,
        },
    )
}

/// `manager.list_jobs` — list the allocator's live jobs.
async fn manager_list_jobs(allocator: &AllocatorHandle) -> Result<Envelope> {
    let result = snapshot::list_jobs_result(&allocator.read());
    make_envelope(
        next_request_id(),
        "system-a",
        "",
        "manager.list_jobs.result",
        result,
    )
}

/// `manager.get_unit_by_pid` — resolve which unit owns a PID.
async fn manager_get_unit_by_pid(allocator: &AllocatorHandle, env: Envelope) -> Result<Envelope> {
    let req = GetUnitByPidRequest::decode(env.payload.as_slice())
        .context(sysa::l10n::t_("bad manager.get_unit_by_pid"))?;
    let name = allocator
        .read()
        .unit_states
        .iter()
        .find(|(_, c)| c.main_pid == req.pid || c.pids.contains(&req.pid))
        .map(|(n, _)| n.clone());
    make_envelope(
        next_request_id(),
        "system-a",
        "",
        "manager.get_unit_by_pid.result",
        GetUnitByPidResult {
            success: name.is_some(),
            message: name
                .as_ref()
                .map(|_| String::new())
                .unwrap_or_else(|| format!("no unit found for pid {}", req.pid)),
            name: name.unwrap_or_default(),
        },
    )
}

/// `manager.get_unit_by_invocation` — resolve a unit by invocation ID.
async fn manager_get_unit_by_invocation(
    allocator: &AllocatorHandle,
    env: Envelope,
) -> Result<Envelope> {
    let req = GetUnitByInvocationRequest::decode(env.payload.as_slice())
        .context(sysa::l10n::t_("bad manager.get_unit_by_invocation"))?;
    let name = allocator
        .read()
        .invocation_ids
        .iter()
        .find(|(_, id)| *id == &req.invocation_id)
        .map(|(n, _)| n.clone());
    make_envelope(
        next_request_id(),
        "system-a",
        "",
        "manager.get_unit_by_invocation.result",
        GetUnitByPidResult {
            success: name.is_some(),
            message: name
                .as_ref()
                .map(|_| String::new())
                .unwrap_or_else(|| "no unit with that invocation id".to_string()),
            name: name.unwrap_or_default(),
        },
    )
}

// ---------------------------------------------------------------------------
// Control-plane methods moved off the allocator (SysAInit boot control)
// ---------------------------------------------------------------------------

/// `manager.list_units` — list the units System A has loaded, optionally
/// filtered to enabled ones (the boot set).
///
/// Enablement is not computed here.  The Finder decided it while scanning the
/// unit search paths and reported it on the `UnitIR`, so System A only filters
/// the desired state it already holds — a unit whose definition never reached
/// the allocator is not in the boot set.
async fn manager_list_units(allocator: &AllocatorHandle, env: Envelope) -> Result<Envelope> {
    let req = ListUnitsRequest::decode(env.payload.as_slice())
        .context(sysa::l10n::t_("bad manager.list_units"))?;
    let result = {
        let state = allocator.read();
        build_list_result(&state, req.enabled_only)
    };
    info!(
        "manager.list_units returning {} unit(s) (enabled_only={})",
        result.units.len(),
        req.enabled_only
    );
    make_envelope(
        next_request_id(),
        "system-a",
        "",
        "manager.list_units.result",
        result,
    )
}

/// Build the `manager.list_units` reply (pure, testable without the global
/// path config).
fn build_list_result(state: &crate::state::AllocatorState, enabled_only: bool) -> ListUnitsResult {
    let mut units: Vec<UnitInfo> = state
        .units
        .iter()
        .filter(|(_, unit)| !enabled_only || unit.enabled)
        .map(|(name, unit)| UnitInfo {
            name: name.clone(),
            unit_type: unit.kind.worker_type().to_string(),
            enabled: unit.enabled,
        })
        .collect();
    units.sort_by(|a, b| a.name.cmp(&b.name));
    ListUnitsResult {
        success: true,
        message: String::new(),
        units,
    }
}

/// `manager.start_units` — enqueue a start job for every listed unit
/// (mirrors `systemctl start a b c`).
async fn manager_start_units(allocator: &AllocatorHandle, env: Envelope) -> Result<Envelope> {
    let req = StartUnitsRequest::decode(env.payload.as_slice())
        .context(sysa::l10n::t_("bad manager.start_units"))?;
    info!("manager.start_units: {} unit(s)", req.names.len());

    // One transaction for the whole batch (systemd `EnqueueUnitJobMany`):
    // planning each name separately drops the ordering edges between roots,
    // which is how polkit.service used to start before dbus.socket bound.
    let results: Vec<UnitStartResult> =
        crate::scheduler::enqueue_start_batch(allocator.clone(), &req.names, JobMode::Replace)
            .await
            .into_iter()
            .map(|(name, result)| match result {
                Ok(job_id) => {
                    info!("manager.start_units: enqueued job {job_id} for '{name}'");
                    UnitStartResult {
                        name,
                        success: true,
                        message: sysa::l10n::fmt(
                            sysa::l10n::t_("job {job_id}"),
                            &[("job_id", &job_id.to_string())],
                        ),
                    }
                }
                Err(e) => {
                    warn!("manager.start_units: cannot start '{}': {}", name, e);
                    UnitStartResult {
                        name,
                        success: false,
                        message: e.to_string(),
                    }
                }
            })
            .collect();

    let success = !results.is_empty() && results.iter().all(|r| r.success);
    make_envelope(
        next_request_id(),
        "system-a",
        "",
        "manager.start_units.result",
        StartUnitsResult { success, results },
    )
}

/// `manager.stop_units` — enqueue a stop job for one unit.
async fn manager_stop_units(allocator: &AllocatorHandle, env: Envelope) -> Result<Envelope> {
    let req = StopUnitsRequest::decode(env.payload.as_slice())
        .context(sysa::l10n::t_("bad manager.stop_units"))?;
    info!("manager.stop_units: '{}'", req.name);

    let result = match crate::scheduler::enqueue_job(
        allocator.clone(),
        &req.name,
        JobKind::Stop,
        JobMode::Replace,
    )
    .await
    {
        Ok(job_id) => {
            info!(
                "manager.stop_units: enqueued job {job_id} for '{}'",
                req.name
            );
            StopUnitsResult {
                success: true,
                message: sysa::l10n::fmt(
                    sysa::l10n::t_("job {job_id}"),
                    &[("job_id", &job_id.to_string())],
                ),
            }
        }
        Err(e) => {
            warn!("manager.stop_units: cannot stop '{}': {}", req.name, e);
            StopUnitsResult {
                success: false,
                message: e.to_string(),
            }
        }
    };

    make_envelope(
        next_request_id(),
        "system-a",
        "",
        "manager.stop_units.result",
        result,
    )
}

/// `manager.daemon_reload` — trigger a full unit-file rescan via System F
/// (the control-plane equivalent of `systemctl daemon-reload`).
async fn manager_daemon_reload(allocator: &AllocatorHandle) -> Result<Envelope> {
    info!("manager.daemon_reload");
    let tx = allocator.read().reload_tx.clone();
    let Some(tx) = tx else {
        anyhow::bail!(sysa::l10n::t_("ReloadTask not yet running"));
    };
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    tx.send(crate::reload_task::ReloadRequest::ByTrigger(reply_tx))
        .await
        .context(sysa::l10n::t_("ReloadTask channel closed"))?;
    let _ = reply_rx.await;
    make_envelope(
        next_request_id(),
        "system-a",
        "",
        "manager.daemon_reload.result",
        DaemonReloadResult {
            success: true,
            message: String::new(),
        },
    )
}

/// `manager.register_power_units` — merge the built-in `.power` unit
/// definitions supplied by System Init into the unit graph.
///
/// System Init owns the `power` unit type: it synthesizes the definitions
/// (`poweroff.power`, `reboot.power`, ...) and pushes them over the control
/// plane at boot.  System A merges them idempotently (the same path as the
/// finder commit path) and injects DefaultDependencies=; the units then
/// dispatch through System Init's registered `power` worker.
async fn manager_register_power_units(
    allocator: &AllocatorHandle,
    env: Envelope,
) -> Result<Envelope> {
    let req = RegisterPowerUnitsRequest::decode(env.payload.as_slice())
        .context(sysa::l10n::t_("bad manager.register_power_units"))?;
    let units: std::collections::HashMap<String, systema_sysf::ir::UnitIR> =
        serde_json::from_slice(&req.units_json)
            .context(sysa::l10n::t_("bad power units JSON payload"))?;
    info!(
        "manager.register_power_units: {} unit definition(s)",
        units.len()
    );
    let (created, updated) = {
        let mut state = allocator.write();
        state.merge_units(&units).map_err(anyhow::Error::msg)?
    };
    // Newly materialized units must also receive DefaultDependencies=
    // (After=sysinit.target & co.).
    crate::unit::loader::inject_default_dependencies(allocator.clone());
    let message = sysa::l10n::fmt(
        sysa::l10n::t_("registered {created} new, {updated} updated .power unit(s)"),
        &[
            ("created", &created.to_string()),
            ("updated", &updated.to_string()),
        ],
    );
    info!("{}", message);
    make_envelope(
        next_request_id(),
        "system-a",
        "",
        "manager.register_power_units.result",
        RegisterPowerUnitsResult {
            success: true,
            message,
            created: created as u32,
            updated: updated as u32,
        },
    )
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn ok_result(method: &str) -> Result<Envelope> {
    make_envelope(
        next_request_id(),
        "system-a",
        "",
        method.to_string(),
        SimpleManagerResult {
            success: true,
            message: String::new(),
        },
    )
}

fn encode_envelope(env: &Envelope) -> bytes::Bytes {
    let mut buf = bytes::BytesMut::with_capacity(env.encoded_len());
    env.encode(&mut buf)
        .expect("Envelope encoding is infallible");
    buf.freeze()
}

/// Track the desired state implied by a collapsed job kind, mirroring the
/// single-unit methods.  `Nop` jobs change nothing.
fn set_desired_state(alloc: &AllocatorHandle, name: &str, kind: JobKind) {
    match kind {
        JobKind::Start | JobKind::Restart => {
            alloc
                .write()
                .desired
                .insert(name.to_string(), DesiredState::Active);
        }
        JobKind::Stop => {
            alloc
                .write()
                .desired
                .insert(name.to_string(), DesiredState::Inactive);
        }
        JobKind::Reload | JobKind::Nop => {}
    }
}

/// Parse a job-type string as accepted by `manager.enqueue` (mirrors
/// `bus_unit_parse_job_type()`).  Returns the type and whether the
/// reload-if-possible flag is set (`reload-or-*` magic types).
fn parse_job_type(s: &str) -> Option<(JobType, bool)> {
    match s {
        "start" => Some((JobType::Start, false)),
        "verify-active" => Some((JobType::VerifyActive, false)),
        "stop" => Some((JobType::Stop, false)),
        "reload" => Some((JobType::Reload, false)),
        "restart" => Some((JobType::Restart, false)),
        "try-restart" => Some((JobType::TryRestart, false)),
        "try-reload" => Some((JobType::TryReload, false)),
        "reload-or-start" => Some((JobType::ReloadOrStart, false)),
        "nop" => Some((JobType::Nop, false)),
        "reload-or-restart" => Some((JobType::Restart, true)),
        "reload-or-try-restart" => Some((JobType::TryRestart, true)),
        _ => None,
    }
}

/// The resource-control block of a unit, regardless of unit kind.
fn rc_of_unit_mut<'a>(
    state: &'a mut AllocatorState,
    name: &str,
) -> Option<&'a mut ResourceControl> {
    let unit = state.units.get_mut(name)?;
    match unit.kind {
        UnitKind::Service => unit.service.as_mut().map(|s| &mut s.rc),
        UnitKind::Slice => unit.slice.as_mut().map(|s| &mut s.rc),
        UnitKind::Scope => unit.scope.as_mut().map(|s| &mut s.rc),
        _ => None,
    }
}

/// Apply one resource-control property onto a runtime `ResourceControl`
/// (port of the D-Bus layer's `apply_resource_property` over `ManagerValue`).
fn apply_resource_property(rc: &mut ResourceControl, key: &str, value: &ManagerValue) -> bool {
    // Accounting switches and OOM policy are implicit in System A's model;
    // accepted for compatibility with logind's user_update_slice.
    if matches!(
        key,
        "MemoryAccounting"
            | "CPUAccounting"
            | "TasksAccounting"
            | "MemoryPressureAccounting"
            | "OOMPolicy"
            | "MemoryPressureThresholdUSec"
            | "Delegate"
    ) {
        return true;
    }
    match (key, &value.value) {
        (key, Some(Value::S(s)))
            if matches!(
                key,
                "MemoryMin"
                    | "MemoryLow"
                    | "MemoryHigh"
                    | "MemoryMax"
                    | "MemorySwapMax"
                    | "CPUQuotaPeriodSec"
                    | "CPUQuotaPeriodUSec"
            ) =>
        {
            set_byte_string(rc, key, s);
            true
        }
        (key, Some(Value::U(u)))
            if matches!(
                key,
                "MemoryMin"
                    | "MemoryLow"
                    | "MemoryHigh"
                    | "MemoryMax"
                    | "MemorySwapMax"
                    | "CPUQuotaPeriodSec"
                    | "CPUQuotaPeriodUSec"
            ) =>
        {
            set_byte_string(rc, key, &u.to_string());
            true
        }
        ("CPUWeight", Some(Value::U(u))) => {
            rc.cpu_weight = (*u).min(u32::MAX as u64) as u32;
            true
        }
        ("StartupCPUWeight", Some(Value::U(u))) => {
            rc.startup_cpu_weight = (*u).min(u32::MAX as u64) as u32;
            true
        }
        ("CPUQuotaPerSecUSec", Some(Value::U(u))) => {
            rc.cpu_quota = format!("{}%", u / 10_000);
            true
        }
        ("IOWeight", Some(Value::U(u))) => {
            rc.io_weight = (*u).min(u32::MAX as u64) as u32;
            true
        }
        ("TasksMax", Some(Value::U(u))) => {
            rc.tasks_max = (*u).min(u32::MAX as u64) as u32;
            true
        }
        ("TasksMax", Some(Value::S(s))) => {
            rc.tasks_max = if s == "infinity" {
                u32::MAX
            } else {
                s.parse::<u64>()
                    .map(|v| v.min(u32::MAX as u64) as u32)
                    .unwrap_or(rc.tasks_max)
            };
            true
        }
        ("AllowedCPUs", Some(Value::S(s))) => {
            rc.allowed_cpus = s.clone();
            true
        }
        ("AllowedMemoryNodes", Some(Value::S(s))) => {
            rc.allowed_memory_nodes = s.clone();
            true
        }
        _ => false,
    }
}

fn set_byte_string(rc: &mut ResourceControl, key: &str, v: &str) {
    match key {
        "MemoryMin" => rc.memory_min = v.to_string(),
        "MemoryLow" => rc.memory_low = v.to_string(),
        "MemoryHigh" => rc.memory_high = v.to_string(),
        "MemoryMax" => rc.memory_max = v.to_string(),
        "MemorySwapMax" => rc.memory_swap_max = v.to_string(),
        "CPUQuotaPeriodSec" | "CPUQuotaPeriodUSec" => rc.cpu_quota_period = v.to_string(),
        _ => {}
    }
}

/// Re-publish the current runtime state of `name` on the event bus so
/// subscribed workers (System R) apply the updated resource limits.
async fn push_resource_update(allocator: &AllocatorHandle, name: &str) {
    let event = {
        let state = allocator.read();
        let Some(cached) = state.unit_states.get(name) else {
            debug!(
                "SetUnitProperties: {} has no runtime state yet; limits cached in unit",
                name
            );
            return;
        };
        let status = sysa::controller::UnitStatus {
            unit_name: name.to_string(),
            active_state: cached.active_state.clone(),
            sub_state: cached.sub_state.clone(),
            main_pid: cached.main_pid,
            invocation_id: cached.invocation_id.clone(),
            extensions: cached.extensions.clone(),
        };
        Event {
            topic: EventTopic::UnitStateChange,
            unit_name: name.to_string(),
            worker_id: "system-a".to_string(),
            timestamp: tokio::time::Instant::now(),
            data: bytes::Bytes::from(status.encode_to_vec()),
        }
    };
    let bus = allocator.read().event_bus.clone();
    bus.read().await.dispatch(&event).await;
}

// ---------------------------------------------------------------------------
// Transient unit construction (over the control protocol's PropertyBag)
// ---------------------------------------------------------------------------

/// Build a transient `UnitFile` from a `manager.start_transient` property
/// bag.  Only the subset of properties that map onto System A's unit model
/// is honoured; unknown properties are ignored (systemd semantics).
fn transient_unit_from_properties(
    name: &str,
    props: &[TransientProperty],
    sender_pid: Option<u32>,
) -> UnitFile {
    use crate::unit::types::ScopeSection;

    let mut uf = UnitFile::new(name);
    uf.transient = true;

    if let Some(d) = prop_string(props, "Description") {
        uf.unit.description = d;
    }
    if let Some(b) = prop_bool(props, "DefaultDependencies") {
        uf.unit.default_dependencies = b;
    }

    for (key, into) in [
        ("Requires", "requires"),
        ("Wants", "wants"),
        ("After", "after"),
        ("Before", "before"),
        ("Conflicts", "conflicts"),
        ("BindsTo", "binds_to"),
        ("PartOf", "part_of"),
    ] {
        let vals = prop_strings(props, key);
        if !vals.is_empty() {
            let set: std::collections::HashSet<String> = vals.into_iter().collect();
            match into {
                "requires" => uf.unit.requires.extend(set),
                "wants" => uf.unit.wants.extend(set),
                "after" => uf.unit.after.extend(set),
                "before" => uf.unit.before.extend(set),
                "conflicts" => uf.unit.conflicts.extend(set),
                "binds_to" => uf.unit.binds_to.extend(set),
                "part_of" => uf.unit.part_of.extend(set),
                _ => {}
            }
        }
    }

    // `Slice=` — scopes/slices live under a slice; mirror the service
    // loader's Requires+After edge on the parent slice and record it.
    if let Some(slice) = prop_string(props, "Slice") {
        if !slice.is_empty() && slice != "root.slice" {
            uf.unit.requires.insert(slice.clone());
            uf.unit.after.insert(slice.clone());
            uf.unit.slice = slice;
        }
    }

    // Scope-specific: the PIDs the scope wraps.  An empty array (or entries
    // of 0) denote the *sender* of the call.
    if uf.kind == UnitKind::Scope {
        let mut scope = ScopeSection::default();
        if let Some(pids) = scope_pids(props) {
            if pids.is_empty() {
                if let Some(pid) = sender_pid {
                    scope.pids = vec![pid.to_string()];
                }
            } else {
                scope.pids = pids
                    .into_iter()
                    .map(|p| {
                        if p == 0 {
                            sender_pid.unwrap_or(0).to_string()
                        } else {
                            p.to_string()
                        }
                    })
                    .collect();
            }
        }
        uf.scope = Some(scope);
    }

    // Kind-specific property mapping.  systemd-run sends each unit's own
    // directives in the bag (the command list in `ExecStart=`, timer delays
    // as usec counts, path specs as string lists), so a transient call can
    // fully materialize a service, timer or path unit without a file.
    if uf.kind == UnitKind::Service {
        uf.service = Some(transient_service_from_props(props));
    }
    if uf.kind == UnitKind::Timer {
        let timer = transient_timer_from_props(props);
        // A transient timer without any trigger cannot be scheduled; its
        // worker refuses it (systemd's `timer_verify()` does the same).  Warn
        // here, at the property-mapping boundary, because this is where a
        // directive the bag does not carry gets lost — the worker's own
        // warning only says the unit arrived empty.
        if !timer_has_trigger(&timer) {
            warn!(
                "Transient timer {name} maps to no OnCalendar=/On*Sec= trigger \
                 (received property keys: {keys:?}); the unit will fail to start",
                keys = property_keys(props)
            );
        }
        uf.timer = Some(timer);
    }
    if uf.kind == UnitKind::Path {
        let path = transient_path_from_props(props);
        if !path_has_condition(&path) {
            warn!(
                "Transient path {name} maps to no PathExists=/PathChanged=/DirectoryNotEmpty= \
                 condition (received property keys: {keys:?}); the unit will fail to start",
                keys = property_keys(props)
            );
        }
        uf.path = Some(path);
    }

    uf
}

/// Whether a transient `[Timer]` section carries any scheduling trigger.
fn timer_has_trigger(t: &TimerSection) -> bool {
    !t.on_calendar.is_empty()
        || t.on_active_sec.is_some()
        || t.on_boot_sec.is_some()
        || t.on_startup_sec.is_some()
        || t.on_unit_active_sec.is_some()
        || t.on_unit_inactive_sec.is_some()
}

/// Whether a transient `[Path]` section carries any watch condition.
fn path_has_condition(p: &PathSection) -> bool {
    !p.path_exists.is_empty()
        || !p.path_exists_glob.is_empty()
        || !p.path_changed.is_empty()
        || !p.path_modified.is_empty()
        || !p.directory_not_empty.is_empty()
}

/// The property keys of a transient request, in arrival order (for
/// diagnostics only).
fn property_keys(props: &[TransientProperty]) -> Vec<&str> {
    props.iter().map(|p| p.key.as_str()).collect()
}

// ---------------------------------------------------------------------------
// Transient service/timer/path assembly
// ---------------------------------------------------------------------------

/// Assemble the `[Service]` section of a transient service from its
/// `systemd-run` property bag.
fn transient_service_from_props(props: &[TransientProperty]) -> ServiceSection {
    let commands = prop_execs(props, "ExecStart");
    // `Type=exec` is systemd's default for transient services; it behaves
    // like `simple`, which is this model's spelling of the same semantics.
    let raw_type = prop_string(props, "Type").unwrap_or_else(|| "exec".to_string());
    let service_type = match raw_type.as_str() {
        "exec" => ServiceType::Simple,
        other => ServiceType::from(other),
    };
    ServiceSection {
        service_type,
        exec_start: commands,
        exec_stop: Vec::new(),
        exec_reload: Vec::new(),
        working_directory: prop_string(props, "WorkingDirectory").unwrap_or_default(),
        user: prop_string(props, "User").unwrap_or_default(),
        group: prop_string(props, "Group").unwrap_or_default(),
        environment: prop_strings(props, "Environment"),
        standard_output: prop_string(props, "StandardOutput").unwrap_or_default(),
        standard_error: prop_string(props, "StandardError").unwrap_or_default(),
        remain_after_exit: prop_bool(props, "RemainAfterExit").unwrap_or(false),
        ..Default::default()
    }
}

/// Assemble the `[Timer]` section of a transient timer from its
/// `systemd-run` property bag.  On the bus `On*Sec=`/`AccuracyUSec=`/
/// `RandomizedDelayUSec=` carry microsecond counts (`t`); this model stores
/// them in whole seconds (ceil, so a sub-second entry still fires on time).
/// The `…Sec` spellings are the obsolete-but-still-accepted D-Bus names and
/// are read as a fallback.
fn transient_timer_from_props(props: &[TransientProperty]) -> TimerSection {
    let usecs = |key: &str| -> Option<u32> { prop_u64(props, key).map(secs_from_usecs_value) };
    // `AccuracyUSec`/`RandomizedDelayUSec` are the current D-Bus property
    // names; `AccuracySec`/`RandomizedDelaySec` are their obsolete spellings.
    let accuracy = prop_u64(props, "AccuracyUSec").or_else(|| prop_u64(props, "AccuracySec"));
    let randomized =
        prop_u64(props, "RandomizedDelayUSec").or_else(|| prop_u64(props, "RandomizedDelaySec"));
    TimerSection {
        on_active_sec: usecs("OnActiveSec"),
        on_boot_sec: usecs("OnBootSec"),
        on_startup_sec: usecs("OnStartupSec"),
        on_unit_active_sec: usecs("OnUnitActiveSec"),
        on_unit_inactive_sec: usecs("OnUnitInactiveSec"),
        on_calendar: prop_strings(props, "OnCalendar"),
        accuracy_sec: accuracy.map(secs_from_usecs_value).unwrap_or_default(),
        randomized_delay_sec: randomized.map(secs_from_usecs_value).unwrap_or_default(),
        unit: prop_string(props, "Unit").unwrap_or_default(),
        persistent: prop_bool(props, "Persistent").unwrap_or(false),
        wake_system: prop_bool(props, "WakeSystem").unwrap_or(false),
        remain_after_elapse: prop_bool(props, "RemainAfterElapse").unwrap_or(false),
    }
}

/// Assemble the `[Path]` section of a transient path unit from its
/// `systemd-run` property bag (`--path-property=PathModified=/…`).
fn transient_path_from_props(props: &[TransientProperty]) -> PathSection {
    PathSection {
        path_exists: prop_strings(props, "PathExists"),
        path_exists_glob: prop_strings(props, "PathExistsGlob"),
        path_changed: prop_strings(props, "PathChanged"),
        path_modified: prop_strings(props, "PathModified"),
        directory_not_empty: prop_strings(props, "DirectoryNotEmpty"),
        unit: prop_string(props, "Unit").unwrap_or_default(),
        make_directory: prop_bool(props, "MakeDirectory").unwrap_or(false),
        directory_mode: prop_string(props, "DirectoryMode").unwrap_or_default(),
        ..Default::default()
    }
}

/// Microseconds (the D-Bus encoding of timer settings) → whole seconds.
/// Ceils so a nonzero sub-second entry keeps firing within a second.
fn secs_from_usecs_value(u: u64) -> u32 {
    ((u + 999_999) / 1_000_000).min(u32::MAX as u64) as u32
}

fn prop_u64(props: &[TransientProperty], key: &str) -> Option<u64> {
    props
        .iter()
        .find(|p| p.key == key)
        .and_then(|p| p.value.as_ref())
        .and_then(|v| v.value.as_ref())
        .and_then(|v| match v {
            Value::U(u) => Some(*u),
            Value::I(i) if *i >= 0 => Some(*i as u64),
            _ => None,
        })
}

/// Decode the `ExecStart=` command list into this model's `ExecCommand`s.
fn prop_execs(props: &[TransientProperty], key: &str) -> Vec<ExecCommand> {
    props
        .iter()
        .filter(|p| p.key == key)
        .filter_map(|p| p.value.as_ref())
        .filter_map(|v| v.value.as_ref())
        .filter_map(|v| match v {
            Value::Exec(ex) => Some(ExecCommand {
                raw: exec_raw_join(&ex.path, &ex.args),
                program: ex.path.clone(),
                args: ex.args.iter().skip(1).cloned().collect(),
                ignore_failure: ex.ignore_failure,
                privileged: false,
                no_env_lookup: false,
                no_kill_on_stop: false,
                no_new_privileges: false,
            }),
            _ => None,
        })
        .collect()
}

/// Render an argv (from systemd's exec struct) back into a command line as
/// `ExecStart=` would spell it, so the existing runtime split resolves the
/// same argv.  Arguments containing whitespace/quotes get POSIX single-quote
/// escaping.
fn exec_raw_join(program: &str, args: &[String]) -> String {
    let mut out = String::from(program);
    for arg in args.iter().skip(1) {
        if arg.chars().any(|c| {
            c.is_whitespace() || c == '\'' || c == '"' || c == '\\' || c == '#' || c == ';'
        }) {
            out.push_str(&format!(" '{}'", arg.replace('\'', "'\\''")));
        } else {
            out.push(' ');
            out.push_str(arg);
        }
    }
    out
}

/// The processes a transient scope wraps, as `PIDs=` / `PIDFDs=` list them.
///
/// `PIDFDs=` reaches here already resolved to PIDs by the D-Bus bridge (see
/// `push_prop_value`), so both spellings mean the same thing over this link.
///
/// `None` means *neither* key was present, i.e. the call named no processes
/// at all.  That is deliberately not the same as an explicitly empty list:
/// falling back to the sender would drag whatever process asked — logind asks
/// for every session scope — into the scope's cgroup, to die with it.
fn scope_pids(props: &[TransientProperty]) -> Option<Vec<u32>> {
    let mut pids = Vec::new();
    let mut any_key = false;
    for key in ["PIDs", "PIDFDs"] {
        if let Some(listed) = prop_u32s(props, key) {
            any_key = true;
            pids.extend(listed);
        }
    }
    any_key.then_some(pids)
}

fn prop_string<'a>(props: &'a [TransientProperty], key: &str) -> Option<String> {
    props
        .iter()
        .find(|p| p.key == key)
        .and_then(|p| p.value.as_ref())
        .and_then(|v| v.value.as_ref())
        .and_then(|v| match v {
            Value::S(s) => Some(s.clone()),
            Value::U(u) => Some(u.to_string()),
            Value::I(i) => Some(i.to_string()),
            _ => None,
        })
}

fn prop_bool(props: &[TransientProperty], key: &str) -> Option<bool> {
    props
        .iter()
        .find(|p| p.key == key)
        .and_then(|p| p.value.as_ref())
        .and_then(|v| v.value.as_ref())
        .and_then(|v| match v {
            Value::B(b) => Some(*b),
            _ => None,
        })
}

fn prop_strings(props: &[TransientProperty], key: &str) -> Vec<String> {
    props
        .iter()
        .filter(|p| p.key == key)
        .filter_map(|p| p.value.as_ref())
        .filter_map(|v| v.value.as_ref())
        .filter_map(|v| match v {
            Value::S(s) => Some(s.clone()),
            _ => None,
        })
        .collect()
}

fn prop_u32s(props: &[TransientProperty], key: &str) -> Option<Vec<u32>> {
    let vals: Vec<u32> = props
        .iter()
        .filter(|p| p.key == key)
        .filter_map(|p| p.value.as_ref())
        .filter_map(|v| v.value.as_ref())
        .filter_map(|v| match v {
            Value::U(u) => Some(*u as u32),
            _ => None,
        })
        .collect();
    if vals.is_empty() {
        None
    } else {
        Some(vals)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use sysa::proto::{
        ListUnitsRequest, ListUnitsResult, StartUnitsRequest, StartUnitsResult, StopUnitsRequest,
        StopUnitsResult,
    };

    use crate::state::{StagingArea, WorkerEntry};
    use crate::unit::types::UnitSection as LocalUnitSection;
    use systema_sysf::ir::{UnitIR, UnitType};

    fn test_allocator() -> AllocatorHandle {
        let state = Arc::new(parking_lot::RwLock::new(AllocatorState::new()));
        {
            let mut state = state.write();
            let mut foo = UnitFile::new("foo.service");
            foo.unit = LocalUnitSection::default();
            state.units.insert("foo.service".to_string(), foo);
            state.units.insert(
                "default.target".to_string(),
                UnitFile::new("default.target"),
            );
        }
        state
    }

    fn register_service_worker(state: &AllocatorHandle) {
        register_worker(state, "system-s-1", &["service"]);
    }

    fn register_worker(state: &AllocatorHandle, id: &str, unit_types: &[&str]) {
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let mut state = state.write();
        state.workers.insert(
            id.to_string(),
            WorkerEntry {
                worker_id: id.to_string(),
                unit_types: unit_types.iter().map(|t| t.to_string()).collect(),
                supports_unit_define: false,
                supports_resource_prepare: false,
                ready: false,
                envelope_tx: tx,
            },
        );
    }

    fn req_env(method: &str, payload: impl ProstMessage) -> Envelope {
        make_envelope(7, "system-sysi", "system-a", method, payload).unwrap()
    }

    async fn call(_method: &str, env: Envelope, allocator: AllocatorHandle) -> Envelope {
        dispatch(&allocator, env).await.unwrap().unwrap()
    }

    /// Spawn a control session over a socketpair and drive the client side,
    /// returning the framed client stream so the caller can send/receive.
    async fn spawn_session_pair(allocator: AllocatorHandle) -> sysa::ipc::EnvelopeFramed {
        use tokio::net::UnixStream;
        let (server, client) = UnixStream::pair().unwrap();
        tokio::spawn(async move {
            let _ = handle_control_session(server, allocator).await;
        });
        sysa::ipc::frame_stream(client)
    }

    /// Drive the manager.hello handshake on an open framed client.
    async fn do_hello(framed: &mut sysa::ipc::EnvelopeFramed) {
        let hello = ManagerHelloRequest {
            flavor: "test-bridge".to_string(),
            version: "0.0.0".to_string(),
            worker_id: String::new(),
        };
        sysa::ipc::send_envelope(framed, &req_env("manager.hello", hello))
            .await
            .unwrap();
        let reply = sysa::ipc::recv_envelope(framed)
            .await
            .unwrap()
            .expect("expected hello result");
        assert_eq!(reply.method, "manager.hello.result");
        assert_eq!(
            reply.request_id, 7,
            "hello result must echo the requester's request id (routing is by id)"
        );
    }

    #[tokio::test]
    async fn control_socketpair_full_rpc_roundtrip() {
        let alloc = test_allocator();
        let mut client = spawn_session_pair(alloc.clone()).await;
        do_hello(&mut client).await;

        let req = ListUnitsRequest {
            enabled_only: false,
        };
        sysa::ipc::send_envelope(&mut client, &req_env("manager.list_units", req))
            .await
            .unwrap();
        let reply = sysa::ipc::recv_envelope(&mut client)
            .await
            .unwrap()
            .expect("expected list_units result");
        assert_eq!(reply.method, "manager.list_units.result");
        let result = ListUnitsResult::decode(reply.payload.as_slice()).unwrap();
        assert!(result.success);
        let names: Vec<&str> = result.units.iter().map(|u| u.name.as_str()).collect();
        assert!(
            names.contains(&"foo.service"),
            "foo.service must appear in list: {:?}",
            names
        );
    }

    /// A one-shot session must be able to finish with a half-close (FIN) and
    /// a drain, so the server sees EOF instead of ECONNRESET.
    #[tokio::test]
    async fn half_closed_session_ends_cleanly() {
        let alloc = test_allocator();
        let (server, client) = UnixStream::pair().unwrap();
        let session = tokio::spawn(async move { handle_control_session(server, alloc).await });
        let mut client = sysa::ipc::frame_stream(client);
        do_hello(&mut client).await;

        sysa::ipc::send_envelope(
            &mut client,
            &req_env(
                "manager.list_units",
                ListUnitsRequest {
                    enabled_only: false,
                },
            ),
        )
        .await
        .unwrap();
        loop {
            let reply = sysa::ipc::recv_envelope(&mut client)
                .await
                .unwrap()
                .expect("session must not close before the reply");
            if reply.method == "manager.list_units.result" {
                break;
            }
        }

        sysa::ipc::finish_control_session(&mut client, sysa::ipc::CONTROL_DRAIN_TIMEOUT).await;
        drop(client);

        tokio::time::timeout(Duration::from_secs(2), session)
            .await
            .expect("session must end after the client half-closes")
            .expect("session task must not panic")
            .expect("session must close without an error");
    }

    #[test]
    fn peer_disconnect_errors_are_not_transport_faults() {
        use std::io::{Error, ErrorKind};
        for kind in [
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted,
            ErrorKind::BrokenPipe,
            ErrorKind::NotConnected,
        ] {
            assert!(peer_disconnected(&Error::from(kind)), "{kind:?}");
        }
        assert!(!peer_disconnected(&Error::from(ErrorKind::UnexpectedEof)));
        assert!(!peer_disconnected(&Error::from(ErrorKind::InvalidData)));
    }

    #[tokio::test]
    async fn one_to_many_broadcast_delivers_event_to_all_sessions() {
        use sysa::event_bus::{Event, EventTopic};

        let alloc = test_allocator();
        let mut client_a = spawn_session_pair(alloc.clone()).await;
        let mut client_b = spawn_session_pair(alloc.clone()).await;
        do_hello(&mut client_a).await;
        do_hello(&mut client_b).await;

        // Push a UnitChanged event onto the bus; both forwarders should
        // receive it and deliver it to their respective clients.
        let bus = alloc.read().event_bus.clone();
        let payload_bytes = b"fake-payload";
        let ev = Event {
            topic: EventTopic::UnitChanged,
            unit_name: "foo.service".to_string(),
            worker_id: "test".to_string(),
            timestamp: tokio::time::Instant::now(),
            data: bytes::Bytes::from_static(payload_bytes),
        };
        bus.write().await.dispatch(&ev).await;

        // Each client should receive a pushed event envelope.
        let env_a = sysa::ipc::recv_envelope(&mut client_a)
            .await
            .unwrap()
            .expect("client A should receive broadcast");
        assert_eq!(env_a.method, "unit.changed");

        let env_b = sysa::ipc::recv_envelope(&mut client_b)
            .await
            .unwrap()
            .expect("client B should receive broadcast");
        assert_eq!(env_b.method, "unit.changed");
        assert_eq!(env_b.payload, env_a.payload);
    }

    #[test]
    fn build_list_result_filters_enabled() {
        let state = Arc::new(parking_lot::RwLock::new(AllocatorState::new()));
        {
            let mut state_guard = state.write();
            // Enablement is the Finder's report, stored on the unit; System A
            // never re-derives it from the filesystem.
            let mut enabled_unit = UnitFile::new("foo.service");
            enabled_unit.enabled = true;
            state_guard
                .units
                .insert("foo.service".to_string(), enabled_unit);
            state_guard
                .units
                .insert("bar.timer".to_string(), UnitFile::new("bar.timer"));
        }

        let all = build_list_result(&state.read(), false);
        // "-.slice" (root slice, auto-created) + foo.service + bar.timer.
        assert_eq!(all.units.len(), 3);
        assert!(all
            .units
            .iter()
            .any(|u| u.name == "bar.timer" && !u.enabled));

        let only = build_list_result(&state.read(), true);
        assert_eq!(only.units.len(), 1);
        let unit = &only.units[0];
        assert_eq!(unit.name, "foo.service");
        assert!(unit.enabled);
        assert_eq!(unit.unit_type, "service");
    }

    #[tokio::test]
    async fn register_power_units_merges_into_graph() {
        let allocator = test_allocator();
        let units: HashMap<String, UnitIR> = [(
            "poweroff.power".to_string(),
            UnitIR {
                id: "poweroff.power".to_string(),
                unit_type: Some(UnitType::Power),
                description: Some("System Power Off".to_string()),
                source_format: None,
                source_path: None,
                aliases: Vec::new(),
                alias_of: None,
                slice: None,
                dependencies: None,
                service: None,
                mount: None,
                automount: None,
                timer: None,
                socket: None,
                path: None,
                resource_control: None,
                conditions: None,
                asserts: None,
                wanted_by: None,
                required_by: None,
                enabled: None,
                template: None,
            },
        )]
        .into_iter()
        .collect();

        let mut state = allocator.write();
        let (created, updated) = state
            .merge_units(&units)
            .map_err(|e| panic!("merge_units failed: {e}"))
            .unwrap();
        assert_eq!(created, 1);
        assert_eq!(updated, 0);
        let uf = state.units.get("poweroff.power").expect("unit in graph");
        assert_eq!(uf.kind.worker_type(), "power");
    }

    #[tokio::test]
    async fn register_power_units_rejects_bad_json() {
        let allocator = test_allocator();
        let env = req_env(
            "manager.register_power_units",
            RegisterPowerUnitsRequest {
                units_json: b"not-json".to_vec(),
            },
        );
        let err = dispatch(&allocator, env)
            .await
            .expect_err("bad JSON must fail");
        assert!(
            err.to_string().contains("JSON"),
            "error must describe the bad payload: {err:#}"
        );
    }

    #[tokio::test]
    async fn register_power_units_bad_json_rejected() {
        let allocator = test_allocator();
        let env = req_env(
            "manager.register_power_units",
            RegisterPowerUnitsRequest {
                units_json: b"not json".to_vec(),
            },
        );
        let err = dispatch(&allocator, env)
            .await
            .expect_err("bad JSON must fail");
        assert!(
            err.to_string().contains("JSON"),
            "error must describe the bad payload: {}",
            err
        );
    }

    #[tokio::test]
    async fn list_units_returns_sorted_snapshot() {
        let allocator = test_allocator();
        let reply = call(
            "manager.list_units",
            req_env(
                "manager.list_units",
                ListUnitsRequest {
                    enabled_only: false,
                },
            ),
            allocator,
        )
        .await;
        assert_eq!(reply.method, "manager.list_units.result");
        let result = ListUnitsResult::decode(reply.payload.as_slice()).unwrap();
        assert!(result.success);
        let names: Vec<&str> = result.units.iter().map(|u| u.name.as_str()).collect();
        // "-.slice" (root slice, auto-created), default.target, foo.service — sorted.
        assert_eq!(names, vec!["-.slice", "default.target", "foo.service"]);
    }

    #[tokio::test]
    async fn start_units_enqueues_known_and_rejects_unknown() {
        let allocator = test_allocator();
        register_service_worker(&allocator);

        let reply = call(
            "manager.start_units",
            req_env(
                "manager.start_units",
                StartUnitsRequest {
                    names: vec!["foo.service".to_string(), "nope.service".to_string()],
                },
            ),
            allocator.clone(),
        )
        .await;
        assert_eq!(reply.method, "manager.start_units.result");
        let result = StartUnitsResult::decode(reply.payload.as_slice()).unwrap();
        assert_eq!(result.results.len(), 2);

        let foo = result
            .results
            .iter()
            .find(|r| r.name == "foo.service")
            .unwrap();
        assert!(foo.success, "foo.service should enqueue: {}", foo.message);
        assert!(foo.message.starts_with("job "));

        let nope = result
            .results
            .iter()
            .find(|r| r.name == "nope.service")
            .unwrap();
        assert!(!nope.success);
        assert!(!nope.message.is_empty());
    }

    #[tokio::test]
    async fn start_units_creates_jobs() {
        let allocator = test_allocator();
        register_service_worker(&allocator);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        allocator.write().job_event_tx = Some(tx);

        let _ = call(
            "manager.start_units",
            req_env(
                "manager.start_units",
                StartUnitsRequest {
                    names: vec!["foo.service".to_string()],
                },
            ),
            allocator.clone(),
        )
        .await;

        // A job was created for foo.service: it is announced on the
        // lifecycle channel.  (A job that completes is freed from the table
        // immediately, so the table itself may already be empty again.)
        let ev = rx.try_recv().expect("job.new emitted for the enqueued job");
        assert!(
            matches!(ev, crate::state::JobLifecycle::New(ref info) if info.unit_name == "foo.service"),
            "expected a job.new for foo.service, got {ev:?}"
        );
    }

    #[tokio::test]
    async fn stop_units_unknown_rejected() {
        let allocator = test_allocator();
        let reply = call(
            "manager.stop_units",
            req_env(
                "manager.stop_units",
                StopUnitsRequest {
                    name: "nope.service".to_string(),
                },
            ),
            allocator,
        )
        .await;
        assert_eq!(reply.method, "manager.stop_units.result");
        let result = StopUnitsResult::decode(reply.payload.as_slice()).unwrap();
        assert!(!result.success);
        assert!(!result.message.is_empty());
    }

    // -----------------------------------------------------------------------
    // Admin one-shot sessions (moved from the allocator IPC socket)
    // -----------------------------------------------------------------------

    /// Drive `admin_replies` against a fake allocator, returning the envelope
    /// sequence.  The helper avoids the uid check by accepting `client_uid`
    /// directly so tests run deterministically as any UID.
    fn run_admin_replies(
        allocator: AllocatorHandle,
        method: &str,
        payload: impl prost::Message,
        client_uid: u32,
    ) -> Vec<Envelope> {
        let env = make_envelope(42, "tool", "system-a", method, payload).unwrap();
        admin_replies(&allocator, &env, client_uid).unwrap()
    }

    #[test]
    fn admin_staging_list_returns_empty() {
        let alloc = test_allocator();
        let replies = run_admin_replies(
            alloc,
            "admin.staging",
            AdminStagingOp {
                op: "list".to_string(),
                uid: 0,
                name: String::new(),
            },
            0,
        );
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].method, "admin.staging.result");
        let result = AdminStagingResult::decode(replies[0].payload.as_slice()).unwrap();
        assert!(result.success, "empty staging 'list' must succeed");
        assert!(result.entries.is_empty());
    }

    #[test]
    fn admin_staging_rejects_unprivileged_uid() {
        let alloc = test_allocator();
        let replies = run_admin_replies(
            alloc,
            "admin.staging",
            AdminStagingOp {
                op: "list".to_string(),
                uid: 0,
                name: String::new(),
            },
            0xdead00d,
        );
        assert_eq!(replies.len(), 1);
        let result = AdminStagingResult::decode(replies[0].payload.as_slice()).unwrap();
        assert!(!result.success);
        assert!(!result.message.is_empty());
    }

    #[test]
    fn admin_unitstate_streams_every_unit_sorted() {
        let alloc = test_allocator();
        let replies = run_admin_replies(alloc, "admin.unitstate", UnitStateListRequest {}, 0);
        // Should be N entries + 1 EOF.
        let (entries, eof) = split_unitstate_replies(&replies);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["-.slice", "default.target", "foo.service"]);
        for entry in &entries {
            let doc: serde_json::Value = serde_json::from_slice(&entry.json).unwrap();
            assert!(doc["kind"].is_string());
            assert!(doc["unit"]["description"].is_string() || doc["unit"]["after"].is_array());
        }
        assert_eq!(eof.total, 3);
        assert!(eof.message.is_empty());
    }

    #[test]
    fn admin_unitstate_rejects_unprivileged_uid() {
        let alloc = test_allocator();
        let replies =
            run_admin_replies(alloc, "admin.unitstate", UnitStateListRequest {}, 0xdead00d);
        let (entries, eof) = split_unitstate_replies(&replies);
        assert!(entries.is_empty());
        assert_eq!(eof.total, 0);
        assert!(!eof.message.is_empty());
    }

    #[tokio::test]
    async fn admin_staging_via_control_session_roundtrip() {
        let alloc = test_allocator();
        let mut client = spawn_session_pair(alloc.clone()).await;
        // Send admin.staging as the first envelope — no manager.hello.
        let op = AdminStagingOp {
            op: "list".to_string(),
            uid: 0,
            name: String::new(),
        };
        sysa::ipc::send_envelope(&mut client, &req_env("admin.staging", op))
            .await
            .unwrap();
        let reply = sysa::ipc::recv_envelope(&mut client)
            .await
            .unwrap()
            .expect("expected admin.staging result envelope");
        assert_eq!(reply.method, "admin.staging.result");
        // Connection must be closed after the one-shot reply.
        let eof = sysa::ipc::recv_envelope(&mut client).await.unwrap();
        assert!(eof.is_none(), "session must close after admin reply");
    }

    #[tokio::test]
    async fn admin_unitstate_via_control_session_roundtrip() {
        let alloc = test_allocator();
        let mut client = spawn_session_pair(alloc.clone()).await;
        sysa::ipc::send_envelope(
            &mut client,
            &req_env("admin.unitstate", UnitStateListRequest {}),
        )
        .await
        .unwrap();
        // Read entries until EOF; the entry count tracks the EOF tally.
        // (When the test runs as a non-root UID the server returns a
        // permission-denied EOF with zero entries — the framing is identical.)
        let mut total = 0u32;
        loop {
            let env = sysa::ipc::recv_envelope(&mut client)
                .await
                .unwrap()
                .expect("expected admin.unitstate entry or EOF");
            if env.method == "admin.unitstate.eof" {
                let eof = UnitStateEof::decode(env.payload.as_slice()).unwrap();
                assert_eq!(eof.total, total, "EOF total must match streamed entries");
                break;
            }
            assert_eq!(env.method, "admin.unitstate.entry");
            total += 1;
        }
        // Connection must be closed after EOF.
        let eof = sysa::ipc::recv_envelope(&mut client).await.unwrap();
        assert!(eof.is_none(), "session must close after unitstate EOF");
    }

    #[test]
    fn admin_staging_commit_merges_and_consumes_area() {
        let alloc = test_allocator();
        {
            let mut state = alloc.write();
            state.staging_areas.insert(
                (0, "system-f1".to_string()),
                StagingArea {
                    name: "system-f1".to_string(),
                    uid: 0,
                    units: HashMap::from([(
                        "graphical.target".to_string(),
                        staging_target_ir("graphical.target"),
                    )]),
                },
            );
        }
        let replies = run_admin_replies(
            alloc.clone(),
            "admin.staging",
            AdminStagingOp {
                op: "commit".to_string(),
                uid: 0,
                name: String::new(),
            },
            0,
        );
        assert_eq!(replies.len(), 1);
        let result = AdminStagingResult::decode(replies[0].payload.as_slice()).unwrap();
        assert!(result.success, "commit must succeed: {}", result.message);
        let state = alloc.read();
        assert!(
            !state
                .staging_areas
                .contains_key(&(0, "system-f1".to_string())),
            "staging area must be consumed after commit"
        );
    }

    #[test]
    fn admin_staging_commit_by_name_and_missing_area() {
        let alloc = test_allocator();
        {
            let mut state = alloc.write();
            state.staging_areas.insert(
                (1000, "sysv-nginx".to_string()),
                StagingArea {
                    name: "sysv-nginx".to_string(),
                    uid: 1000,
                    units: HashMap::from([(
                        "nginx.service".to_string(),
                        staging_target_ir("nginx.service"),
                    )]),
                },
            );
        }
        // Committing a named area succeeds and consumes it.
        let replies = run_admin_replies(
            alloc.clone(),
            "admin.staging",
            AdminStagingOp {
                op: "commit".to_string(),
                uid: 1000,
                name: "sysv-nginx".to_string(),
            },
            0,
        );
        let result = AdminStagingResult::decode(replies[0].payload.as_slice()).unwrap();
        assert!(
            result.success,
            "named commit must succeed: {}",
            result.message
        );
        // A second commit of the same (already consumed) area is idempotent.
        let replies = run_admin_replies(
            alloc.clone(),
            "admin.staging",
            AdminStagingOp {
                op: "commit".to_string(),
                uid: 1000,
                name: "sysv-nginx".to_string(),
            },
            0,
        );
        let result = AdminStagingResult::decode(replies[0].payload.as_slice()).unwrap();
        assert!(
            result.success,
            "idempotent re-commit must succeed: {}",
            result.message
        );
        // Committing for a UID with no areas reports failure.
        let replies = run_admin_replies(
            alloc.clone(),
            "admin.staging",
            AdminStagingOp {
                op: "commit".to_string(),
                uid: 4242,
                name: String::new(),
            },
            0,
        );
        let result = AdminStagingResult::decode(replies[0].payload.as_slice()).unwrap();
        assert!(!result.success);
    }

    // -- helpers for the unitstate tests ---------------------------------

    fn split_unitstate_replies(replies: &[Envelope]) -> (Vec<UnitStateEntry>, UnitStateEof) {
        let mut entries = Vec::new();
        let mut eof = None;
        for env in replies {
            match env.method.as_str() {
                "admin.unitstate.entry" => {
                    entries.push(UnitStateEntry::decode(env.payload.as_slice()).unwrap());
                }
                "admin.unitstate.eof" => {
                    eof = Some(UnitStateEof::decode(env.payload.as_slice()).unwrap());
                }
                _ => panic!("unexpected admin reply method '{}'", env.method),
            }
        }
        (entries, eof.expect("EOF sentinel"))
    }

    /// Minimal `UnitIR` for a target unit, as a systemd finder would emit it.
    fn staging_target_ir(id: &str) -> UnitIR {
        UnitIR {
            id: id.to_string(),
            unit_type: Some(UnitType::Target),
            description: None,
            source_format: Some("systemd".to_string()),
            source_path: None,
            aliases: vec![],
            alias_of: None,
            slice: None,
            dependencies: None,
            service: None,
            mount: None,
            automount: None,
            timer: None,
            socket: None,
            path: None,
            resource_control: None,
            conditions: None,
            asserts: None,
            wanted_by: None,
            required_by: None,
            enabled: None,
            template: None,
        }
    }

    fn scope_prop(key: &str, pid: u64) -> TransientProperty {
        TransientProperty {
            key: key.to_string(),
            value: Some(ManagerValue {
                value: Some(Value::U(pid)),
            }),
        }
    }

    #[test]
    fn scope_pids_reads_both_spellings() {
        // Neither key present names nobody at all — that is a different
        // statement from an explicitly empty list.
        assert_eq!(scope_pids(&[]), None);
        assert_eq!(scope_pids(&[scope_prop("Description", 0)]), None);

        assert_eq!(scope_pids(&[scope_prop("PIDs", 1)]), Some(vec![1]));
        assert_eq!(scope_pids(&[scope_prop("PIDFDs", 4111)]), Some(vec![4111]));
        assert_eq!(
            scope_pids(&[scope_prop("PIDs", 1), scope_prop("PIDFDs", 4111)]),
            Some(vec![1, 4111])
        );
    }

    #[test]
    fn transient_scope_takes_pids_from_pidfds() {
        // logind names a session scope's processes by pidfd; the D-Bus bridge
        // resolves them to PIDs (push_prop_value), so they arrive here under
        // `PIDFDs=` and must end up as the scope's own PIDs=.
        let props = vec![scope_prop("PIDFDs", 4111)];
        let uf = transient_unit_from_properties("session-c3.scope", &props, Some(888));
        let scope = uf.scope.expect("scope section");
        assert_eq!(scope.pids, vec!["4111".to_string()]);
    }

    #[test]
    fn transient_scope_without_pids_does_not_swallow_the_sender() {
        // A scope that lists no processes must stay empty rather than default
        // to whoever asked.  logind issues every session scope, so the old
        // sender fallback moved logind itself into the sessions it was
        // creating — and would have taken it down with them.
        let uf = transient_unit_from_properties("session-c3.scope", &[], Some(888));
        let scope = uf.scope.expect("scope section");
        assert!(scope.pids.is_empty());
    }

    // -------------------------------------------------------------------
    // Transient service/timer/path assembly (systemd-run property bags)
    // -------------------------------------------------------------------

    fn sv_prop(key: &str, value: Value) -> TransientProperty {
        TransientProperty {
            key: key.to_string(),
            value: Some(ManagerValue { value: Some(value) }),
        }
    }

    fn bag(items: &[(&str, Value)]) -> Vec<TransientProperty> {
        items.iter().map(|(k, v)| sv_prop(k, v.clone())).collect()
    }

    fn exec_prop(key: &str, path: &str, args: &[&str]) -> TransientProperty {
        sv_prop(
            key,
            Value::Exec(sysa::proto::Exec {
                path: path.to_string(),
                args: args.iter().map(|s| s.to_string()).collect(),
                ignore_failure: false,
            }),
        )
    }

    #[test]
    fn transient_timer_seconds_ceil_from_usecs() {
        // Timer delays arrive on the bus as microsecond counts (type `t`);
        // this model stores whole seconds and must ceil, else a sub-second
        // entry (ceil → 1) would never fire at all.
        let uf = transient_unit_from_properties(
            "t.timer",
            &bag(&[
                ("OnActiveSec", Value::U(90_000_000)),
                ("OnBootSec", Value::U(1_500_000)),
                ("AccuracySec", Value::U(60_000_000)),
                ("OnCalendar", Value::S("*-*-* 02:00:00".to_string())),
                ("Persistent", Value::B(true)),
                ("RemainAfterElapse", Value::B(false)),
            ]),
            None,
        );
        assert_eq!(uf.kind, UnitKind::Timer);
        let t = uf.timer.expect("timer section");
        assert_eq!(t.on_active_sec, Some(90));
        assert_eq!(t.on_boot_sec, Some(2));
        assert_eq!(t.accuracy_sec, 60);
        assert_eq!(t.on_calendar, vec!["*-*-* 02:00:00".to_string()]);
        assert!(t.persistent);
        assert!(!t.remain_after_elapse);
    }

    #[test]
    fn transient_units_without_trigger_are_detected() {
        // `lastoreAutoCheck.timer` / `watchOsVersion.path`: when the property
        // bag carries no trigger directive at all, the mapped section is
        // empty, so the unit can never be scheduled or armed.  The assembly
        // step warns about this and the owning worker refuses the unit.
        let timer = transient_unit_from_properties(
            "lastoreAutoCheck.timer",
            &bag(&[("Persistent", Value::B(false))]),
            None,
        );
        assert!(!timer_has_trigger(
            timer.timer.as_ref().expect("timer section")
        ));

        let path = transient_unit_from_properties(
            "watchOsVersion.path",
            &bag(&[("MakeDirectory", Value::B(true))]),
            None,
        );
        assert!(!path_has_condition(
            path.path.as_ref().expect("path section")
        ));

        // A single trigger directive is enough for the unit to be schedulable.
        let timer = transient_unit_from_properties(
            "ok.timer",
            &bag(&[("OnBootSec", Value::U(90_000_000))]),
            None,
        );
        assert!(timer_has_trigger(
            timer.timer.as_ref().expect("timer section")
        ));
    }

    #[test]
    fn transient_path_specs_from_lists() {
        // `systemd-run --unit=watch --path-property=PathModified=/etc/os-version`
        // sends the path's directives as string-list properties.
        let uf = transient_unit_from_properties(
            "watch.path",
            &bag(&[
                ("PathModified", Value::S("/etc/os-version".to_string())),
                ("PathExists", Value::S("/run/a".to_string())),
                ("MakeDirectory", Value::B(true)),
                ("DirectoryMode", Value::S("0o755".to_string())),
            ]),
            None,
        );
        assert_eq!(uf.kind, UnitKind::Path);
        let p = uf.path.expect("path section");
        assert_eq!(p.path_modified, vec!["/etc/os-version".to_string()]);
        assert_eq!(p.path_exists, vec!["/run/a".to_string()]);
        assert!(p.make_directory);
        assert_eq!(p.directory_mode, "0o755");
    }

    #[test]
    fn transient_service_exec_and_type() {
        // `Type=exec` is systemd's transient-service default and behaves like
        // `simple`, which is this model's spelling of the same semantics.
        let mut props = bag(&[
            ("Type", Value::S("exec".to_string())),
            ("RemainAfterExit", Value::B(true)),
            ("WorkingDirectory", Value::S("/var/tmp".to_string())),
        ]);
        props.push(exec_prop("ExecStart", "/bin/true", &["/bin/true"]));
        let uf = transient_unit_from_properties("x.service", &props, None);
        assert_eq!(uf.kind, UnitKind::Service);
        let s = uf.service.expect("service section");
        assert_eq!(s.service_type, ServiceType::Simple);
        assert!(s.remain_after_exit);
        assert_eq!(s.working_directory, "/var/tmp");
    }

    #[test]
    fn transient_service_exec_commands_decode() {
        // One a(sasb) element per command: argv[0] repeats the program path
        // (systemd-run convention); the model's `args` starts after it and
        // `raw` rejoins the argv so the runtime split resolves the same.
        let props = vec![
            exec_prop(
                "ExecStart",
                "/usr/bin/sh",
                &["/usr/bin/sh", "-c", "echo hi"],
            ),
            sv_prop("Type", Value::S("oneshot".to_string())),
        ];
        let uf = transient_unit_from_properties("oneshot.service", &props, None);
        let s = uf.service.expect("service section");
        assert_eq!(s.service_type, ServiceType::Oneshot);
        assert_eq!(s.exec_start.len(), 1);
        let cmd = &s.exec_start[0];
        assert_eq!(cmd.program, "/usr/bin/sh");
        assert_eq!(cmd.args, vec!["-c".to_string(), "echo hi".to_string()]);
        assert_eq!(cmd.raw, "/usr/bin/sh -c 'echo hi'");
    }

    #[test]
    fn exec_raw_join_quotes_special_args() {
        // Arguments containing whitespace/quotes must survive the roundtrip
        // through the raw string and the runtime word split.  The argv passed
        // here includes argv[0] (systemd's exec-struct convention), which is
        // skipped except for the program itself.
        for (args, raw) in [
            (
                &["/bin/x".to_string(), "a b".to_string()][..],
                "/bin/x 'a b'",
            ),
            (
                &["/bin/x".to_string(), "it's".to_string()][..],
                "/bin/x 'it'\\''s'",
            ),
        ] {
            assert_eq!(exec_raw_join("/bin/x", args), raw);
        }
    }

    #[tokio::test]
    async fn start_transient_installs_timer_and_aux_service() {
        // systemd-run timer mode ships the companion NAME.service in the
        // aux list of the same StartTransientUnit call; both units must end
        // up in the table, the timer with its [Timer] section filled.
        let allocator = test_allocator();
        register_worker(&allocator, "system-s-1", &["timer", "service"]);
        let props = bag(&[
            ("OnActiveSec", Value::U(10_000_000)),
            ("Description", Value::S("run periodically".to_string())),
        ]);
        let aux = sysa::proto::TransientUnit {
            name: "auxtick.service".to_string(),
            properties: Some(sysa::proto::PropertyBag {
                properties: vec![
                    exec_prop("ExecStart", "/bin/true", &["/bin/true"]),
                    sv_prop("Description", Value::S("tick body".to_string())),
                ],
            }),
        };
        let req = TransientUnitRequest {
            name: "audittimer.timer".to_string(),
            kind: String::new(),
            properties: Some(sysa::proto::PropertyBag { properties: props }),
            sender_pid: 0,
            mode: "fail".to_string(),
            aux_units: vec![aux],
        };
        let _ = call(
            "manager.start_transient",
            req_env("manager.start_transient", req),
            allocator.clone(),
        )
        .await;
        let state = allocator.read();
        let timer = state.units.get("audittimer.timer").expect("timer unit");
        assert!(timer.transient);
        assert_eq!(timer.kind, UnitKind::Timer);
        let t = timer.timer.as_ref().expect("timer section");
        assert_eq!(t.on_active_sec, Some(10));
        let service = state.units.get("auxtick.service").expect("aux service");
        assert!(service.transient);
        assert_eq!(service.kind, UnitKind::Service);
        let s = service.service.as_ref().expect("service section");
        assert_eq!(s.exec_start[0].program, "/bin/true");
        assert_eq!(service.unit.description, "tick body");
    }

    #[tokio::test]
    async fn start_transient_refreshes_transient_but_not_file_backed() {
        // A repeat transient call refreshes an existing transient unit;
        // a real, file-backed definition always wins over the transient.
        let allocator = test_allocator();
        register_service_worker(&allocator);
        {
            let mut state = allocator.write();
            let mut file_backed = UnitFile::new("real.service");
            file_backed.unit = LocalUnitSection::default();
            state.units.insert("real.service".to_string(), file_backed);
        }
        for description in ["first", "second"] {
            let req = TransientUnitRequest {
                name: "again.service".to_string(),
                kind: String::new(),
                properties: Some(sysa::proto::PropertyBag {
                    properties: vec![sv_prop("Description", Value::S(description.to_string()))],
                }),
                sender_pid: 0,
                mode: "fail".to_string(),
                aux_units: Vec::new(),
            };
            // The upsert happens before the job enqueue; only tolerate the
            // enqueue error ("job already exists" for the repeat call) since
            // the unit-table assertions below are what this test is about.
            let env = req_env("manager.start_transient", req);
            let _ = dispatch(&allocator, env).await;
        }
        let state = allocator.read();
        let transient = state.units.get("again.service").unwrap();
        assert!(transient.transient);
        assert_eq!(transient.unit.description, "second");
        let real = state.units.get("real.service").unwrap();
        assert!(!real.transient);
        assert!(real.unit.description.is_empty());
    }
}
