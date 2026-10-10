use anyhow::Result;

use sysa::worker_ipc::WorkerIpc;

use crate::controller::DeviceController;
use crate::engine::{spawn_engine, EngineShared};

const WORKER_ID: &str = "system-d-1";
const WORKER_UNIT_TYPES: &[&str] = &["device"];

pub async fn run() -> Result<()> {
    let (shared, refresh_rx) = EngineShared::new();
    spawn_engine(shared.clone(), refresh_rx);

    WorkerIpc::new(WORKER_ID, WORKER_UNIT_TYPES)
        .run(
            |event_pub| {
                // Refresh the publisher used by the background engine on
                // every (re)connection and ask for an immediate refresh so
                // discovered devices are (re)committed and states re-pushed.
                *shared.event_pub.lock() = Some(event_pub.clone());
                let _ = shared.refresh_tx.send(());
                DeviceController::new(shared.clone())
            },
            |_, _| Ok(false),
        )
        .await
}