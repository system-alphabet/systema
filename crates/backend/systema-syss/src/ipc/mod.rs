use anyhow::Result;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{info, warn};

use sysa::worker_ipc::{EventPublisher, WorkerIpc};

use crate::controller::ServiceController;
use crate::state::{new_registry, ServiceRegistry, ServiceState};

const WORKER_ID: &str = "system-s-1";
const WORKER_UNIT_TYPES: &[&str] = &["service"];

/// Shared handle to the service registry, accessible from `main` after
/// `run()` returns so that the shutdown sequence can reap child processes.
pub(crate) type SharedRegistry = Arc<Mutex<Option<ServiceRegistry>>>;

pub fn shared_registry() -> SharedRegistry {
    Arc::new(Mutex::new(None))
}

pub async fn run(shared: &SharedRegistry) -> Result<()> {
    let registry = new_registry();
    *shared.lock().await = Some(registry.clone());
    let fdpass: Arc<Mutex<Option<tokio::net::UnixStream>>> = Arc::new(Mutex::new(None));

    // Connect to the allocator's fdpass socket (SCM_RIGHTS channel used to
    // receive listener fds for socket activation). Non-fatal if unavailable:
    // services are then started without LISTEN_FDS.
    {
        use tokio::io::AsyncWriteExt;
        match tokio::net::UnixStream::connect(sysa::paths::instance().systema_fdpass_sock).await {
            Ok(mut s) => {
                let ident = format!("{WORKER_ID}\n");
                if let Err(e) = s.write_all(ident.as_bytes()).await {
                    warn!("Failed to send id on fdpass channel: {}", e);
                }
                *fdpass.lock().await = Some(s);
            }
            Err(e) => {
                warn!(
                    "Cannot connect to fdpass socket ({}): {}",
                    sysa::paths::instance().systema_fdpass_sock,
                    e,
                );
            }
        }
    }

    let fdpass_controller = fdpass.clone();
    let notify = crate::notify::NotifyManager::setup();
    let dbus = crate::dbus::DbusWaiter::default();
    // On SIGTERM/SIGINT the IPC loop stops our services *first* — while the
    // connection is still open, so their final states still reach System A —
    // and only then exchanges the `worker.exit` goodbye.
    let shared_for_cleanup = shared.clone();
    WorkerIpc::new(WORKER_ID, WORKER_UNIT_TYPES)
        .on_shutdown(move || {
            let shared = shared_for_cleanup.clone();
            async move { crate::shutdown(shared).await }
        })
        .run(
            move |event_pub| {
                ServiceController::new(
                    registry.clone(),
                    event_pub,
                    fdpass_controller.clone(),
                    notify.clone(),
                    dbus.clone(),
                )
            },
            |_, _| Ok(false),
        )
        .await
}

pub(crate) async fn monitor_service(
    registry: ServiceRegistry,
    unit_name: String,
    event_pub: EventPublisher,
    mut child: tokio::process::Child,
) {
    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        let state = {
            let reg = registry.lock();
            match reg.get(&unit_name) {
                None => break,
                Some(inst) => inst.state,
            }
        };

        if matches!(
            state,
            ServiceState::Dead | ServiceState::Failed | ServiceState::Stopping
        ) {
            break;
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                let pid = child.id().unwrap_or(0);
                info!(
                    "Service {} (PID {}) exited: code={:?}, success={}",
                    unit_name,
                    pid,
                    status.code(),
                    status.success()
                );
                let stay_active = {
                    let reg = registry.lock();
                    reg.get(&unit_name)
                        .map(|inst| status.success() && inst.remain_after_exit)
                        .unwrap_or(false)
                };
                if stay_active {
                    // RemainAfterExit=yes: keep the unit "active" after a
                    // successful exit (oneshot style), like systemd.  The
                    // invocation stays assigned (systemd keeps the ID of the
                    // still-active activation).
                    info!(
                        "Service {}: RemainAfterExit=yes, keeping unit active after successful exit",
                        unit_name
                    );
                    {
                        let mut reg = registry.lock();
                        if let Some(inst) = reg.get_mut(&unit_name) {
                            inst.state = ServiceState::Running;
                            inst.main_pid = None;
                            inst.last_exit_code = status.code();
                        }
                    }
                    publish_service_state(&registry, &event_pub, &unit_name);
                    break;
                }
                let state = if status.success() {
                    ServiceState::Dead
                } else {
                    ServiceState::Failed
                };
                {
                    let mut reg = registry.lock();
                    if let Some(inst) = reg.get_mut(&unit_name) {
                        inst.state = state;
                        inst.main_pid = None;
                        inst.last_exit_code = status.code();
                        inst.invocation_id = None;
                    }
                }
                publish_service_state(&registry, &event_pub, &unit_name);
                break;
            }
            Ok(None) => {}
            Err(e) => {
                warn!("Error waiting for child process of {}: {}", unit_name, e);
                {
                    let mut reg = registry.lock();
                    if let Some(inst) = reg.get_mut(&unit_name) {
                        inst.state = ServiceState::Failed;
                        inst.main_pid = None;
                        inst.invocation_id = None;
                    }
                }
                publish_service_state(&registry, &event_pub, &unit_name);
                break;
            }
        }
    }
}

/// Publish the current runtime state of a service as a unified
/// `unit.state_update` (with `last_exit_code` in the extensions).
fn publish_service_state(registry: &ServiceRegistry, event_pub: &EventPublisher, unit_name: &str) {
    use std::collections::HashMap;
    use sysa::controller::UnitStatus;

    let mut extensions = HashMap::new();
    let reg = registry.lock();
    let inst = match reg.get(unit_name) {
        Some(inst) => inst,
        None => return,
    };
    if let Some(code) = inst.last_exit_code {
        extensions.insert("last_exit_code".to_string(), code.to_string());
    }
    let status = UnitStatus {
        unit_name: unit_name.to_string(),
        active_state: match inst.state {
            ServiceState::Dead => "inactive",
            ServiceState::Failed => "failed",
            ServiceState::Running => "active",
            ServiceState::Starting => "activating",
            ServiceState::Stopping => "deactivating",
        }
        .to_string(),
        sub_state: inst.state.as_str().to_string(),
        main_pid: inst.main_pid.unwrap_or(0),
        invocation_id: inst.invocation_id.clone().unwrap_or_default(),
        extensions,
    };
    drop(reg);
    event_pub.publish_unit_state_update(vec![status], false);
}
