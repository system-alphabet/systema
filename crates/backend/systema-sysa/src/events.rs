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

    // job-completion → job.completed
    let (completion_tx, mut completion_rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::state::JobCompletion>();
    // job-new → job.new
    let (job_new_tx, mut job_new_rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::state::JobNewInfo>();
    // unit-loaded → unit.new
    let (unit_loaded_tx, mut unit_loaded_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    {
        let mut state = allocator.write();
        state.job_completion_tx = Some(completion_tx);
        state.job_new_tx = Some(job_new_tx);
        state.unit_loaded_tx = Some(unit_loaded_tx);
    }

    // job.completed
    let alloc = allocator.clone();
    tokio::spawn(async move {
        while let Some(completion) = completion_rx.recv().await {
            let mut buf = BytesMut::new();
            let _ = crate::snapshot::job_completion_event(
                completion.job_id,
                &completion.unit_name,
                &completion.result,
            )
            .encode(&mut buf);
            dispatch_control_event(
                &alloc,
                sysa::event_bus::EventTopic::JobCompleted,
                completion.unit_name,
                bytes::Bytes::from(buf),
            )
            .await;
        }
    });

    // job.new
    let alloc = allocator.clone();
    tokio::spawn(async move {
        while let Some(info) = job_new_rx.recv().await {
            let mut buf = BytesMut::new();
            let _ = sysa::proto::JobEvent {
                job_id: info.job_id,
                unit_name: info.unit_name.clone(),
                result: String::new(),
            }
            .encode(&mut buf);
            dispatch_control_event(
                &alloc,
                sysa::event_bus::EventTopic::JobNew,
                info.unit_name,
                bytes::Bytes::from(buf),
            )
            .await;
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

/// Load a single unit file from disk into the allocator registry.
///
/// Resolves aliases, canonicalises the name, and notifies the unit-loaded
/// channel so the event bus (and therefore the control plane) sees the new
/// unit.  Errors when the unit cannot be loaded.
pub fn load_unit_sync(allocator: &AllocatorHandle, name: &str) -> Result<()> {
    // Resolve known aliases first; the on-disk loader also canonicalises
    // symlink aliases, so the returned unit carries the canonical name.
    let requested = allocator.read().resolve_unit_name(name);
    let unit = crate::unit::loader::load_unit_flexible(&requested)?;
    let canonical = unit.name.clone();
    let mut state = allocator.write();
    state.units.insert(canonical.clone(), unit);
    state.rebuild_alias_map();
    // Notify the event layer so a unit.new is pushed to control-plane
    // consumers (bridge flavors) which register their Unit object.
    if let Some(ref tx) = state.unit_loaded_tx {
        let _ = tx.send(canonical);
    }
    Ok(())
}