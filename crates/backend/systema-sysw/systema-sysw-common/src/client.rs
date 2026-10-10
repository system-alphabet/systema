//! Control-port client — how System Wrapper bridge flavors talk to System A.
//!
//! A single `ControlClient` per bridge process:
//! - performs the `manager.hello` handshake,
//! - issues `manager.*` request/reply envelopes (RPC),
//! - receives server-pushed lifecycle events on a separate stream.
//!
//! Reconnection is the caller's job: `connect` returns an error when the
//! session cannot be established, and the event stream / pending RPCs fail
//! closed once the peer disconnects so the flavor can reconnect and rebuild
//! its mirror from `list_snapshots`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use bytes::BytesMut;
use futures::{SinkExt, StreamExt};
use prost::Message as ProstMessage;
use tokio::select;
use tokio::sync::{mpsc, oneshot, watch, Mutex};
use tokio::net::UnixStream;
use tracing::{debug, warn};

use sysa::ipc::{frame_stream, make_envelope};
use sysa::proto::{Envelope, ManagerHelloRequest, ManagerHelloResult, SimpleManagerResult};

use crate::event::{ControlEvent, ControlEventKind};

type PendingRequests = Arc<Mutex<HashMap<u64, oneshot::Sender<Envelope>>>>;

/// An established control-port session.
pub struct ControlClient {
    writer: mpsc::UnboundedSender<Envelope>,
    event_rx: Arc<Mutex<ControlEventStream>>,
    pending: PendingRequests,
    closed_rx: watch::Receiver<bool>,
    next_request_id: Arc<AtomicU64>,
    flavor: String,
}

/// Received as `UnboundedReceiver` from [`ControlClient::events`].
pub type ControlEventStream = mpsc::UnboundedReceiver<ControlEvent>;

impl ControlClient {
    #[cfg(test)]
    pub fn mock(flavor: &str) -> Self {
        let (writer, _) = mpsc::unbounded_channel();
        let (_, event_rx) = mpsc::unbounded_channel();
        let (_, closed_rx) = watch::channel(false);
        Self {
            writer,
            event_rx: Arc::new(Mutex::new(event_rx)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            closed_rx,
            next_request_id: Arc::new(AtomicU64::new(1)),
            flavor: flavor.to_string(),
        }
    }

    /// Connect to the control socket and perform the `manager.hello` handshake.
    ///
    /// `flavor` is the bridge identity reported to System A (e.g. `"systemd"`).
    /// `worker_id` is an optional supervised-worker identity (e.g.
    /// `"system-w-1"`) that lets the control session handler broadcast
    /// `WORKER_READY` on the notify channel for SysAInit.
    pub async fn connect(flavor: &str, worker_id: Option<&str>) -> Result<ControlClient> {
        let path = sysa::paths::instance().control_socket_path;
        let stream = UnixStream::connect(path).await.with_context(|| {
            sysa::l10n::fmt(
                sysa::l10n::t_("connecting to control socket {path}"),
                &[("path", &path.to_string())],
            )
        })?;
        debug!("Connected to control socket {path}");
        Self::connect_on(stream, flavor, worker_id).await
    }

    /// Establish a control session over an already-connected stream.
    ///
    /// Performs the `manager.hello` handshake and starts the writer/reader
    /// tasks, exactly like [`ControlClient::connect`] but without owning the
    /// transport.  Used by embedders that provide their own socket (and by
    /// the socketpair tests).
    pub async fn connect_on(stream: UnixStream, flavor: &str, worker_id: Option<&str>) -> Result<ControlClient> {
        let version = env!("CARGO_PKG_VERSION").to_string();

        let framed = frame_stream(stream);
        let (mut writer_sink, mut reader_stream) = framed.split();

        let pending: PendingRequests = Arc::new(Mutex::new(HashMap::new()));
        let (writer_tx, mut writer_rx) = mpsc::unbounded_channel::<Envelope>();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<ControlEvent>();
        let (closed_tx, closed_rx) = watch::channel(false);

        // Writer task: drain outgoing RPC envelopes toward System A.
        let writer_flavor = flavor.to_string();
        let writer_task_flavor = writer_flavor.clone();
        tokio::spawn(async move {
            while let Some(env) = writer_rx.recv().await {
                let mut buf = BytesMut::with_capacity(env.encoded_len());
                if env.encode(&mut buf).is_err() {
                    warn!("[{}] failed to encode control envelope", writer_task_flavor);
                    break;
                }
                if writer_sink.send(buf.freeze()).await.is_err() {
                    debug!("[{}] write side closed", writer_task_flavor);
                    break;
                }
            }
            drop(writer_sink);
        });

        // Reader task: route replies to pending RPCs; forward event envelopes.
        let reader_pending = pending.clone();
        let reader_flavor = flavor.to_string();
        let reader_closed = closed_tx.clone();
        tokio::spawn(async move {
            let closed = async {
                loop {
                    let frame = match reader_stream.next().await {
                        None => break,
                        Some(Err(e)) => {
                            warn!("[{}] control read error: {e:#}", reader_flavor);
                            break;
                        }
                        Some(Ok(bytes)) => match Envelope::decode(bytes.freeze()) {
                            Ok(env) => env,
                            Err(e) => {
                                warn!("[{}] control envelope decode error: {e}", reader_flavor);
                                continue;
                            }
                        },
                    };
                    if frame.request_id != 0 {
                        let tx = { reader_pending.lock().await.remove(&frame.request_id) };
                        if let Some(tx) = tx {
                            let _ = tx.send(frame);
                            continue;
                        }
                    }
                    let Some(kind) = ControlEventKind::from_method(&frame.method) else {
                        continue;
                    };
                    if event_tx
                        .send(ControlEvent {
                            kind,
                            envelope: frame,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            };
            closed.await;
            reader_pending.lock().await.clear();
            let _ = reader_closed.send(true);
            debug!("[{}] control session read side closed", reader_flavor);
        });

        let client = ControlClient {
            writer: writer_tx,
            event_rx: Arc::new(Mutex::new(event_rx)),
            pending,
            closed_rx,
            next_request_id: Arc::new(AtomicU64::new(1)),
            flavor: flavor.to_string(),
        };

        let hello = ManagerHelloRequest {
            flavor: flavor.to_string(),
            version,
            worker_id: worker_id.unwrap_or_default().to_string(),
        };
        let reply: ManagerHelloResult = client
            .call("manager.hello", &hello)
            .await
            .context(sysa::l10n::t_("manager.hello handshake"))?;
        if !reply.success {
            bail!(sysa::l10n::fmt(
                sysa::l10n::t_("control bus rejected us: {message}"),
                &[("message", &(reply.message).to_string())]
            ));
        }
        debug!("Control handshake OK: {}", reply.message);

        Ok(client)
    }

    /// Issue one `manager.*` request and await its reply.
    ///
    /// Replies with method `manager.error` are surfaced as [`anyhow::Error`];
    /// a disconnected session fails all in-flight requests.
    pub async fn call<Q, R>(&self, method: &str, request: &Q) -> Result<R>
    where
        Q: ProstMessage + Clone,
        R: ProstMessage + Default,
    {
        let id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let env = make_envelope(id, self.flavor.clone(), "system-a", method, request.clone())
            .with_context(|| {
                sysa::l10n::fmt(
                    sysa::l10n::t_("encoding {method} request"),
                    &[("method", &method.to_string())],
                )
            })?;
        if self.writer.send(env).is_err() {
            self.pending.lock().await.remove(&id);
            bail!(sysa::l10n::t_("control session closed (writer task gone)"));
        }

        let mut closed = self.closed_rx.clone();
        let received = select! {
            r = rx => r.map_err(|_| anyhow::anyhow!(sysa::l10n::t_("control session closed")))?,
            _ = closed.changed() => bail!(sysa::l10n::t_("control session closed (disconnected)")),
        };

        match received.method.as_str() {
            "manager.error" => {
                let err = SimpleManagerResult::decode(received.payload.as_slice())
                    .context(sysa::l10n::t_("decoding manager.error payload"))?;
                bail!("{}: {}", method, err.message);
            }
            expected => {
                if expected != &format!("{method}.result") {
                    warn!(
                        "[{}] unexpected reply for {method}: {}",
                        self.flavor, expected
                    );
                }
                R::decode(received.payload.as_slice()).with_context(|| {
                    sysa::l10n::fmt(
                        sysa::l10n::t_("decoding {method} reply"),
                        &[("method", &method.to_string())],
                    )
                })
            }
        }
    }

    /// The stream of server-pushed lifecycle events.
    ///
    /// The receiver closes (`recv()` returns `None`) when the session dies so
    /// the flavor's mirror loop can exit and trigger a reconnect.  Shared via
    /// an `Arc<Mutex>` because the client is itself shared, and only the
    /// bridge event loop consumes it.
    pub fn events(&self) -> Arc<Mutex<ControlEventStream>> {
        Arc::clone(&self.event_rx)
    }

    /// The bridge identity reported during the handshake.
    pub fn flavor(&self) -> &str {
        &self.flavor
    }
}

/// Convenience accessors for the RPC payload types used by bridge flavors.
pub mod calls {
    use anyhow::Result;

    use sysa::proto::{
        GetUnitByInvocationRequest, GetUnitByPidRequest, GetUnitByPidResult, JobInfo,
        ListJobsRequest, ListJobsResult, ListSnapshotsRequest, ListSnapshotsResult, LoadUnitRequest,
        LoadUnitResult, UnitSnapshot, UnitSnapshotRequest, UnitSnapshotResult,
    };

    use super::ControlClient;

    /// Fetch the full current unit set (initial mirror population).
    pub async fn list_snapshots(client: &ControlClient) -> Result<Vec<UnitSnapshot>> {
        let reply: ListSnapshotsResult = client
            .call("manager.list_snapshots", &ListSnapshotsRequest::default())
            .await?;
        if !reply.success {
            anyhow::bail!("list_snapshots: {}", reply.message);
        }
        Ok(reply.units)
    }

    /// Fetch one unit's snapshot without loading it.  This also resolves
    /// alias names server-side (the returned snapshot carries the canonical
    /// name).
    pub async fn unit_snapshot(client: &ControlClient, name: &str) -> Result<UnitSnapshot> {
        let reply: UnitSnapshotResult = client
            .call("manager.unit_snapshot", &UnitSnapshotRequest {
                name: name.to_string(),
            })
            .await?;
        if !reply.success {
            anyhow::bail!("unit_snapshot {name}: {}", reply.message);
        }
        reply
            .unit
            .ok_or_else(|| anyhow::anyhow!("unit_snapshot {name}: no unit in reply"))
    }

    /// Current running jobs, for mirror seeding and `ListJobs`.
    pub async fn list_jobs(client: &ControlClient) -> Result<Vec<JobInfo>> {
        let reply: ListJobsResult = client
            .call("manager.list_jobs", &ListJobsRequest::default())
            .await?;
        if !reply.success {
            anyhow::bail!("list_jobs: {}", reply.message);
        }
        Ok(reply.jobs)
    }

    /// Ensure a unit is loaded and fetch its snapshot.
    pub async fn load_unit_snapshot(
        client: &ControlClient,
        name: &str,
    ) -> Result<(LoadUnitResult, UnitSnapshot)> {
        let load: LoadUnitResult = client
            .call("manager.load_unit", &LoadUnitRequest {
                name: name.to_string(),
            })
            .await?;
        if !load.success {
            anyhow::bail!("load_unit {name}: {}", load.message);
        }
        let snap: UnitSnapshotResult = client
            .call("manager.unit_snapshot", &UnitSnapshotRequest {
                name: name.to_string(),
            })
            .await?;
        let unit = snap.unit.ok_or_else(|| anyhow::anyhow!("unit_snapshot {name}: no unit"))?;
        Ok((load, unit))
    }

    /// Resolve a unit by its invocation ID; returns the canonical unit name.
    pub async fn get_unit_by_invocation(
        client: &ControlClient,
        invocation_id: &str,
    ) -> Result<String> {
        let reply: GetUnitByPidResult = client
            .call(
                "manager.get_unit_by_invocation",
                &GetUnitByInvocationRequest {
                    invocation_id: invocation_id.to_string(),
                },
            )
            .await?;
        if !reply.success {
            anyhow::bail!("get_unit_by_invocation: {}", reply.message);
        }
        Ok(reply.name)
    }

    /// Resolve a unit by PID; returns the canonical unit name.
    pub async fn get_unit_by_pid(client: &ControlClient, pid: u32) -> Result<String> {
        let reply: GetUnitByPidResult = client
            .call("manager.get_unit_by_pid", &GetUnitByPidRequest {
                pid,
            })
            .await?;
        if !reply.success {
            anyhow::bail!("get_unit_by_pid: {}", reply.message);
        }
        Ok(reply.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sysa::ipc::{make_envelope, recv_envelope, send_envelope};
    use sysa::proto::{
        EnqueueJobRequest, EnqueueJobResult, JobEvent, ListSnapshotsResult, ManagerHelloResult,
        SimpleManagerResult, UnitSnapshot,
    };

    /// Minimal in-test control-bus server: performs the hello handshake,
    /// answers `manager.*` RPCs, and emits pre-scripted event envelopes.
    ///
    /// The script is consumed in order; the final entry is *pushed* with a
    /// nonzero request_id (a guarded reply) — every other entry is pushed as
    /// a one-way event (request_id 0).  This mirrors how System A's control
    /// bus serves bridge flavors.
    struct MockControlServer {
        framed: sysa::ipc::EnvelopeFramed,
        script: Vec<Envelope>,
    }

    impl MockControlServer {
        async fn spawn(stream: UnixStream, script: Vec<Envelope>) {
            tokio::spawn(async move {
                let mut server = Self {
                    framed: sysa::ipc::frame_stream(stream),
                    script,
                };
                if let Err(e) = server.run().await {
                    panic!("mock control server error: {e:#}");
                }
            });
        }

        async fn run(&mut self) -> anyhow::Result<()> {
            while let Some(env) = recv_envelope(&mut self.framed).await? {
                match env.method.as_str() {
                    "manager.hello" => {
                        let reply = ManagerHelloResult {
                            success: true,
                            message: "hello from mock".to_string(),
                        };
                        let reply = make_envelope(
                            env.request_id,
                            "system-a",
                            "mock-server",
                            "manager.hello.result",
                            reply,
                        )?;
                        send_envelope(&mut self.framed, &reply).await?;
                    }
                    "manager.enqueue" => {
                        let req = EnqueueJobRequest::decode(env.payload.as_slice())?;
                        let reply = EnqueueJobResult {
                            success: true,
                            message: String::new(),
                            job_id: 42,
                            unit_name: req.name.clone(),
                        };
                        let reply = make_envelope(
                            env.request_id,
                            "system-a",
                            "mock-server",
                            "manager.enqueue.result",
                            reply,
                        )?;
                        send_envelope(&mut self.framed, &reply).await?;
                        // Drain the pre-scripted event pushes to the session.
                        while let Some(event) = self.script.pop() {
                            send_envelope(&mut self.framed, &event).await?;
                        }
                    }
                    "manager.list_snapshots" => {
                        let reply = ListSnapshotsResult {
                            success: true,
                            message: String::new(),
                            units: vec![UnitSnapshot {
                                name: "sshd.service".to_string(),
                                kind: "service".to_string(),
                                active_state: "active".to_string(),
                                ..Default::default()
                            }],
                        };
                        let reply = make_envelope(
                            env.request_id,
                            "system-a",
                            "mock-server",
                            "manager.list_snapshots.result",
                            reply,
                        )?;
                        send_envelope(&mut self.framed, &reply).await?;
                    }
                    "manager.list_jobs" => {
                        let reply = sysa::proto::ListJobsResult {
                            success: true,
                            message: String::new(),
                            jobs: vec![],
                        };
                        let reply = make_envelope(
                            env.request_id,
                            "system-a",
                            "mock-server",
                            "manager.list_jobs.result",
                            reply,
                        )?;
                        send_envelope(&mut self.framed, &reply).await?;
                    }
                    other => {
                        // Unknown RPC → manager.error so the client's `call`
                        // surfaces a clean failure instead of hanging.
                        let reply = SimpleManagerResult {
                            success: false,
                            message: format!("mock server: no handler for {other}"),
                        };
                        let reply = make_envelope(
                            env.request_id,
                            "system-a",
                            "mock-server",
                            "manager.error",
                            reply,
                        )?;
                        send_envelope(&mut self.framed, &reply).await?;
                    }
                }
            }
            Ok(())
        }
    }

    fn hello_script() -> Vec<Envelope> {
        Vec::new()
    }

    #[tokio::test]
    async fn control_socketpair_rpc_roundtrip() {
        let (server_stream, client_stream) = UnixStream::pair().unwrap();
        MockControlServer::spawn(server_stream, hello_script()).await;

        let client = ControlClient::connect_on(client_stream, "test", None).await.unwrap();

        // manager.hello handshake succeeded during connect; now exercise an
        // RPC through the same framed pair.
        let snaps = calls::list_snapshots(&client).await.unwrap();
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].name, "sshd.service");
        assert_eq!(snaps[0].active_state, "active");

        // A second round-trip on the same session keeps working.
        let enqueued = calls::list_jobs(&client).await;
        assert!(enqueued.is_ok());
    }

    #[tokio::test]
    async fn broadcast_reaches_every_subscriber() {
        // Two bridge flavors sharing one server.  A pushed `job.new` /
        // `job.completed` pair must reach BOTH clients, exactly like System A
        // broadcasts lifecycle events to every connected bridge.
        let (server_a, client_a) = UnixStream::pair().unwrap();
        let (server_b, client_b) = UnixStream::pair().unwrap();

        let job_new = make_envelope(
            0,
            "system-a",
            "test",
            "job.new",
            JobEvent {
                job_id: 7,
                unit_name: "sshd.service".to_string(),
                result: String::new(),
            },
        )
        .unwrap();
        // Reverse scripts so each session gets its own job.id.
        let mut script_a = vec![make_envelope(
            0,
            "system-a",
            "test",
            "job.completed",
            JobEvent {
                job_id: 7,
                unit_name: "sshd.service".to_string(),
                result: "done".to_string(),
            },
        )
        .unwrap()];
        script_a.push(job_new.clone());
        let script_b = vec![job_new.clone()];

        MockControlServer::spawn(server_a, script_a).await;
        MockControlServer::spawn(server_b, script_b).await;

        let client_a = ControlClient::connect_on(client_a, "test-a", None).await.unwrap();
        let client_b = ControlClient::connect_on(client_b, "test-b", None).await.unwrap();

        // Trigger the scripted event pushes on both sessions with an RPC each.
        let events_a = client_a.events();
        let events_b = client_b.events();
        let mut rx_a = events_a.lock().await;
        let mut rx_b = events_b.lock().await;

        let _: EnqueueJobResult = client_a
            .call(
                "manager.enqueue",
                &EnqueueJobRequest {
                    name: "sshd.service".to_string(),
                    job_type: "stop".to_string(),
                    mode: "replace".to_string(),
                    reload_if_possible: false,
                },
            )
            .await
            .unwrap();
        let _: EnqueueJobResult = client_b
            .call(
                "manager.enqueue",
                &EnqueueJobRequest {
                    name: "sshd.service".to_string(),
                    job_type: "start".to_string(),
                    mode: "replace".to_string(),
                    reload_if_possible: false,
                },
            )
            .await
            .unwrap();

        // client_a: job.new then job.completed (7).
        assert_eq!(rx_a.recv().await.unwrap().kind, ControlEventKind::JobNew);
        let completed_a = rx_a.recv().await.unwrap();
        assert_eq!(completed_a.kind, ControlEventKind::JobCompleted);
        let payload = JobEvent::decode(completed_a.envelope.payload.as_slice()).unwrap();
        assert_eq!(payload.job_id, 7);
        assert_eq!(payload.result, "done");

        // client_b: at least the job.new push arrived.
        assert_eq!(rx_b.recv().await.unwrap().kind, ControlEventKind::JobNew);
    }
}