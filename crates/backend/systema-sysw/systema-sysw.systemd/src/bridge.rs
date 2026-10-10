//! The System Wrapper bridge: keeps the unit mirror fresh from control-port
//! events and serves the systemd1 D-Bus interface.

use std::sync::Arc;

use anyhow::Result;
use prost::Message as ProstMessage;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use zvariant::OwnedObjectPath;

use systema_sysw_common::event::{ControlEvent, ControlEventKind};
use systema_sysw_common::ControlClient;

use crate::dbus;
use crate::dbus::manager::{job_object_path, ManagerInterface};
use crate::mirror::{MirrorHandle, UnitMirror};

/// Prime the mirror from the initial snapshot + job listings.
async fn seed(ctx: &Arc<dbus::BridgeContext>) -> Result<()> {
    let units = systema_sysw_common::client::calls::list_snapshots(&ctx.client).await?;
    let jobs = systema_sysw_common::client::calls::list_jobs(&ctx.client).await?;
    info!(
        "Primed mirror from control plane: {} unit(s), {} job(s)",
        units.len(),
        jobs.len()
    );
    ctx.mirror.write().seed(units, jobs);
    Ok(())
}

/// Emit a Manager signal for a control event (best effort).
async fn emit_signals(conn: &zbus::Connection, event: &ControlEvent) {
    let Ok(signal_ctx) = zbus::SignalContext::new(conn, "/org/freedesktop/systemd1") else {
        return;
    };
    match event.kind {
        ControlEventKind::JobNew => {
            if let Ok(payload) = sysa::proto::JobEvent::decode(event.envelope.payload.as_slice())
            {
                let _ = ManagerInterface::job_new(
                    &signal_ctx,
                    payload.job_id as u32,
                    job_object_path(payload.job_id),
                    payload.unit_name,
                )
                .await;
            }
        }
        ControlEventKind::JobCompleted => {
            if let Ok(payload) = sysa::proto::JobEvent::decode(event.envelope.payload.as_slice())
            {
                let _ = ManagerInterface::job_removed(
                    &signal_ctx,
                    payload.job_id as u32,
                    job_object_path(payload.job_id),
                    payload.unit_name.clone(),
                    payload.result.clone(),
                )
                .await;
            }
        }
        ControlEventKind::UnitRemoved => {
            // UnitRemovedEvent carries only the unit_name.  The event loop
            // has already removed the unit from the mirror; job paths are
            // best-effort, so emit the root path (systemd sends the job path
            // but we don't track removal jobs).
            if let Ok(payload) =
                sysa::proto::UnitRemovedEvent::decode(event.envelope.payload.as_slice())
            {
                let job_path = OwnedObjectPath::try_from("/").unwrap();
                let _ = ManagerInterface::unit_removed(&signal_ctx, payload.unit_name, job_path)
                    .await;
            }
        }
        _ => {}
    }
}

/// Consume control-port events: maintain the mirror, register D-Bus objects,
/// resolve job waiters, and relay lifecycle signals on the bus.
async fn event_loop(ctx: Arc<dbus::BridgeContext>, mut rx: mpsc::UnboundedReceiver<ControlEvent>) {
    loop {
        let Some(event) = rx.recv().await else {
            warn!("Control event stream ended — bridge loses its mirror");
            return;
        };
        match event.kind {
            ControlEventKind::UnitNew => {
                if let Ok(snapshot) =
                    sysa::proto::UnitSnapshot::decode(event.envelope.payload.as_slice())
                {
                    ctx.mirror.write().upsert(snapshot.clone());
                    if let Some(conn) = dbus::connection(&ctx) {
                        dbus::register_unit_object(conn, &ctx, &snapshot.name).await;
                    }
                }
            }
            ControlEventKind::UnitChanged => {
                if let Ok(snapshot) =
                    sysa::proto::UnitSnapshot::decode(event.envelope.payload.as_slice())
                {
                    ctx.mirror.write().upsert(snapshot);
                }
            }
            ControlEventKind::UnitRemoved => {
                if let Ok(payload) =
                    sysa::proto::UnitRemovedEvent::decode(event.envelope.payload.as_slice())
                {
                    ctx.mirror.write().remove(&payload.unit_name);
                }
            }
            ControlEventKind::UnitMetrics => {
                if let Ok(metrics) =
                    sysa::proto::UnitCgroupMetrics::decode(event.envelope.payload.as_slice())
                {
                    let mut mirror = ctx.mirror.write();
                    if let Some(snap) = mirror.get(&metrics.unit_name) {
                        let mut snap = snap.clone();
                        snap.metrics = Some(metrics);
                        mirror.upsert(snap);
                    }
                }
            }
            ControlEventKind::JobNew => {
                if let Ok(payload) = sysa::proto::JobEvent::decode(event.envelope.payload.as_slice())
                {
                    ctx.mirror.write().add_job(sysa::proto::JobInfo {
                        job_id: payload.job_id,
                        unit_name: payload.unit_name,
                        job_type: String::new(),
                        status: "running".to_string(),
                    });
                }
            }
            ControlEventKind::JobCompleted => {
                if let Ok(payload) = sysa::proto::JobEvent::decode(event.envelope.payload.as_slice())
                {
                    ctx.mirror.write().remove_job(payload.job_id);
                }
            }
        }
        if let Some(conn) = dbus::connection(&ctx) {
            emit_signals(conn, &event).await;
        }
        let (n_units, n_jobs) = {
            let mirror = ctx.mirror.read();
            (mirror.len(), mirror.running_jobs().len())
        };
        debug!("Mirror: {n_units} unit(s), {n_jobs} job(s)");
    }
}

/// Run the bridge: connect to the control plane, prime the mirror, serve
/// D-Bus, and keep the mirror fresh until the session dies (returning so the
/// caller can reconnect).
pub async fn run() -> Result<()> {
    let flavor = "systemd";
    let client = Arc::new(ControlClient::connect(flavor, Some("system-w-1")).await?);

    let mirror: MirrorHandle = Arc::new(parking_lot::RwLock::new(UnitMirror::new()));
    let ctx = dbus::BridgeContext::new(mirror, client.clone());

    seed(&ctx).await?;

    let conn = dbus::run_dbus(&ctx).await?;
    let _conn = conn; // kept alive by ctx.conn, but hold it for safety

    // dbus-daemon's systemd-style activation reaches us over a signal rather
    // than a method call, so it needs its own listener beside the object
    // server.  Tie it to this bridge session: a listener that outlived it
    // would keep `conn` — and with it `org.freedesktop.systemd1` — alive, and
    // the safety check in `run_dbus` would then refuse every reconnect.
    let activation = tokio::spawn(dbus::activator::run(ctx.clone()));

    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let events = client.events();
    tokio::spawn(async move {
        loop {
            let mut rx = events.lock().await;
            match rx.recv().await {
                Some(event) => {
                    if event_tx.send(event).is_err() {
                        break;
                    }
                }
                None => break,
            }
        }
        drop(event_tx);
    });

    let event_task = tokio::spawn(event_loop(ctx, event_rx));

    info!("systema-sysw.systemd bridge running");
    let _ = event_task.await;
    // Drop the activation listener — and with it the `zbus::Connection` it
    // holds — before the reconnect loop claims the bus name again.
    activation.abort();
    let _ = activation.await;
    anyhow::bail!(sysa::l10n::t_("control session ended — reconnect required"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixStream;
    use tokio::sync::mpsc;

    use sysa::ipc::{make_envelope, recv_envelope, send_envelope};
    use sysa::proto::{
        EnqueueJobRequest, EnqueueJobResult, JobEvent, ManagerHelloResult, SimpleManagerResult,
    };

    use crate::dbus::BridgeContext;

    /// Minimal control-bus server for the StartUnit test: answers
    /// `manager.enqueue` and immediately pushes `job.new` + `job.completed`.
    async fn mock_server(mut framed: sysa::ipc::EnvelopeFramed) -> anyhow::Result<()> {
        loop {
            let Some(env) = recv_envelope(&mut framed).await? else {
                return Ok(());
            };
            match env.method.as_str() {
                "manager.hello" => {
                    let reply = make_envelope(
                        env.request_id,
                        "system-a",
                        "mock",
                        "manager.hello.result",
                        ManagerHelloResult {
                            success: true,
                            message: "ok".to_string(),
                        },
                    )?;
                    send_envelope(&mut framed, &reply).await?;
                }
                "manager.enqueue" => {
                    let req = EnqueueJobRequest::decode(env.payload.as_slice())?;
                    let reply = make_envelope(
                        env.request_id,
                        "system-a",
                        "mock",
                        "manager.enqueue.result",
                        EnqueueJobResult {
                            success: true,
                            message: String::new(),
                            job_id: 7,
                            unit_name: req.name,
                        },
                    )?;
                    send_envelope(&mut framed, &reply).await?;

                    let events = [
                        ("job.new", String::new()),
                        ("job.completed", "done".to_string()),
                    ];
                    for (i, (method, result)) in events.into_iter().enumerate() {
                        let event = make_envelope(
                            0,
                            "system-a",
                            "mock",
                            method,
                            JobEvent {
                                job_id: 7,
                                unit_name: "demo.service".to_string(),
                                result,
                            },
                        )?;
                        send_envelope(&mut framed, &event).await?;
                        if i == 0 {
                            // Hold `job.completed` back for a beat: both events
                            // are pushed back-to-back, so without a gap the
                            // mirror's "job present" state is only ever visible
                            // inside one event-loop pass and the test could
                            // never observe it.
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        }
                    }
                }
                other => {
                    let reply = make_envelope(
                        env.request_id,
                        "system-a",
                        "mock",
                        "manager.error",
                        SimpleManagerResult {
                            success: false,
                            message: format!("mock: no handler for {other}"),
                        },
                    )?;
                    send_envelope(&mut framed, &reply).await?;
                }
            }
        }
    }

    /// Wait until `job_id` has shown up in the mirror and has then been
    /// dropped from it — i.e. the event loop applied both `job.new` and
    /// `job.completed`.
    async fn mirror_sees_job_transition(ctx: &Arc<BridgeContext>, job_id: u64) {
        for expect_present in [true, false] {
            loop {
                let present = ctx
                    .mirror
                    .read()
                    .running_jobs()
                    .iter()
                    .any(|j| j.job_id == job_id);
                if present == expect_present {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        }
    }

    #[tokio::test]
    async fn start_unit_reply_is_queued_then_job_events_reach_the_mirror() {
        let (server, client) = UnixStream::pair().unwrap();
        let server_framed = sysa::ipc::frame_stream(server);
        tokio::spawn(async move { mock_server(server_framed).await });

        let client = Arc::new(ControlClient::connect_on(client, "test", None).await.unwrap());
        let mirror: MirrorHandle = Arc::new(parking_lot::RwLock::new(UnitMirror::new()));
        let ctx = BridgeContext::new(mirror, client.clone());

        // Forward pushed control events into the event loop, exactly like
        // `run()` does.
        let (event_tx, event_rx) = mpsc::unbounded_channel::<ControlEvent>();
        let events = client.events();
        tokio::spawn(async move {
            loop {
                let mut rx = events.lock().await;
                match rx.recv().await {
                    Some(event) => {
                        if event_tx.send(event).is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
            drop(event_tx);
        });
        let event_task = tokio::spawn(event_loop(ctx.clone(), event_rx));

        // Replicate `Manager::StartUnit`: it enqueues the job and replies with
        // the job path straight away, without waiting for the job to run.
        let reply: EnqueueJobResult = client
            .call(
                "manager.enqueue",
                &EnqueueJobRequest {
                    name: "demo.service".to_string(),
                    job_type: "start".to_string(),
                    mode: "replace".to_string(),
                    reload_if_possible: false,
                },
            )
            .await
            .unwrap();
        assert_eq!(reply.job_id, 7);
        assert_eq!(reply.unit_name, "demo.service");

        // Completion then arrives asynchronously over the event stream:
        // `job.new` populates the mirror, `job.completed` drains it, and on a
        // live bus the same event emits `JobRemoved` carrying the result.
        // Guard with a timeout so a regression fails fast instead of hanging
        // the test suite.
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            mirror_sees_job_transition(&ctx, reply.job_id),
        )
        .await
        .expect("job.new then job.completed must arrive within 5s");

        // The mirror tracked the job and removed it once terminal.
        assert!(ctx.mirror.read().running_jobs().is_empty());

        event_task.abort();
    }
}