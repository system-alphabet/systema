use anyhow::Result;
use sysa::worker_ipc::WorkerIpc;

use crate::controller::TargetController;
use crate::state::new_registry;

const WORKER_ID: &str = "system-t-1";
const WORKER_UNIT_TYPES: &[&str] = &["target"];

pub async fn run() -> Result<()> {
    let registry = new_registry();
    WorkerIpc::new(WORKER_ID, WORKER_UNIT_TYPES)
        .run(
            |_event_pub| TargetController::new(registry.clone()),
            |_, _| Ok(false),
        )
        .await
}
