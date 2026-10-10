use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::BytesMut;
use prost::Message as ProstMessage;
use tokio::sync::{mpsc, oneshot};
use tokio_util::codec::{FramedRead, FramedWrite};
use tracing::{debug, error, info, warn};

use crate::controller::{UnitController, UnitStatus};
use crate::ipc::{frame_stream, make_envelope, recv_envelope, send_envelope};
use crate::proto::*;

/// How long `WorkerIpc::run` waits, after sending `worker.exit`, for System
/// A to close the connection before giving up and exiting anyway.  Well
/// inside SysAInit's shutdown grace, so a stuck System A can never make a
/// worker overrun its deadline.
const DISCONNECT_GRACE: Duration = Duration::from_secs(2);

/// Handle for publishing unit state updates from within a unit controller.
///
/// Obtained via [`WorkerIpc::run`]'s factory closure and stored inside the
/// controller so it can emit `unit.state_update` messages at any time.
///
/// Publishing is asynchronous: each call serializes the update and spawns a
/// background task that sends it and waits for the `unit.state_update_ack`
/// from System A (5s timeout, up to 3 attempts, logging each failure, then
/// giving up without blocking the caller).  This keeps publish safe to call
/// from inside the reader task, which is also the task that resolves ACKs.
#[derive(Clone)]
pub struct EventPublisher {
    tx: mpsc::UnboundedSender<bytes::Bytes>,
    worker_id: String,
    next_request_id: Arc<AtomicU64>,
    pending_acks: Arc<Mutex<HashMap<u64, oneshot::Sender<UnitStateUpdateAck>>>>,
}

impl EventPublisher {
    pub fn new(
        tx: mpsc::UnboundedSender<bytes::Bytes>,
        worker_id: &str,
        pending_acks: Arc<Mutex<HashMap<u64, oneshot::Sender<UnitStateUpdateAck>>>>,
    ) -> Self {
        EventPublisher {
            tx,
            worker_id: worker_id.to_string(),
            next_request_id: Arc::new(AtomicU64::new(1)),
            pending_acks,
        }
    }

    /// Publish a `unit.state_update` envelope (fire-and-forget from the
    /// caller's perspective; ACK waiting and retries happen in the
    /// background).
    pub fn publish_unit_state_update(&self, units: Vec<UnitStatus>, full_snapshot: bool) {
        let update = UnitStateUpdate {
            units: units.into_iter().map(UnitStatus::into_proto).collect(),
            full_snapshot,
            seq: 0,
        };
        let publisher = self.clone();
        tokio::spawn(async move {
            const ACK_TIMEOUT: Duration = Duration::from_secs(5);
            const MAX_ATTEMPTS: u32 = 3;

            for attempt in 1..=MAX_ATTEMPTS {
                let request_id = publisher.next_request_id.fetch_add(1, Ordering::Relaxed);
                if request_id == 0 {
                    continue;
                }
                let env = match make_envelope(
                    request_id,
                    &publisher.worker_id,
                    "system-a",
                    "unit.state_update",
                    update.clone(),
                )
                .and_then(encode_envelope)
                {
                    Ok(env) => env,
                    Err(e) => {
                        error!("Failed to encode unit.state_update: {}", e);
                        return;
                    }
                };

                let (ack_tx, ack_rx) = oneshot::channel();
                publisher
                    .pending_acks
                    .lock()
                    .unwrap()
                    .insert(request_id, ack_tx);
                if publisher.tx.send(env).is_err() {
                    error!("Outgoing channel closed; cannot send unit.state_update");
                    return;
                }

                match tokio::time::timeout(ACK_TIMEOUT, ack_rx).await {
                    Ok(Ok(ack)) => {
                        if !ack.accepted {
                            error!(
                                "unit.state_update rejected by System A: {} (ignored units: {:?})",
                                ack.message, ack.ignored_units
                            );
                        }
                        return;
                    }
                    Ok(Err(_)) => {
                        error!("unit.state_update_ack channel closed");
                        return;
                    }
                    Err(_) => {
                        publisher.pending_acks.lock().unwrap().remove(&request_id);
                        error!(
                            "unit.state_update ACK timed out (attempt {}/{}); retrying",
                            attempt, MAX_ATTEMPTS
                        );
                    }
                }
            }
            error!("Giving up on unit.state_update after {MAX_ATTEMPTS} attempts");
        });
    }

    /// Send a fire-and-forget envelope with the given method name and payload
    /// (no ACK handling).  Used for one-way notifications such as the
    /// `timer.fired` message sent by the timer worker.
    pub fn send_envelope<P>(&self, method: &str, payload: P)
    where
        P: ProstMessage + Send + 'static,
    {
        let publisher = self.clone();
        let method = method.to_string();
        tokio::spawn(async move {
            let request_id = publisher.next_request_id.fetch_add(1, Ordering::Relaxed);
            if request_id == 0 {
                return;
            }
            let env = match make_envelope(
                request_id,
                &publisher.worker_id,
                "system-a",
                &method,
                payload,
            )
            .and_then(encode_envelope)
            {
                Ok(env) => env,
                Err(e) => {
                    error!("Failed to encode {method} envelope: {}", e);
                    return;
                }
            };
            if publisher.tx.send(env).is_err() {
                warn!("Outgoing channel closed; cannot send {method}");
            }
        });
    }

    /// Send a fire-and-forget envelope whose payload is an arbitrary raw byte
    /// string (NOT prost-encoded).  Use this for messages whose payload is
    /// plain text/bytes on the wire, e.g. `socket.request_fd` carrying a unit
    /// name.  Sending such payloads through [`send_envelope`] would prost-
    /// encode them (length prefix) and corrupt the receiver's parsing.
    pub fn send_envelope_bytes(&self, method: &str, payload: Vec<u8>) {
        let publisher = self.clone();
        let method = method.to_string();
        tokio::spawn(async move {
            let request_id = publisher.next_request_id.fetch_add(1, Ordering::Relaxed);
            if request_id == 0 {
                return;
            }
            let env = Envelope {
                request_id,
                source: publisher.worker_id.clone(),
                target: "system-a".to_string(),
                method: method.clone(),
                payload,
            };
            let env = match encode_envelope(env) {
                Ok(env) => env,
                Err(e) => {
                    error!("Failed to encode envelope: {}", e);
                    return;
                }
            };
            if publisher.tx.send(env).is_err() {
                warn!("Outgoing channel closed; cannot send {method}");
            }
        });
    }

    /// Send a reply envelope echoing an incoming `request_id` (e.g. the
    /// `unit.define_result` answer to a System A `unit.define` request).
    /// Fire-and-forget from the caller's perspective.
    pub fn send_reply<P>(&self, request_id: u64, method: &str, payload: P)
    where
        P: ProstMessage + Send + 'static,
    {
        let publisher = self.clone();
        let method = method.to_string();
        tokio::spawn(async move {
            let env = match make_envelope(
                request_id,
                &publisher.worker_id,
                "system-a",
                &method,
                payload,
            )
            .and_then(encode_envelope)
            {
                Ok(env) => env,
                Err(e) => {
                    error!("Failed to encode {method} envelope: {}", e);
                    return;
                }
            };
            if publisher.tx.send(env).is_err() {
                warn!("Outgoing channel closed; cannot send {method} reply");
            }
        });
    }

    /// Subscribe to `event.publish` notifications for specific units.
    /// An empty list subscribes to updates for *all* units.  Additive.
    pub fn subscribe_units(&self, unit_names: &[String]) {
        self.send_envelope(
            "event.subscribe",
            EventSubscribe {
                unit_names: unit_names.to_vec(),
            },
        );
    }

    /// Cancel a subscription to `event.publish` notifications.
    /// An empty list removes every subscription.  Subtractive.
    pub fn unsubscribe_units(&self, unit_names: &[String]) {
        self.send_envelope(
            "event.unsubscribe",
            EventUnsubscribe {
                unit_names: unit_names.to_vec(),
            },
        );
    }
}

fn encode_envelope(env: Envelope) -> Result<bytes::Bytes> {
    let mut buf = BytesMut::new();
    env.encode(&mut buf)
        .context(crate::l10n::t_("Failed to encode Envelope."))?;
    Ok(buf.freeze())
}

/// Local cleanup run on a shutdown signal *before* the `worker.exit`
/// handshake, produced by [`WorkerIpc::on_shutdown`]'s closure.  Boxed so
/// the hook can be an `async` block without generics infecting the struct.
type ShutdownCleanup = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Encapsulated worker IPC loop.
///
/// Handles connection, registration, method dispatch, state synchronisation,
/// and event publishing.  Workers only need to provide a [`UnitController`]
/// implementation and, optionally, a custom-envelope handler.
pub struct WorkerIpc {
    worker_id: String,
    unit_types: Vec<String>,
    supports_unit_define: bool,
    on_shutdown: Option<Arc<dyn Fn() -> ShutdownCleanup + Send + Sync>>,
    /// Test-only override of System A's socket path, so the fake System A in
    /// the tests need not live at the compiled-in `/run/...` location.
    #[cfg(test)]
    socket_path: Option<String>,
}

impl WorkerIpc {
    /// Create a new IPC worker with the given identity.
    ///
    /// `worker_id` is a unique identifier (e.g. `"system-s-1"`).
    /// `unit_types` lists the unit types this worker manages (e.g. `["service"]`).
    pub fn new(worker_id: &str, unit_types: &[&str]) -> Self {
        WorkerIpc {
            worker_id: worker_id.to_string(),
            unit_types: unit_types.iter().map(|s| s.to_string()).collect(),
            supports_unit_define: false,
            on_shutdown: None,
            #[cfg(test)]
            socket_path: None,
        }
    }

    /// Connect to `path` instead of the configured socket (tests only).
    #[cfg(test)]
    pub(crate) fn with_socket_path(mut self, path: impl Into<String>) -> Self {
        self.socket_path = Some(path.into());
        self
    }

    /// Arrange for `cleanup` to run on SIGTERM/SIGINT, **before** the
    /// `worker.exit` handshake and while the System A connection is still
    /// open.
    ///
    /// For a worker that has to finish local work on its way out (System S
    /// stopping its services) the ordering matters: state emitted while the
    /// cleanup runs must still reach System A, and only once it is done does
    /// the loop say goodbye.  The connection is polled concurrently with the
    /// cleanup, so a System A that disconnects first neither cancels nor
    /// stalls it.
    pub fn on_shutdown<F, Fut>(mut self, cleanup: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.on_shutdown = Some(Arc::new(move || Box::pin(cleanup())));
        self
    }

    /// Declare support for the `unit.define` protocol: System A may ask this
    /// worker to synthesize definitions for dynamic units of its types (e.g.
    /// System R answers slice-name requests with the parent-slice chain).
    pub fn supports_unit_define(mut self) -> Self {
        self.supports_unit_define = true;
        self
    }

    /// Run the worker IPC loop with automatic reconnection.
    ///
    /// `controller_factory` is called on **each** connection attempt with a
    /// fresh [`EventPublisher`] so the controller can publish events.
    ///
    /// `custom_handler` is invoked for envelopes that are neither `method.call`
    /// nor `state.sync_request`.  Return `Ok(true)` to mark the envelope as
    /// handled, `Ok(false)` to let the loop log a warning.
    ///
    /// The closure receives `(envelope, event_publisher)` and may capture
    /// local variables (e.g. an fdpass stream).
    ///
    /// Workers that need to share the [`EventPublisher`] with externally-spawned
    /// tasks (e.g. trigger forwarders, mount monitors) should pass a
    /// `controller_factory` that clones the publisher and spawns the tasks
    /// inside the closure.  The spawned tasks will stop naturally when the
    /// underlying channel is closed on the next reconnection attempt.
    ///
    /// # Graceful shutdown
    ///
    /// SIGTERM/SIGINT are handled *in here*: the loop first runs any
    /// [`Self::on_shutdown`] cleanup on the still-open connection, then
    /// queues a final `worker.exit` envelope, keeps the connection running so
    /// System A receives everything queued before it, and returns only once
    /// System A has closed the connection (or [`DISCONNECT_GRACE`] elapses).
    /// Callers must therefore not race this function against their own
    /// [`crate::signals::shutdown_signal()`] — that would drop the socket
    /// before the goodbye is exchanged.
    pub async fn run<C, H>(
        &self,
        controller_factory: impl Fn(EventPublisher) -> C,
        custom_handler: H,
    ) -> Result<()>
    where
        C: UnitController + Send + Sync + 'static,
        H: Fn(&Envelope, &EventPublisher) -> Result<bool>,
    {
        self.run_until_shutdown(
            crate::signals::shutdown_signal(),
            controller_factory,
            custom_handler,
        )
        .await
    }

    /// [`Self::run`] with the shutdown signal supplied by the caller.
    ///
    /// `run()` wires in the real SIGTERM/SIGINT listener; tests drive this
    /// one instead so they never raise a real signal in-process.
    async fn run_until_shutdown<S, C, H>(
        &self,
        shutdown: S,
        controller_factory: impl Fn(EventPublisher) -> C,
        custom_handler: H,
    ) -> Result<()>
    where
        S: Future<Output = &'static str>,
        C: UnitController + Send + Sync + 'static,
        H: Fn(&Envelope, &EventPublisher) -> Result<bool>,
    {
        // One listener for the whole loop.  Rebuilding it per attempt would
        // open a window in which a signal delivered between attempts is
        // never observed again — tokio does not replay signals.
        tokio::pin!(shutdown);

        let mut backoff = Duration::from_millis(500);
        loop {
            let (out_tx, out_rx) = mpsc::unbounded_channel::<bytes::Bytes>();
            // Clone kept aside for the shutdown path: it queues
            // `worker.exit` *behind* everything already queued, so System A
            // sees it only after this connection's outstanding messages.
            let exit_tx = out_tx.clone();

            let inner = self.try_run_inner(&controller_factory, &custom_handler, out_tx, out_rx);
            tokio::pin!(inner);

            // What ended this attempt: the connection itself, or us being
            // asked to leave (SIGTERM/SIGINT).
            enum Stopped {
                Finished(Result<()>),
                Signal(&'static str),
            }
            let stopped = tokio::select! {
                res = &mut inner => Stopped::Finished(res),
                sig = &mut shutdown => Stopped::Signal(sig),
            };

            match stopped {
                Stopped::Finished(Ok(())) => {
                    info!("Worker loop exited cleanly");
                    return Ok(());
                }
                Stopped::Finished(Err(e)) => {
                    warn!("Worker error: {}; reconnecting in {:?}", e, backoff);
                    // A signal must not be lost while we back off: there is
                    // no live connection, so there is nothing to disconnect.
                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {}
                        sig = &mut shutdown => {
                            info!("{sig}: no live System A connection; exiting");
                            return Ok(());
                        }
                    }
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
                Stopped::Signal(sig) => {
                    info!("{sig} received; shutting down locally before saying goodbye");

                    // Local cleanup runs first, *while the connection is
                    // still up*, so whatever state it reports still reaches
                    // System A.  `inner` is polled alongside it: a System A
                    // that disconnects early must neither cancel nor wedge
                    // the cleanup — we just note it and finish anyway.
                    let mut closed_during_cleanup = false;
                    if let Some(cleanup_fn) = &self.on_shutdown {
                        let cleanup = cleanup_fn();
                        tokio::pin!(cleanup);
                        tokio::select! {
                            _ = &mut cleanup => {}
                            _ = &mut inner => {
                                closed_during_cleanup = true;
                                info!(
                                    "System A closed the connection while shutting down locally"
                                );
                            }
                        }
                        if closed_during_cleanup {
                            // `inner` is spent (a future may not be polled
                            // after it completed); wait out the cleanup on
                            // its own.
                            cleanup.await;
                        }
                    }
                    if closed_during_cleanup {
                        // Nothing left to say goodbye on.
                        return Ok(());
                    }

                    info!("sending worker.exit and waiting for System A to disconnect");
                    match make_envelope(
                        0,
                        &self.worker_id,
                        "system-a",
                        "worker.exit",
                        WorkerExit {
                            reason: sig.to_string(),
                        },
                    )
                    .and_then(encode_envelope)
                    {
                        Ok(bytes) => {
                            let _ = exit_tx.send(bytes);
                        }
                        Err(e) => {
                            warn!("Failed to encode worker.exit: {}", e);
                        }
                    }
                    // Keep pumping the connection (writer flushes the exit
                    // frame, reader waits for System A's close) until the
                    // inner loop finishes on its own or we run out of
                    // patience — either way we leave after this.
                    match tokio::time::timeout(DISCONNECT_GRACE, &mut inner).await {
                        Ok(Ok(())) => info!("System A closed the connection; disconnected"),
                        Ok(Err(e)) => {
                            warn!("Connection failed while disconnecting: {e}; exiting")
                        }
                        Err(_) => warn!(
                            "System A did not close the connection within {DISCONNECT_GRACE:?}; exiting anyway"
                        ),
                    }
                    return Ok(());
                }
            }
        }
    }

    async fn try_run_inner<C, H>(
        &self,
        controller_factory: &impl Fn(EventPublisher) -> C,
        custom_handler: &H,
        out_tx: mpsc::UnboundedSender<bytes::Bytes>,
        mut out_rx: mpsc::UnboundedReceiver<bytes::Bytes>,
    ) -> Result<()>
    where
        C: UnitController + Send + Sync + 'static,
        H: Fn(&Envelope, &EventPublisher) -> Result<bool>,
    {
        use futures::SinkExt;
        use futures::StreamExt;
        use tokio_util::codec::LengthDelimitedCodec;

        #[cfg(test)]
        let socket_path = self
            .socket_path
            .clone()
            .unwrap_or_else(|| crate::paths::instance().ipc_socket_path.to_string());
        #[cfg(not(test))]
        let socket_path = crate::paths::instance().ipc_socket_path.to_string();

        info!("Connecting to System A at {}", socket_path);

        let stream = tokio::net::UnixStream::connect(&socket_path)
            .await
            .with_context(|| {
                crate::l10n::fmt(
                    crate::l10n::t_("Cannot connect to {path}."),
                    &[("path", socket_path.as_str())],
                )
            })?;

        info!("Connected to System A");

        let mut framed = frame_stream(stream);

        let reg = WorkerRegistration {
            worker_id: self.worker_id.clone(),
            unit_types: self.unit_types.clone(),
            supports_unit_define: self.supports_unit_define,
        };
        let env = make_envelope(0, &self.worker_id, "system-a", "worker.register", reg)?;
        send_envelope(&mut framed, &env).await?;

        let ack_env = recv_envelope(&mut framed).await?.ok_or_else(|| {
            anyhow::anyhow!(crate::l10n::t_("System A closed connection before ack."))
        })?;
        let ack = RegisterAck::decode(ack_env.payload.as_slice())?;
        if !ack.accepted {
            anyhow::bail!(crate::l10n::fmt(
                crate::l10n::t_("Registration rejected: {message}."),
                &[("message", &ack.message)],
            ));
        }
        info!("Registration accepted: {}", ack.message);

        let inner = framed.into_inner();
        let (reader_half, writer_half) = tokio::io::split(inner);

        let make_codec = || {
            LengthDelimitedCodec::builder()
                .max_frame_length(16 * 1024 * 1024)
                .new_codec()
        };
        let mut reader = FramedRead::new(reader_half, make_codec());
        let mut writer = FramedWrite::new(writer_half, make_codec());

        let pending_acks: Arc<Mutex<HashMap<u64, oneshot::Sender<UnitStateUpdateAck>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let event_publisher =
            EventPublisher::new(out_tx.clone(), &self.worker_id, pending_acks.clone());
        let controller = Arc::new(controller_factory(event_publisher.clone()));

        // Writer task: drain out_rx → write to socket
        let writer_task = async move {
            while let Some(msg) = out_rx.recv().await {
                writer
                    .send(msg)
                    .await
                    .context(crate::l10n::t_("Write to System A socket."))?;
            }
            Ok::<_, anyhow::Error>(())
        };

        // Reader task: read envelopes → dispatch
        let reader_task = async {
            // Publish the initial full snapshot so System A's runtime cache
            // is populated right after (re)connection.  The ACK is awaited
            // in the background — the reader loop below resolves it.
            {
                let units = controller.sync_state().await;
                event_publisher.publish_unit_state_update(units, true);
            }

            // Declare readiness: registration accepted and the initial full
            // snapshot sent.  System A marks the worker ready and broadcasts
            // `WORKER_READY=<worker_id>` on the notify channel (re-sent on
            // every reconnect, idempotent on the allocator side).
            match make_envelope(
                0,
                &self.worker_id,
                "system-a",
                "worker.ready",
                WorkerReady {},
            )
            .and_then(encode_envelope)
            {
                Ok(env) => {
                    if out_tx.send(env).is_err() {
                        warn!("Outgoing channel closed; cannot send worker.ready");
                    }
                }
                Err(e) => {
                    warn!("Failed to encode worker.ready: {}", e);
                }
            }

            loop {
                let bytes = match reader.next().await {
                    None => {
                        info!("System A closed the connection");
                        break;
                    }
                    Some(Err(e)) => {
                        warn!("Read error from System A: {}", e);
                        break;
                    }
                    Some(Ok(b)) => b,
                };

                let env = match Envelope::decode(bytes.freeze()) {
                    Ok(e) => e,
                    Err(e) => {
                        warn!("Failed to decode envelope: {} — disconnecting", e);
                        break;
                    }
                };

                match env.method.as_str() {
                    "method.call" => {
                        let call = match MethodCall::decode(env.payload.as_slice()) {
                            Ok(c) => c,
                            Err(e) => {
                                warn!("Failed to decode MethodCall: {} — disconnecting", e);
                                break;
                            }
                        };
                        debug!(
                            "Received method.call: method={} unit={}",
                            call.method, call.unit_name
                        );

                        // start/restart/reload may block for a long time
                        // (Type=notify waits for READY=1, up to
                        // TimeoutStartSec).  Run them in a spawned task so
                        // the reader loop keeps servicing other envelopes
                        // (state acks, status/stop requests, ...) meanwhile;
                        // the result is sent by the task itself.
                        let deferred =
                            matches!(call.method.as_str(), "start" | "restart" | "reload");
                        if deferred {
                            let controller = controller.clone();
                            let out_tx = out_tx.clone();
                            let worker_id = self.worker_id.clone();
                            let request_id = env.request_id;
                            tokio::spawn(async move {
                                let method_result = run_method(&*controller, &call).await;
                                match make_envelope(
                                    request_id,
                                    &worker_id,
                                    "system-a",
                                    "method.result",
                                    method_result,
                                )
                                .and_then(encode_envelope)
                                {
                                    Ok(encoded) => {
                                        if out_tx.send(encoded).is_err() {
                                            warn!(
                                                "Outgoing channel closed; cannot send method result for {}",
                                                call.method
                                            );
                                        }
                                    }
                                    Err(e) => {
                                        warn!(
                                            "Failed to encode method.result for {}: {}",
                                            call.method, e
                                        )
                                    }
                                }
                            });
                            continue;
                        }

                        let method_result = run_method(controller.as_ref(), &call).await;

                        match make_envelope(
                            env.request_id,
                            &self.worker_id,
                            "system-a",
                            "method.result",
                            method_result,
                        )
                        .and_then(encode_envelope)
                        {
                            Ok(encoded) => {
                                if out_tx.send(encoded).is_err() {
                                    warn!("Outgoing channel closed; cannot send method result");
                                    break;
                                }
                            }
                            Err(e) => warn!("Failed to encode method.result: {}", e),
                        }
                    }

                    "unit.state_update_ack" => {
                        if let Some(ack_tx) = pending_acks.lock().unwrap().remove(&env.request_id) {
                            let ack = UnitStateUpdateAck::decode(env.payload.as_slice())
                                .unwrap_or_else(|_| UnitStateUpdateAck {
                                    accepted: false,
                                    ignored_units: vec![],
                                    message: crate::l10n::t_("failed to decode ack").to_string(),
                                });
                            let _ = ack_tx.send(ack);
                        } else {
                            debug!(
                                "unit.state_update_ack with unknown request_id {} — ignoring",
                                env.request_id
                            );
                        }
                    }

                    "unit.sync_request" => {
                        let units = controller.sync_state().await;
                        let report = UnitSyncReport {
                            snapshot: Some(UnitStateUpdate {
                                units: units.into_iter().map(UnitStatus::into_proto).collect(),
                                full_snapshot: true,
                                seq: 0,
                            }),
                        };
                        match make_envelope(
                            env.request_id,
                            &self.worker_id,
                            "system-a",
                            "unit.sync_report",
                            report,
                        )
                        .and_then(encode_envelope)
                        {
                            Ok(encoded) => {
                                if out_tx.send(encoded).is_err() {
                                    warn!("Outgoing channel closed; cannot send sync report");
                                    break;
                                }
                            }
                            Err(e) => warn!("Failed to encode sync report: {}", e),
                        }
                    }

                    other => {
                        if !custom_handler(&env, &event_publisher)? {
                            warn!("Unexpected method from System A: {other}");
                        }
                    }
                }
            }
            Ok::<_, anyhow::Error>(())
        };

        tokio::select! {
            res = writer_task => {
                if let Err(e) = res { warn!("Writer task error: {}", e); }
            }
            res = reader_task => {
                if let Err(e) = res { warn!("Reader task error: {}", e); }
            }
        }

        Ok(())
    }
}

/// Execute a `method.call` against the unit controller and build the
/// `method.result` payload.  Long-running methods (start/restart/reload)
/// are invoked through a spawned task so the reader loop is never blocked.
async fn run_method<C: UnitController>(controller: &C, call: &MethodCall) -> MethodResult {
    let result = match call.method.as_str() {
        "status" => controller
            .status(&call.unit_name)
            .await
            .map(|s| s.encode_to_vec()),
        "start" => controller
            .start(&call.unit_name, &call.args, &call.invocation_id)
            .await
            .map(|()| vec![]),
        "stop" => controller.stop(&call.unit_name).await.map(|()| vec![]),
        "restart" => controller
            .restart(&call.unit_name, &call.args, &call.invocation_id)
            .await
            .map(|()| vec![]),
        "reload" => controller
            .reload(&call.unit_name, &call.args)
            .await
            .map(|()| vec![]),
        other => Err(anyhow::anyhow!(crate::l10n::fmt(
            crate::l10n::t_("unknown method: {other}"),
            &[("other", &other.to_string())]
        ))),
    };
    match result {
        Ok(payload) => {
            // For start/restart calls, if the controller returned an empty
            // payload (the common case for non-oneshot types), fetch the
            // current unit status and include it.  This ensures sysa's
            // `apply_state_to_cache()` overwrites the stale optimistic
            // update from `update_cache_on_task_result()` with the
            // controller's actual state (critical for oneshot services
            // that have already exited by the time the start job completes).
            let result =
                if payload.is_empty() && matches!(call.method.as_str(), "start" | "restart") {
                    controller
                        .status(&call.unit_name)
                        .await
                        .map(|s| s.encode_to_vec())
                        .unwrap_or(payload)
                } else {
                    payload
                };
            MethodResult {
                method: call.method.clone(),
                unit_name: call.unit_name.clone(),
                success: true,
                error: String::new(),
                result,
            }
        }
        Err(e) => MethodResult {
            method: call.method.clone(),
            unit_name: call.unit_name.clone(),
            success: false,
            error: e.to_string(),
            result: vec![],
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the fake System A observed, in arrival order — shared with the
    /// worker side so both can be interleaved in assertions.
    type Log = Arc<Mutex<Vec<String>>>;

    fn push(log: &Log, entry: impl Into<String>) {
        log.lock().unwrap().push(entry.into());
    }

    fn snapshot(log: &Log) -> Vec<String> {
        log.lock().unwrap().clone()
    }

    fn index_of(entries: &[String], needle: &str) -> usize {
        entries
            .iter()
            .position(|e| e == needle || e.starts_with(needle))
            .unwrap_or_else(|| panic!("{needle:?} missing from {entries:?}"))
    }

    /// Socket path for one test, in a directory tagged by test name so the
    /// tests of this binary never collide on it.
    fn test_socket(tag: &str) -> String {
        let dir = std::env::temp_dir().join(format!("sysa-worker-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("allocator.sock").to_str().unwrap().to_string()
    }

    /// A controller with nothing to manage: these tests exercise the IPC loop,
    /// not unit lifecycle.  `sync_state` keeps the trait's empty default.
    struct NoopController;

    #[async_trait::async_trait]
    impl UnitController for NoopController {
        async fn status(&self, _unit_name: &str) -> Result<UnitStatus> {
            anyhow::bail!("no units in this test")
        }

        async fn start(
            &self,
            _unit_name: &str,
            _config: &[u8],
            _invocation_id: &str,
        ) -> Result<()> {
            Ok(())
        }

        async fn stop(&self, _unit_name: &str) -> Result<()> {
            Ok(())
        }

        async fn restart(
            &self,
            _unit_name: &str,
            _config: &[u8],
            _invocation_id: &str,
        ) -> Result<()> {
            Ok(())
        }

        async fn reload(&self, _unit_name: &str, _config: &[u8]) -> Result<()> {
            Ok(())
        }
    }

    /// The final state a shutdown cleanup reports on its way out.
    fn cleanup_status() -> UnitStatus {
        UnitStatus {
            unit_name: "cleanup.service".to_string(),
            active_state: "inactive".to_string(),
            sub_state: "dead".to_string(),
            main_pid: 0,
            invocation_id: String::new(),
            extensions: HashMap::new(),
        }
    }

    /// Minimal System A: completes the handshake, acknowledges every state
    /// update, and closes the connection when `worker.exit` arrives — that
    /// close *is* the acknowledgement.  With `close_after_ready` it hangs up
    /// right after the handshake instead, modelling a System A that leaves
    /// first.
    async fn fake_system_a(
        listener: tokio::net::UnixListener,
        log: Log,
        ready: oneshot::Sender<()>,
        close_after_ready: Option<Duration>,
    ) -> anyhow::Result<()> {
        let (stream, _) = listener.accept().await?;
        let mut framed = frame_stream(stream);
        // Taken on the first `worker.ready` so the sender is moved only once.
        let mut ready = Some(ready);

        let reg = recv_envelope(&mut framed)
            .await?
            .context("worker never registered")?;
        assert_eq!(reg.method, "worker.register");
        let ack = RegisterAck {
            accepted: true,
            message: "Welcome".to_string(),
        };
        send_envelope(
            &mut framed,
            &make_envelope(1, "system-a", "system-t-1", "worker.ack", ack)?,
        )
        .await?;

        while let Some(env) = recv_envelope(&mut framed).await? {
            match env.method.as_str() {
                "unit.state_update" => {
                    let update = UnitStateUpdate::decode(env.payload.as_slice())?;
                    let names: Vec<&str> =
                        update.units.iter().map(|u| u.unit_name.as_str()).collect();
                    push(&log, format!("state_update:{}", names.join(",")));
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
                            "system-t-1",
                            "unit.state_update_ack",
                            ack,
                        )?,
                    )
                    .await?;
                }
                "worker.ready" => {
                    push(&log, "ready");
                    if let Some(tx) = ready.take() {
                        let _ = tx.send(());
                    }
                    if let Some(delay) = close_after_ready {
                        tokio::time::sleep(delay).await;
                        return Ok(()); // drop the socket
                    }
                }
                "worker.exit" => {
                    let exit = WorkerExit::decode(env.payload.as_slice())?;
                    push(&log, format!("worker.exit:{}", exit.reason));
                    return Ok(());
                }
                other => push(&log, format!("unexpected:{other}")),
            }
        }
        Ok(())
    }

    /// The full protocol: on a shutdown signal the worker runs its local
    /// cleanup *on the live connection* — System A sees the state it reports
    /// and acknowledges it — and only then says `worker.exit`, waits for
    /// System A to hang up, and returns.
    #[tokio::test]
    async fn shutdown_runs_cleanup_while_connected_then_says_goodbye() {
        let sock = test_socket("exit");

        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let (ready_tx, ready_rx) = oneshot::channel();
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let (a_log, a_sock) = (log.clone(), sock.clone());
        tokio::spawn(async move {
            let _ = fake_system_a(listener, a_log, ready_tx, None).await;
        });

        // The connection hands its publisher to the controller factory; the
        // shutdown cleanup reuses it to report final state.
        let publisher: Arc<Mutex<Option<EventPublisher>>> = Arc::new(Mutex::new(None));
        let (pub_for_factory, pub_for_cleanup) = (publisher.clone(), publisher.clone());
        let log_for_cleanup = log.clone();

        let ipc = WorkerIpc::new("system-t-1", &["timer"])
            .with_socket_path(a_sock)
            .on_shutdown(move || {
                let (publisher, log) = (pub_for_cleanup.clone(), log_for_cleanup.clone());
                async move {
                    push(&log, "cleanup-start");
                    let publisher = publisher.lock().unwrap().clone();
                    let ep = publisher.expect("factory installed the publisher on connect");
                    ep.publish_unit_state_update(vec![cleanup_status()], false);
                    // Done once System A has received (and logged) the update:
                    // without the connection still being open this never
                    // happens and the cleanup would time out.
                    for _ in 0..500 {
                        if snapshot(&log).iter().any(|e| e.contains("cleanup.service")) {
                            push(&log, "cleanup-done");
                            return;
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    panic!("System A never saw the cleanup state update");
                }
            });

        let signal = async move {
            ready_rx
                .await
                .expect("fake System A disappeared before the handshake finished");
            "SIGTERM"
        };

        let result = tokio::time::timeout(
            Duration::from_secs(10),
            ipc.run_until_shutdown(
                signal,
                move |event_pub| {
                    *pub_for_factory.lock().unwrap() = Some(event_pub);
                    NoopController
                },
                |_, _| Ok(false),
            ),
        )
        .await
        .expect("graceful shutdown must not hang");
        assert!(result.is_ok(), "worker must exit cleanly: {result:?}");

        let entries = snapshot(&log);
        let ready = index_of(&entries, "ready");
        let start = index_of(&entries, "cleanup-start");
        let update = entries
            .iter()
            .position(|e| e.contains("cleanup.service"))
            .unwrap_or_else(|| panic!("cleanup state update missing from {entries:?}"));
        let done = index_of(&entries, "cleanup-done");
        let exit = index_of(&entries, "worker.exit:SIGTERM");
        assert!(
            ready < start && start < update && update < done && done < exit,
            "cleanup must run, and be seen by System A, before the goodbye: {entries:?}"
        );
    }

    /// A shutdown signal with no System A behind the socket exits instead of
    /// sitting in the reconnect backoff forever.
    #[tokio::test]
    async fn shutdown_signal_without_a_connection_exits_promptly() {
        let ipc = WorkerIpc::new("system-t-1", &["timer"])
            .with_socket_path("/nonexistent/systema/allocator.sock");

        let result = tokio::time::timeout(
            Duration::from_secs(10),
            ipc.run_until_shutdown(async { "SIGTERM" }, |_| NoopController, |_, _| Ok(false)),
        )
        .await
        .expect("must not wait for a System A that will never come");
        assert!(result.is_ok(), "worker must exit cleanly: {result:?}");
    }

    /// System A hanging up *while* the local cleanup runs must neither cancel
    /// nor wedge it: the cleanup finishes, then the worker leaves.
    #[tokio::test]
    async fn cleanup_finishes_even_when_system_a_leaves_first() {
        let sock = test_socket("early-close");

        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let (ready_tx, ready_rx) = oneshot::channel();
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let (a_log, a_sock) = (log.clone(), sock.clone());
        tokio::spawn(async move {
            let _ =
                fake_system_a(listener, a_log, ready_tx, Some(Duration::from_millis(250))).await;
        });

        let log_for_cleanup = log.clone();
        let ipc = WorkerIpc::new("system-t-1", &["timer"])
            .with_socket_path(a_sock)
            .on_shutdown(move || {
                let log = log_for_cleanup.clone();
                async move {
                    push(&log, "cleanup-start");
                    // Local work that outlives System A's patience.
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    push(&log, "cleanup-done");
                }
            });

        let signal = async move {
            ready_rx
                .await
                .expect("fake System A disappeared before the handshake finished");
            "SIGTERM"
        };

        let result = tokio::time::timeout(
            Duration::from_secs(10),
            ipc.run_until_shutdown(signal, |_| NoopController, |_, _| Ok(false)),
        )
        .await
        .expect("shutdown must not wedge when System A leaves first");
        assert!(result.is_ok(), "worker must exit cleanly: {result:?}");

        let entries = snapshot(&log);
        index_of(&entries, "cleanup-start");
        index_of(&entries, "cleanup-done");
        assert!(
            !entries.iter().any(|e| e.starts_with("unexpected:")),
            "System A must not be sent anything unexpected: {entries:?}"
        );
    }
}
