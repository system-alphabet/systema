//! `org.freedesktop.systemd1.Activator` — the receiving half of systemd-style
//! D-Bus activation.
//!
//! Started with `--systemd-activation` (as our system bus daemon is),
//! dbus-daemon stops spawning `Exec=` from a `.service` file.  For every entry
//! carrying `SystemdService=` it instead emits
//!
//! ```text
//! sender      = org.freedesktop.DBus
//! destination = org.freedesktop.systemd1
//! path        = /org/freedesktop/DBus
//! interface   = org.freedesktop.systemd1.Activator
//! member      = ActivationRequest
//! body        = "<systemd unit name>"
//! ```
//!
//! and then waits — up to `service_start_timeout` — for that unit to claim its
//! bus name.  We own `org.freedesktop.systemd1`, so the signal lands here; if
//! nothing queues the job, *every* `SystemdService=` activation burns its full
//! 25s (`Failed to activate service 'org.freedesktop.UPower': timed out`).
//!
//! This mirrors systemd's `signal_activation_request()` (`src/core/dbus.c`):
//! queue a start job and return.  Success is observed by dbus-daemon as the bus
//! name appearing, not as a reply — a signal has none — so only enqueue
//! failures are reported, and those go back to the bus daemon as
//! `ActivationFailure`.
//!
//! The interface is deliberately never registered on the object server:
//! systemd only ever sends from it.

use std::sync::Arc;

use futures::StreamExt;
use tracing::{info, warn};
use zbus::interface;

use super::manager::ManagerInterface;
use super::BridgeContext;

/// Object path systemd emits `ActivationFailure` from.
const ACTIVATOR_PATH: &str = "/org/freedesktop/systemd1";

/// The bus daemon: sender of `ActivationRequest`, and the only destination
/// systemd unicasts `ActivationFailure` to.
const BUS_DAEMON: &str = "org.freedesktop.DBus";

/// Object path dbus-daemon emits `ActivationRequest` from.
const ACTIVATION_REQUEST_PATH: &str = "/org/freedesktop/DBus";

/// The `Activator` interface, carrying only `ActivationFailure`.
///
/// Never added to the object server — it exists so the signal is emitted with
/// this interface name rather than `org.freedesktop.systemd1.Manager`.
pub struct ActivatorInterface;

#[interface(name = "org.freedesktop.systemd1.Activator")]
impl ActivatorInterface {
    /// Tell the bus daemon an activation could not be queued, so its caller
    /// fails immediately instead of waiting out `service_start_timeout`.
    #[zbus(signal)]
    pub async fn activation_failure(
        ctxt: &zbus::SignalContext<'_>,
        name: String,
        error_name: String,
        error_message: String,
    ) -> zbus::Result<()>;
}

/// Match rule for dbus-daemon's activation request.
///
/// Every field is pinned, sender included, so a stray signal claiming this
/// interface from anyone else is not honoured.
fn activation_request_rule() -> zbus::OwnedMatchRule {
    zbus::MatchRule::builder()
        .msg_type(zbus::MessageType::Signal)
        .sender(BUS_DAEMON)
        .expect("static well-known bus name")
        .path(ACTIVATION_REQUEST_PATH)
        .expect("static object path")
        .interface("org.freedesktop.systemd1.Activator")
        .expect("static interface name")
        .member("ActivationRequest")
        .expect("static member name")
        .build()
        .into()
}

/// D-Bus error name for an `ActivationFailure` reply.
///
/// systemd forwards the `sd_bus_error` its own call produced
/// (`org.freedesktop.systemd1.NoSuchUnit`, …); the enqueue path only ever
/// surfaces these two shapes, so name them the way D-Bus names its own.
fn activation_error_name(err: &zbus::fdo::Error) -> &'static str {
    match err {
        zbus::fdo::Error::InvalidArgs(_) => "org.freedesktop.DBus.Error.InvalidArgs",
        _ => "org.freedesktop.DBus.Error.Failed",
    }
}

/// Serve `ActivationRequest` for the life of one bridge session.
///
/// Spawned beside the event loop and aborted with it, so the connection held
/// here cannot outlive the session and block the next one from claiming the
/// bus name.
pub async fn run(ctx: Arc<BridgeContext>) {
    let Some(conn) = super::connection(&ctx).cloned() else {
        return;
    };

    let mut stream =
        match zbus::MessageStream::for_match_rule(activation_request_rule(), &conn, None).await {
            Ok(stream) => stream,
            Err(e) => {
                warn!("Unable to subscribe to D-Bus activation requests: {e}");
                return;
            }
        };
    info!("Listening for D-Bus activation requests");

    // Serial, like systemd's main loop: each request is one control-port RPC.
    while let Some(msg) = stream.next().await {
        match msg {
            Ok(msg) => handle(&ctx, &msg).await,
            Err(e) => warn!("D-Bus activation stream error: {e}"),
        }
    }
}

/// Queue the requested unit, or report why we could not.
async fn handle(ctx: &Arc<BridgeContext>, msg: &zbus::Message) {
    let unit: String = match msg.body().deserialize() {
        Ok(unit) => unit,
        Err(e) => {
            warn!("Ignoring malformed ActivationRequest: {e}");
            return;
        }
    };

    let manager = ManagerInterface::new(ctx.clone());
    match manager.enqueue_for_activation(&unit).await {
        Ok(()) => info!("D-Bus activation: queued start of {unit}"),
        Err(err) => {
            let error_message = err.to_string();
            warn!("D-Bus activation of {unit} failed: {error_message}");
            emit_activation_failure(ctx, &unit, activation_error_name(&err), &error_message).await;
        }
    }
}

/// Best effort: an unsent failure only costs the caller its timeout.
async fn emit_activation_failure(
    ctx: &BridgeContext,
    unit: &str,
    error_name: &str,
    error_message: &str,
) {
    let (Some(conn), Ok(destination)) = (
        super::connection(ctx),
        zbus::names::BusName::try_from(BUS_DAEMON),
    ) else {
        return;
    };
    let Ok(ctxt) = zbus::SignalContext::new(conn, ACTIVATOR_PATH) else {
        return;
    };
    let ctxt = ctxt.set_destination(destination);

    if let Err(e) = ActivatorInterface::activation_failure(
        &ctxt,
        unit.to_string(),
        error_name.to_string(),
        error_message.to_string(),
    )
    .await
    {
        warn!("Unable to report activation failure for {unit}: {e}");
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use prost::Message as _;
    use tokio::net::UnixStream;

    use sysa::ipc::{make_envelope, recv_envelope, send_envelope, EnvelopeFramed};
    use sysa::proto::{EnqueueJobRequest, EnqueueJobResult, ManagerHelloResult};
    use systema_sysw_common::ControlClient;

    use super::*;
    use crate::dbus::BridgeContext;
    use crate::mirror::{MirrorHandle, UnitMirror};

    /// Control-port server that records every `manager.enqueue` it is asked
    /// to perform and never reports the job terminal — so a caller that waits
    /// for completion stalls, which is exactly what activation must not do.
    async fn recording_server(
        mut framed: EnvelopeFramed,
        seen: Arc<Mutex<Vec<EnqueueJobRequest>>>,
    ) -> anyhow::Result<()> {
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
                            unit_name: req.name.clone(),
                        },
                    )?;
                    send_envelope(&mut framed, &reply).await?;
                    seen.lock().unwrap().push(req);
                }
                other => {
                    let reply = make_envelope(
                        env.request_id,
                        "system-a",
                        "mock",
                        "manager.error",
                        sysa::proto::SimpleManagerResult {
                            success: false,
                            message: format!("mock: no handler for {other}"),
                        },
                    )?;
                    send_envelope(&mut framed, &reply).await?;
                }
            }
        }
    }

    async fn context() -> (Arc<BridgeContext>, Arc<Mutex<Vec<EnqueueJobRequest>>>) {
        let (server, client) = UnixStream::pair().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        tokio::spawn(recording_server(
            sysa::ipc::frame_stream(server),
            seen.clone(),
        ));

        let client = Arc::new(
            ControlClient::connect_on(client, "test", None)
                .await
                .unwrap(),
        );
        let mirror: MirrorHandle = Arc::new(parking_lot::RwLock::new(UnitMirror::new()));
        let ctx = BridgeContext::new(mirror, client);
        (ctx, seen)
    }

    #[test]
    fn activation_request_rule_pins_every_field_dbus_daemon_sends() {
        let rule = activation_request_rule();

        assert_eq!(rule.msg_type(), Some(zbus::MessageType::Signal));
        assert_eq!(
            rule.sender().map(|s| s.to_string()),
            Some(BUS_DAEMON.to_string())
        );
        assert_eq!(
            rule.interface().map(|i| i.to_string()),
            Some("org.freedesktop.systemd1.Activator".to_string())
        );
        assert_eq!(
            rule.member().map(|m| m.to_string()),
            Some("ActivationRequest".to_string())
        );
        // `path()`, unlike its siblings, is only exposed as a `PathSpec`.
        assert!(
            matches!(rule.path_spec(), Some(zbus::MatchRulePathSpec::Path(p))
                if p.as_str() == ACTIVATION_REQUEST_PATH),
            "expected an exact path match, got {:?}",
            rule.path_spec()
        );
    }

    #[test]
    fn activation_failure_is_emitted_on_the_activator_interface() {
        use zbus::object_server::Interface;

        // Putting the signal on `ManagerInterface` would stamp it with
        // `org.freedesktop.systemd1.Manager`, which dbus-daemon does not
        // recognise as an activation reply.
        assert_eq!(
            <ActivatorInterface as Interface>::name().as_str(),
            "org.freedesktop.systemd1.Activator"
        );
    }

    #[test]
    fn failure_names_follow_dbus_conventions() {
        let invalid_args = zbus::fdo::Error::InvalidArgs("nope".to_string());
        assert_eq!(
            activation_error_name(&invalid_args),
            "org.freedesktop.DBus.Error.InvalidArgs"
        );

        let failed = zbus::fdo::Error::Failed("unit not found".to_string());
        assert_eq!(
            activation_error_name(&failed),
            "org.freedesktop.DBus.Error.Failed"
        );
    }

    /// The listener must enqueue and move on: a signal has no reply, so
    /// waiting for the job would only stall later activations while telling
    /// dbus-daemon nothing new.
    #[tokio::test]
    async fn activation_queues_a_start_job_without_waiting_for_it() {
        let (ctx, seen) = context().await;
        let manager = ManagerInterface::new(ctx.clone());

        let queued = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            manager.enqueue_for_activation("upower.service"),
        )
        .await
        .expect("activation must not block on job completion");

        assert!(queued.is_ok());
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].name, "upower.service");
        assert_eq!(seen[0].job_type, "start");
        assert_eq!(seen[0].mode, "replace");
        assert!(!seen[0].reload_if_possible);
    }

    /// Exercises the whole signal path with a synthetic `ActivationRequest`,
    /// including the `s` body parse that turns a bus name into a unit name.
    #[tokio::test]
    async fn activation_request_message_is_queued() {
        let (ctx, seen) = context().await;

        let msg = zbus::Message::signal(
            ACTIVATION_REQUEST_PATH,
            "org.freedesktop.systemd1.Activator",
            "ActivationRequest",
        )
        .expect("valid signal header")
        .build(&("upower.service",))
        .expect("valid signal body");

        handle(&ctx, &msg).await;

        assert_eq!(seen.lock().unwrap().len(), 1);
        assert_eq!(seen.lock().unwrap()[0].name, "upower.service");
    }

    /// A body we cannot read must be dropped, not turned into a start job.
    #[tokio::test]
    async fn malformed_activation_request_is_ignored() {
        let (ctx, seen) = context().await;

        let msg = zbus::Message::signal(
            ACTIVATION_REQUEST_PATH,
            "org.freedesktop.systemd1.Activator",
            "ActivationRequest",
        )
        .expect("valid signal header")
        .build(&(42_u32,))
        .expect("valid signal body");

        handle(&ctx, &msg).await;

        assert!(seen.lock().unwrap().is_empty());
    }
}
