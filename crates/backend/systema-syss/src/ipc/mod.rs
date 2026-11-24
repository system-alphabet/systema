use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{info, warn};

use sysa::worker_ipc::{EventPublisher, WorkerIpc};

use crate::cgroup::Querier;
use crate::controller::ServiceController;
use crate::process::{run_exec_stop, run_exec_stop_post, ManagerScope};
use crate::state::{new_registry, ServiceRegistry, ServiceState};

const WORKER_ID: &str = "system-s-1";
const WORKER_UNIT_TYPES: &[&str] = &["service"];

/// Shared handle to the service registry, accessible from `main` after
/// `run()` returns so that the shutdown sequence can reap child processes.
pub(crate) type SharedRegistry = Arc<Mutex<Option<ServiceRegistry>>>;

pub fn shared_registry() -> SharedRegistry {
    Arc::new(Mutex::new(None))
}

pub async fn run(
    shared: &SharedRegistry,
    bus_address: Option<zbus::Address>,
    manager_scope: ManagerScope,
) -> Result<()> {
    let registry = new_registry();
    *shared.lock().await = Some(registry.clone());
    let fdpass: Arc<Mutex<Option<tokio::net::UnixStream>>> = Arc::new(Mutex::new(None));
    // Serializes the request/response rounds on `fdpass` (no correlation IDs
    // on that stream — see `ServiceController::fd_request_lock`).
    let fd_request_lock: Arc<Mutex<()>> = Arc::new(Mutex::new(()));

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
    let dbus = crate::dbus::DbusWaiter::new(bus_address);
    // Shared "no cgroup kill on this machine" verdict: the controller's
    // querier learns it from an `available = false` answer and stops asking,
    // and the reconnect hook clears it so a fresh connection asks again
    // (System R may have come up in the meantime, or the cgroup backend
    // recovered).
    let no_cgroup_backend = Arc::new(AtomicBool::new(false));
    // A shutdown request (SIGTERM/SIGINT forwarded by `sysa::signals`) makes
    // the IPC loop stop our services *first* — while the connection is still
    // open, so their final states still reach System A — and only then
    // exchanges the `worker.exit` goodbye.
    let shared_for_cleanup = shared.clone();
    let ipc = WorkerIpc::new(WORKER_ID, WORKER_UNIT_TYPES)
        .with_socket_path(sysa::paths::instance().ipc_socket_path)
        .on_reconnect({
            let no_cgroup_backend = no_cgroup_backend.clone();
            move || {
                let no_cgroup_backend = no_cgroup_backend.clone();
                async move { no_cgroup_backend.store(false, Ordering::Relaxed) }
            }
        })
        .on_shutdown(move || {
            let shared = shared_for_cleanup.clone();
            async move { crate::shutdown(shared).await }
        });
    sysa::signals::forward_shutdown(ipc.shutdown_handle());
    ipc.run(
        {
            let no_cgroup_backend = no_cgroup_backend.clone();
            let fd_request_lock = fd_request_lock.clone();
            move |event_pub| {
                let cgroup = Querier::new(
                    event_pub.clone(),
                    // The querier rides the same fresh-per-connection
                    // publisher the controller uses for state updates;
                    // only the no-backend verdict outlives a reconnect.
                    no_cgroup_backend.clone(),
                );
                ServiceController::new(
                    registry.clone(),
                    event_pub,
                    cgroup,
                    fdpass_controller.clone(),
                    fd_request_lock.clone(),
                    notify.clone(),
                    dbus.clone(),
                    manager_scope,
                )
            }
        },
        |_, _| Ok(false),
    )
    .await
}

pub(crate) async fn monitor_service(
    registry: ServiceRegistry,
    unit_name: String,
    event_pub: EventPublisher,
    mut wait: crate::process::ServiceWait,
    pid: u32,
) {
    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        let (state, current_pid) = {
            let reg = registry.lock();
            match reg.get(&unit_name) {
                None => break,
                Some(inst) => (inst.state, inst.main_pid),
            }
        };

        if matches!(
            state,
            ServiceState::Dead | ServiceState::Failed | ServiceState::Stopping
        ) {
            break;
        }

        // A restart may have replaced this activation while we slept.  This
        // monitor still holds the old handle; waking up and calling
        // `try_wait()` on it would return ECHILD (the reaper already collected
        // it) and, worse, the failure branch below would mark the *new*
        // activation failed.  Stop observing once the registry no longer
        // points at our process.
        if current_pid != Some(pid) {
            break;
        }

        match wait.try_wait() {
            Ok(Some(status)) => {
                info!(
                    "Service {} (PID {}) exited: code={:?}, success={}",
                    unit_name,
                    pid,
                    status.code(),
                    status.success()
                );
                // `ExecStart=-`: a non-zero exit is recorded in
                // `last_exit_code` but counts as success for the unit.
                let (stay_active, succeeded, stopping, timeout_stop) = {
                    let reg = registry.lock();
                    reg.get(&unit_name)
                        .map(|inst| {
                            let succeeded = inst.exit_succeeded(status.success());
                            (
                                succeeded && inst.remain_after_exit,
                                succeeded,
                                inst.state == ServiceState::Stopping,
                                inst.timeout_stop_secs.unwrap_or(30),
                            )
                        })
                        .unwrap_or((false, status.success(), false, 30))
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
                if stopping {
                    // An explicit stop is already running; it owns ExecStop=,
                    // the kill phase and ExecStopPost=.
                    break;
                }
                let mut control_failed = false;
                if succeeded {
                    // systemd runs ExecStop= when a service exits cleanly on
                    // its own (service_enter_exited_or_stop ->
                    // service_enter_stop); a failed exit jumps straight to the
                    // signal/post path without ExecStop=.
                    if let Err(e) = run_exec_stop(registry.clone(), &unit_name, timeout_stop).await
                    {
                        warn!("ExecStop for {} failed: {}", unit_name, e);
                        control_failed = true;
                    }
                }
                if let Err(e) = run_exec_stop_post(registry.clone(), &unit_name, timeout_stop).await
                {
                    warn!("ExecStopPost for {} failed: {}", unit_name, e);
                    control_failed = true;
                }
                let state = if !succeeded || control_failed {
                    ServiceState::Failed
                } else {
                    ServiceState::Dead
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A monitor whose activation was replaced by a restart must stop quietly:
    /// it holds the old `Child`, and calling `try_wait()` on it after the
    /// reaper collected it returns ECHILD, which used to mark the *new*
    /// activation failed and trigger an endless restart loop.
    #[tokio::test]
    async fn stale_monitor_leaves_the_new_activation_alone() {
        let registry = new_registry();
        let child = tokio::process::Command::new("sleep")
            .arg("5")
            .kill_on_drop(true)
            .spawn()
            .expect("spawn sleep");
        let child_pid = child.id().expect("child pid");

        {
            let mut reg = registry.lock();
            let inst = reg.entry("restarted.service".to_string()).or_default();
            inst.state = ServiceState::Running;
            // The registry already points at a newer activation.
            inst.main_pid = Some(child_pid + 1);
        }

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let publisher = EventPublisher::new(tx, "test-worker", Arc::default(), Arc::default());

        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            monitor_service(
                registry.clone(),
                "restarted.service".to_string(),
                publisher,
                crate::process::ServiceWait::Child(child),
                child_pid,
            ),
        )
        .await
        .expect("the stale monitor must return instead of failing the new activation");

        let reg = registry.lock();
        let inst = reg.get("restarted.service").unwrap();
        assert_eq!(inst.state, ServiceState::Running);
        assert_eq!(inst.main_pid, Some(child_pid + 1));
    }
}
