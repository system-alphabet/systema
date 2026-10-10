use anyhow::Result;

use sysa::worker_ipc::WorkerIpc;

use crate::controller::TimerController;
use crate::engine::{spawn_engine, EngineShared};

const WORKER_ID: &str = "system-c-1";
const WORKER_UNIT_TYPES: &[&str] = &["timer"];

pub async fn run() -> Result<()> {
    let shared = EngineShared::new();
    spawn_engine(shared.clone());
    WorkerIpc::new(WORKER_ID, WORKER_UNIT_TYPES)
        .run(
            |event_pub| {
                // Refresh the publisher used by the background engine on
                // every (re)connection.
                *shared.event_pub.lock() = Some(event_pub.clone());
                TimerController::new(shared.clone())
            },
            |_, _| Ok(false),
        )
        .await
}