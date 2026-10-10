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

use std::collections::HashSet;
use std::sync::Arc;

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
    AbandonScopeRequest, AdminStagingOp, AdminStagingResult, DaemonReloadResult, Envelope,
    EnqueueJobRequest, EnqueueJobResult, GetUnitByInvocationRequest, GetUnitByPidRequest,
    GetUnitByPidResult, ListUnitsRequest, ListUnitsResult, LoadUnitRequest, LoadUnitResult,
    ManagerHelloRequest, ManagerHelloResult, ManagerValue, RefUnitRequest, RefUnitResult,
    RegisterPowerUnitsRequest, RegisterPowerUnitsResult, ResetFailedUnitRequest,
    SetUnitPropertiesRequest, SimpleManagerResult, StartUnitsRequest, StartUnitsResult,
    StopUnitsRequest, StopUnitsResult, TransientProperty, TransientUnitRequest, UnitInfo,
    UnitSnapshotRequest, UnitStartResult, UnitStateEntry, UnitStateEof, UnitStateListRequest,
};

use crate::scheduler::job_type::JobType;
use crate::state::{
    next_request_id, next_task_id, AllocatorHandle, AllocatorState, DesiredState, JobKind, JobMode,
};
use crate::unit::types::{ResourceControl, UnitFile, UnitKind};
use crate::{events, snapshot};

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
/// outbound mpsc channel (bounded → slow clients are dropped, never wedged).
struct ControlForwarder {
    tx: mpsc::Sender<bytes::Bytes>,
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
        if self.tx.try_send(buf.freeze()).is_err() {
            // Session read end closed or slow — drop the event.
            debug_drop(event.topic.clone());
        }
    }
}

fn debug_drop(topic: EventTopic) {
    warn!("Control forwarder dropped event for {:?} (slow/closed session)", topic);
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
    let first_env = match reader.next().await {
        None => {
            info!(
                "Control client (PID {client_pid}) disconnected before the first envelope"
            );
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
    let first_method = first_env.method.clone();
    if matches!(first_method.as_str(), "admin.staging" | "admin.unitstate") {
        let replies = admin_replies(&allocator, &first_env, client_uid)?;
        let mut writer = writer;
        for reply in replies {
            if writer.send(encode_envelope(&reply)).await.is_err() {
                break;
            }
        }
        info!(
            "Admin control session from PID {client_pid} (UID {client_uid}): '{first_method}'"
        );
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

    // Subscribe to control events before serving requests.
    let (out_tx, out_rx) = mpsc::channel::<bytes::Bytes>(256);
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
                warn!("Control session read error: {e}");
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
                        if out_tx.send(encode_envelope(&reply)).await.is_err() {
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
                            if out_tx.send(encode_envelope(&err)).await.is_err() {
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
    writer_task.abort();
    info!("Control session from PID {client_pid} closed");
    Ok(())
}

// ---------------------------------------------------------------------------
// One-shot admin sessions (stagingctl, unitstatectl)
// ---------------------------------------------------------------------------

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
                    message: sysa::l10n::fmt(
                        sysa::l10n::t_(
                            "Permission denied (UID {uid}): only root or UID {sys_uid} may query staging areas.",
                        ),
                        &[
                            ("uid", &client_uid.to_string()),
                            ("sys_uid", &sys_uid.to_string()),
                        ],
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
                        message: sysa::l10n::fmt(
                            sysa::l10n::t_(
                                "Permission denied (UID {uid}): only root or UID {sys_uid} may inspect unit state.",
                            ),
                            &[
                                ("uid", &client_uid.to_string()),
                                ("sys_uid", &sys_uid.to_string()),
                            ],
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
async fn dispatch(
    allocator: &AllocatorHandle,
    env: Envelope,
) -> Result<Option<Envelope>> {
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

    let canonical = allocator.read().resolve_unit_name(&req.name);
    if !allocator.read().units.contains_key(&canonical) {
        let alloc = allocator.clone();
        let name = canonical.clone();
        tokio::task::spawn_blocking(move || events::load_unit_sync(&alloc, &name))
            .await
            .context(sysa::l10n::t_("load_unit task panicked"))?
            .context(sysa::l10n::t_("failed to load unit"))?;
    }

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

/// `manager.load_unit` — load a unit file from disk into the registry.
async fn manager_load_unit(allocator: &AllocatorHandle, env: Envelope) -> Result<Envelope> {
    let req = LoadUnitRequest::decode(env.payload.as_slice())
        .context(sysa::l10n::t_("bad manager.load_unit"))?;
    let canonical = allocator.read().resolve_unit_name(&req.name);
    let alloc = allocator.clone();
    let name = canonical.clone();
    let result = tokio::task::spawn_blocking(move || events::load_unit_sync(&alloc, &name))
        .await
        .context(sysa::l10n::t_("load_unit task panicked"))?;
    match result {
        Ok(()) => make_envelope(
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
                name: canonical,
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
    let uf = transient_unit_from_properties(&canonical, &req.properties.as_ref().map(|bag| bag.properties.as_slice()).unwrap_or(&[]), sender_pid);

    {
        let mut state = allocator.write();
        if !state.units.contains_key(&canonical) {
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
        if is_ref { "manager.ref_unit.result" } else { "manager.unref_unit.result" },
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
async fn manager_list_units(allocator: &AllocatorHandle, env: Envelope) -> Result<Envelope> {
    let req = ListUnitsRequest::decode(env.payload.as_slice())
        .context(sysa::l10n::t_("bad manager.list_units"))?;
    let enabled =
        crate::unit::enable::scan_enabled_units(&sysa::paths::instance().unit_search_paths);
    let result = {
        let state = allocator.read();
        build_list_result(&state, &enabled, req.enabled_only)
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
fn build_list_result(
    state: &crate::state::AllocatorState,
    enabled: &HashSet<String>,
    enabled_only: bool,
) -> ListUnitsResult {
    let mut units: Vec<UnitInfo> = if enabled_only {
        enabled
            .iter()
            .map(|name| {
                let unit_type = state
                    .units
                    .get(name)
                    .map(|u| u.kind.worker_type().to_string())
                    .unwrap_or_else(|| {
                        crate::unit::types::UnitKind::from_extension(name)
                            .worker_type()
                            .to_string()
                    });
                UnitInfo {
                    name: name.clone(),
                    unit_type,
                    enabled: true,
                }
            })
            .collect()
    } else {
        state
            .units
            .iter()
            .map(|(name, unit)| UnitInfo {
                name: name.clone(),
                unit_type: unit.kind.worker_type().to_string(),
                enabled: enabled.contains(name),
            })
            .collect()
    };
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

    let mut results = Vec::with_capacity(req.names.len());
    for name in &req.names {
        info!("manager.start_units: processing '{name}'");
        match tokio::time::timeout(
            std::time::Duration::from_secs(15),
            crate::scheduler::enqueue_start_with_mode(
                allocator.clone(),
                name,
                JobMode::Replace,
            ),
        )
        .await
        {
            Ok(Ok(job_id)) => {
                info!("manager.start_units: enqueued job {job_id} for '{name}'");
                results.push(UnitStartResult {
                    name: name.clone(),
                    success: true,
                    message: sysa::l10n::fmt(
                        sysa::l10n::t_("job {job_id}"),
                        &[("job_id", &job_id.to_string())],
                    ),
                });
            }
            Ok(Err(e)) => {
                warn!("manager.start_units: cannot start '{}': {}", name, e);
                results.push(UnitStartResult {
                    name: name.clone(),
                    success: false,
                    message: e.to_string(),
                });
            }
            Err(_) => {
                warn!("manager.start_units: timed out starting '{}'", name);
                results.push(UnitStartResult {
                    name: name.clone(),
                    success: false,
                    message: sysa::l10n::t_("start timed out").to_string(),
                });
            }
        }
    }

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
/// dispatch to System Init through `POWER=<action>` datagrams, so no
/// `power` worker is ever registered.
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
            alloc.write().desired.insert(name.to_string(), DesiredState::Active);
        }
        JobKind::Stop => {
            alloc.write().desired.insert(name.to_string(), DesiredState::Inactive);
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
        (key, Some(Value::S(s))) if matches!(key, "MemoryMin" | "MemoryLow" | "MemoryHigh" | "MemoryMax" | "MemorySwapMax" | "CPUQuotaPeriodSec" | "CPUQuotaPeriodUSec") => {
            set_byte_string(rc, key, s);
            true
        }
        (key, Some(Value::U(u))) if matches!(key, "MemoryMin" | "MemoryLow" | "MemoryHigh" | "MemoryMax" | "MemorySwapMax" | "CPUQuotaPeriodSec" | "CPUQuotaPeriodUSec") => {
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
                s.parse::<u64>().map(|v| v.min(u32::MAX as u64) as u32).unwrap_or(rc.tasks_max)
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

    uf
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
use std::collections::{HashMap, HashSet};

    use sysa::proto::{
        ListUnitsRequest, ListUnitsResult, StartUnitsRequest, StartUnitsResult, StopUnitsRequest,
        StopUnitsResult,
    };

    use crate::state::{StagingArea, WorkerEntry};
    use crate::unit::types::{UnitSection};
    use systema_sysf::ir::{UnitIR, UnitType};

    fn test_allocator() -> AllocatorHandle {
        let state = Arc::new(parking_lot::RwLock::new(AllocatorState::new()));
        {
            let mut state = state.write();
            let mut foo = UnitFile::new("foo.service");
            foo.unit = UnitSection::default();
            state.units.insert("foo.service".to_string(), foo);
            state.units.insert(
                "default.target".to_string(),
                UnitFile::new("default.target"),
            );
        }
        state
    }

    fn register_service_worker(state: &AllocatorHandle) {
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let mut state = state.write();
        state.workers.insert(
            "system-s-1".to_string(),
            WorkerEntry {
                worker_id: "system-s-1".to_string(),
                unit_types: vec!["service".to_string()],
                supports_unit_define: false,
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
    async fn spawn_session_pair(
        allocator: AllocatorHandle,
    ) -> sysa::ipc::EnvelopeFramed {
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

        let req = ListUnitsRequest { enabled_only: false };
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
            state_guard
                .units
                .insert("foo.service".to_string(), UnitFile::new("foo.service"));
            state_guard
                .units
                .insert("bar.timer".to_string(), UnitFile::new("bar.timer"));
        }

        let enabled: HashSet<String> = ["foo.service".to_string()].into_iter().collect();
        let all = build_list_result(&state.read(), &enabled, false);
        // "-.slice" (root slice, auto-created) + foo.service + bar.timer.
        assert_eq!(all.units.len(), 3);

        let only = build_list_result(&state.read(), &enabled, true);
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
                slice: None,
                dependencies: None,
                service: None,
                mount: None,
                automount: None,
                timer: None,
                socket: None,
                resource_control: None,
                conditions: None,
                asserts: None,
                wanted_by: None,
                required_by: None,
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
        let uf = state
            .units
            .get("poweroff.power")
            .expect("unit in graph");
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
        let err = dispatch(&allocator, env).await.expect_err("bad JSON must fail");
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
        let err = dispatch(&allocator, env).await.expect_err("bad JSON must fail");
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
            req_env("manager.list_units", ListUnitsRequest { enabled_only: false }),
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

        let state = allocator.read();
        let has_job = state.jobs.values().any(|j| j.unit_name == "foo.service");
        assert!(has_job, "a job for foo.service must exist in state");
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
        let replies = run_admin_replies(alloc, "admin.staging", AdminStagingOp {
            op: "list".to_string(),
            uid: 0,
            name: String::new(),
        }, 0);
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].method, "admin.staging.result");
        let result = AdminStagingResult::decode(replies[0].payload.as_slice()).unwrap();
        assert!(result.success, "empty staging 'list' must succeed");
        assert!(result.entries.is_empty());
    }

    #[test]
    fn admin_staging_rejects_unprivileged_uid() {
        let alloc = test_allocator();
        let replies = run_admin_replies(alloc, "admin.staging", AdminStagingOp {
            op: "list".to_string(),
            uid: 0,
            name: String::new(),
        }, 0xdead00d);
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
            assert!(
                doc["unit"]["description"].is_string() || doc["unit"]["after"].is_array()
            );
        }
        assert_eq!(eof.total, 3);
        assert!(eof.message.is_empty());
    }

    #[test]
    fn admin_unitstate_rejects_unprivileged_uid() {
        let alloc = test_allocator();
        let replies = run_admin_replies(alloc, "admin.unitstate", UnitStateListRequest {}, 0xdead00d);
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
        sysa::ipc::send_envelope(&mut client, &req_env("admin.unitstate", UnitStateListRequest {}))
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
            !state.staging_areas.contains_key(&(0, "system-f1".to_string())),
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
        assert!(result.success, "named commit must succeed: {}", result.message);
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
            slice: None,
            dependencies: None,
            service: None,
            mount: None,
            automount: None,
            timer: None,
            socket: None,
            resource_control: None,
            conditions: None,
            asserts: None,
            wanted_by: None,
            required_by: None,
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
}