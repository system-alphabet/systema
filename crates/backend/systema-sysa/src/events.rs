//! In-process lifecycle-event dispatch for the control plane.
//!
//! Bridges allocator-internal channels (job/unit lifecycle) onto the shared
//! `event_bus`, which control-port sessions subscribe to and forward to the
//! System Wrapper bridge flavors.

use anyhow::Result;
use bytes::BytesMut;
use prost::Message as ProstMessage;
use tracing::{debug, info};

use crate::state::AllocatorHandle;

/// Dispatch a control-plane lifecycle event onto the in-process event bus
/// (consumed by control-port sessions, which forward it to System Wrapper
/// bridge flavors).  Best-effort: a missing/unreachable bus is only logged.
async fn dispatch_control_event(
    allocator: &AllocatorHandle,
    topic: sysa::event_bus::EventTopic,
    unit_name: String,
    data: bytes::Bytes,
) {
    use sysa::event_bus::Event;
    let ev = Event {
        topic,
        unit_name,
        worker_id: "system-a".to_string(),
        timestamp: tokio::time::Instant::now(),
        data,
    };
    let bus = allocator.read().event_bus.clone();
    bus.read().await.dispatch(&ev).await;
}

/// Wire the in-process lifecycle channels and forward every event to the
/// event bus.  Runs forever once started; the allocator keeps it alive for
/// the whole process lifetime.
pub async fn run(allocator: AllocatorHandle) -> Result<()> {
    info!("Starting control-plane event dispatch");

    // job.new / job.completed — one ordered channel for both lifecycle
    // events.  A separate channel per event type cannot express the
    // invariant "a job is never seen completed before its creation":
    // independent consumers reorder events, and a `job.completed` that
    // arrives at the mirror before its `job.new` would silently drop (the
    // completion is a no-op there), then the late `job.new` would resurrect
    // the finished job as running forever.  Emission sites guarantee
    // program order (New before Completed) by sending through this single
    // channel.
    let (event_tx, mut event_rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::state::JobLifecycle>();
    // unit-loaded → unit.new
    let (unit_loaded_tx, mut unit_loaded_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    {
        let mut state = allocator.write();
        state.job_event_tx = Some(event_tx);
        state.unit_loaded_tx = Some(unit_loaded_tx);
    }

    // job.new + job.completed (in program order)
    let alloc = allocator.clone();
    tokio::spawn(async move {
        while let Some(ev) = event_rx.recv().await {
            let (topic, event) = match ev {
                crate::state::JobLifecycle::Completed(completion) => {
                    let mut buf = BytesMut::new();
                    let _ = crate::snapshot::job_completion_event(
                        completion.job_id,
                        &completion.unit_name,
                        &completion.result,
                    )
                    .encode(&mut buf);
                    (
                        sysa::event_bus::EventTopic::JobCompleted,
                        (completion.job_id, completion.unit_name, buf),
                    )
                }
                crate::state::JobLifecycle::New(info) => {
                    let mut buf = BytesMut::new();
                    let _ = sysa::proto::JobEvent {
                        job_id: info.job_id,
                        unit_name: info.unit_name.clone(),
                        result: String::new(),
                    }
                    .encode(&mut buf);
                    (
                        sysa::event_bus::EventTopic::JobNew,
                        (info.job_id, info.unit_name, buf),
                    )
                }
            };
            dispatch_control_event(&alloc, topic, event.1, bytes::Bytes::from(event.2)).await;
        }
    });

    // unit.new — a unit was loaded → push its full snapshot so the bridge
    // registers its Unit object.
    let alloc = allocator.clone();
    tokio::spawn(async move {
        while let Some(unit_name) = unit_loaded_rx.recv().await {
            let snap = crate::snapshot::unit_snapshot(&alloc.read(), &unit_name);
            let mut buf = BytesMut::new();
            let _ = snap.encode(&mut buf);
            dispatch_control_event(
                &alloc,
                sysa::event_bus::EventTopic::UnitNew,
                unit_name,
                bytes::Bytes::from(buf),
            )
            .await;
        }
        debug!("unit_loaded channel closed");
    });

    // Keep the dispatch machinery alive indefinitely.
    futures::future::pending::<()>().await;
    Ok(())
}
