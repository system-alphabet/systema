use anyhow::Result;

use sysa::worker_ipc::WorkerIpc;

use crate::linux::controller::MountController;
use crate::linux::mountinfo::MountInfoMonitor;
use crate::linux::state::{new_automount_registry, new_mount_registry};

const WORKER_ID: &str = "system-m-1";
const WORKER_UNIT_TYPES: &[&str] = &["mount", "automount"];

pub async fn run() -> Result<()> {
    let mount_registry = new_mount_registry();
    let automount_registry = new_automount_registry();

    WorkerIpc::new(WORKER_ID, WORKER_UNIT_TYPES)
        .run(
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
