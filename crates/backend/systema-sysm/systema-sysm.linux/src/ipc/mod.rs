use anyhow::Result;

use sysa::worker_ipc::WorkerIpc;

use crate::linux::controller::MountController;
use crate::linux::mountinfo::MountInfoMonitor;
use crate::linux::reaper;
use crate::linux::state::{new_automount_registry, new_mount_registry};

const WORKER_ID: &str = "system-m-1";
const WORKER_UNIT_TYPES: &[&str] = &["mount", "automount"];

pub async fn run() -> Result<()> {
    let mount_registry = new_mount_registry();
    let automount_registry = new_automount_registry();

    let ipc = WorkerIpc::new(WORKER_ID, WORKER_UNIT_TYPES)
        .with_socket_path(sysa::paths::instance().ipc_socket_path)
        // The adopted mount daemons (FUSE helpers and friends) are this
        // worker's children, so they are stopped before the `worker.exit`
        // goodbye, while the connection is still open.
        .on_shutdown(|| async { reaper::stop_children().await });
    sysa::signals::forward_shutdown(ipc.shutdown_handle());
    ipc.run(
        move |event_pub| {
            // Spawn MountInfoMonitor with a clone of the fresh EventPublisher.
            {
                let ep = event_pub.clone();
                let reg = mount_registry.clone();
                tokio::spawn(async move {
                    let mut monitor = MountInfoMonitor::new(reg, ep);
                    monitor.run().await;
                });
            }

            // Kernel automount triggers are handled entirely inside
            // MountController (mount/umount + ACK/FAIL reply + state
            // publish); no IPC round-trip to SysA is involved.
            MountController::new(
                mount_registry.clone(),
                automount_registry.clone(),
                event_pub,
            )
        },
        |_, _| Ok(false),
    )
    .await
}
