use anyhow::Result;
use sysa::worker_ipc::WorkerIpc;

use crate::controller::MountController;
use crate::mounttable::MountTableMonitor;
use crate::state::{new_automount_registry, new_registry};

const WORKER_ID: &str = "system-m-1";
const WORKER_UNIT_TYPES: &[&str] = &["mount", "automount"];

pub async fn run() -> Result<()> {
    let registry = new_registry();
    let automount_registry = new_automount_registry();
    WorkerIpc::new(WORKER_ID, WORKER_UNIT_TYPES)
        .run(
            move |event_pub| {
                // Spawn the MountTableMonitor with a clone of the fresh
                // EventPublisher; it periodically reconciles the registry
                // against the real mount table and commits dynamically
                // discovered mount units to System A.
                {
                    let ep = event_pub.clone();
                    let reg = registry.clone();
                    tokio::spawn(async move {
                        let mut monitor = MountTableMonitor::new(reg, ep);
                        monitor.run().await;
                    });
                }

                MountController::new(registry.clone(), automount_registry.clone(), event_pub)
            },
            |_, _| Ok(false),
        )
        .await
}
