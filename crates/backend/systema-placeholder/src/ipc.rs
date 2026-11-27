use anyhow::Result;
use sysa::worker_ipc::WorkerIpc;

use crate::controller::PlaceholderController;
use crate::state::{new_registry, StateMode, UnitState};

const WORKER_ID: &str = "system-p-1";

pub async fn run(
    unit_types: &[&str],
    state_mode: StateMode,
    default_state: UnitState,
) -> Result<()> {
    let registry = new_registry();
    let ipc = WorkerIpc::new(WORKER_ID, unit_types)
        .with_socket_path(sysa::paths::instance().ipc_socket_path);
    sysa::signals::forward_shutdown(ipc.shutdown_handle());
    ipc.run(
        move |_event_pub| PlaceholderController::new(registry.clone(), state_mode, default_state),
        |_, _| Ok(false),
    )
    .await
}
