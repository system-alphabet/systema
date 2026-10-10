use std::collections::HashMap;

use anyhow::{bail, Result};
use sysa::controller::{decode_unit_config, UnitController, UnitStatus};
use sysa::worker_ipc::EventPublisher;
use tracing::{info, warn};

use crate::cgroup::{self, scope_cgroup_path};
use crate::state::{ScopeRegistry, ScopeState};

pub struct ScopeController {
    registry: ScopeRegistry,
    event_pub: EventPublisher,
}

impl ScopeController {
    pub fn new(registry: ScopeRegistry, event_pub: EventPublisher) -> Self {
        ScopeController {
            registry,
            event_pub,
        }
    }

    fn status_of(&self, unit_name: &str) -> UnitStatus {
        let reg = self.registry.lock();
        match reg.get(unit_name) {
            Some(inst) => {
                let mut extensions = HashMap::new();
                if !inst.pids.is_empty() {
                    extensions.insert(
                        "pids".to_string(),
                        inst.pids
                            .iter()
                            .map(u32::to_string)
                            .collect::<Vec<_>>()
                            .join(","),
                    );
                }
                UnitStatus {
                    unit_name: unit_name.to_string(),
                    active_state: match inst.state {
                        ScopeState::Dead | ScopeState::Failed => "inactive",
                        ScopeState::Starting => "activating",
                        ScopeState::Running | ScopeState::Abandoned => "active",
                        ScopeState::Stopping => "deactivating",
                    }
                    .to_string(),
                    sub_state: inst.state.as_str().to_string(),
                    main_pid: inst.pids.first().copied().unwrap_or(0),
                    invocation_id: inst.invocation_id.clone().unwrap_or_default(),
                    extensions,
                }
            }
            None => UnitStatus {
                unit_name: unit_name.to_string(),
                active_state: "inactive".to_string(),
                sub_state: "dead".to_string(),
                main_pid: 0,
                invocation_id: String::new(),
                extensions: HashMap::new(),
            },
        }
    }

    fn publish_state(&self, unit_name: &str) {
        let status = self.status_of(unit_name);
        self.event_pub
            .publish_unit_state_update(vec![status], false);
    }
}

/// Stop the scope by signalling its cgroup processes, waiting up to
/// `timeout` seconds for the cgroup to empty, then escalating to SIGKILL.
///
/// Mirrors systemd's `scope_enter_signal` / `scope_dispatch_timer`:
/// SIGTERM first, SIGKILL after `TimeoutStopSec=` has elapsed.
pub(crate) async fn stop_and_wait(
    registry: &ScopeRegistry,
    event_pub: &EventPublisher,
    unit_name: &str,
    cgroup_path: &str,
    timeout_secs: u32,
) {
    {
        let mut reg = registry.lock();
        if let Some(inst) = reg.get_mut(unit_name) {
            inst.state = ScopeState::Stopping;
        }
    }
    event_pub.publish_unit_state_update(
        vec![{
            let reg = registry.lock();
            let inst = reg.get(unit_name).unwrap();
            UnitStatus {
                unit_name: unit_name.to_string(),
                active_state: "deactivating".to_string(),
                sub_state: "stop-sigterm".to_string(),
                main_pid: inst.pids.first().copied().unwrap_or(0),
                invocation_id: inst.invocation_id.clone().unwrap_or_default(),
                extensions: HashMap::new(),
            }
        }],
        false,
    );

    let _ = cgroup::signal_cgroup(cgroup_path, cgroup::parse_signal(""));

    // Wait for the cgroup to empty, then escalate to SIGKILL after
    // TimeoutStopSec= (mirrors scope_enter_signal → scope_dispatch_timer).
    let start = std::time::Instant::now();
    let timeout_dur = std::time::Duration::from_secs(u64::from(timeout_secs));
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if cgroup::is_cgroup_empty(cgroup_path) {
            dead_and_publish(registry, event_pub, unit_name);
            return;
        }
        if start.elapsed() >= timeout_dur {
            warn!(
                "Scope {} did not empty in {timeout_secs}s, escalating to SIGKILL",
                unit_name
            );
            let _ = cgroup::kill_cgroup(cgroup_path);
            break;
        }
    }

    // After SIGKILL the cgroup should empty promptly.
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if cgroup::is_cgroup_empty(cgroup_path) {
            dead_and_publish(registry, event_pub, unit_name);
            return;
        }
        if start.elapsed() >= timeout_dur + std::time::Duration::from_secs(5) {
            warn!(
                "Scope {} still populated after SIGKILL; marking dead anyway",
                unit_name
            );
            dead_and_publish(registry, event_pub, unit_name);
            return;
        }
    }
}

fn dead_and_publish(registry: &ScopeRegistry, event_pub: &EventPublisher, unit_name: &str) {
    {
        let mut reg = registry.lock();
        if let Some(inst) = reg.get_mut(unit_name) {
            inst.state = ScopeState::Dead;
            inst.invocation_id = None;
        }
    }
    let status = UnitStatus {
        unit_name: unit_name.to_string(),
        active_state: "inactive".to_string(),
        sub_state: "dead".to_string(),
        main_pid: 0,
        invocation_id: String::new(),
        extensions: HashMap::new(),
    };
    event_pub.publish_unit_state_update(vec![status], false);
}

#[async_trait::async_trait]
impl UnitController for ScopeController {
    async fn status(&self, unit_name: &str) -> Result<UnitStatus> {
        Ok(self.status_of(unit_name))
    }

    async fn sync_state(&self) -> Vec<UnitStatus> {
        let names: Vec<String> = {
            let guard = self.registry.lock();
            guard.keys().cloned().collect()
        };
        names.iter().map(|n| self.status_of(n)).collect()
    }

    async fn start(&self, unit_name: &str, config: &[u8], invocation_id: &str) -> Result<()> {
        let cfg = decode_unit_config(config)?;
        let scope_cfg = cfg.scope.as_ref().ok_or_else(|| {
            anyhow::anyhow!(sysa::l10n::fmt(
                sysa::l10n::t_("No scope configuration for {unit_name}."),
                &[("unit_name", unit_name)]
            ))
        })?;

        // systemd refuses to start a scope without PIDs (`scope.c`: "Scope
        // has no PIDs. Refusing.").
        if scope_cfg.pids.is_empty() {
            bail!(sysa::l10n::fmt(
                sysa::l10n::t_("Scope {unit_name} has no PIDs. Refusing."),
                &[("unit_name", unit_name)]
            ));
        }

        let cgroup_path = scope_cgroup_path(&scope_cfg.slice, unit_name);
        info!(
            "Starting scope {} with pids {:?} at {}",
            unit_name, scope_cfg.pids, cgroup_path
        );

        {
            let mut reg = self.registry.lock();
            let inst = reg.entry(unit_name.to_string()).or_default();
            inst.state = ScopeState::Starting;
            inst.pids = scope_cfg.pids.clone();
            inst.timeout_stop_secs = scope_cfg.timeout_stop_secs;
            inst.runtime_max_secs = scope_cfg.runtime_max_secs;
            inst.kill_signal = scope_cfg.kill_signal.clone();
            inst.send_sighup = scope_cfg.send_sighup;
            inst.cgroup_path = Some(cgroup_path.clone());
            inst.invocation_id = if invocation_id.is_empty() {
                None
            } else {
                Some(invocation_id.to_string())
            };
        }
        self.publish_state(unit_name);

        {
            let mut reg = self.registry.lock();
            if let Some(inst) = reg.get_mut(unit_name) {
                inst.state = ScopeState::Running;
            }
        }
        self.publish_state(unit_name);

        tokio::spawn(crate::ipc::monitor_scope(
            self.registry.clone(),
            self.event_pub.clone(),
            unit_name.to_string(),
            cgroup_path,
        ));
        Ok(())
    }

    async fn stop(&self, unit_name: &str) -> Result<()> {
        let (timeout, cgroup_path) = {
            let reg = self.registry.lock();
            match reg.get(unit_name) {
                Some(inst) if matches!(inst.state, ScopeState::Running | ScopeState::Abandoned) => {
                    (inst.timeout_stop_secs, inst.cgroup_path.clone())
                }
                _ => return Ok(()),
            }
        };
        let cgroup_path = cgroup_path.ok_or_else(|| {
            anyhow::anyhow!(sysa::l10n::fmt(
                sysa::l10n::t_("Scope {unit_name} has no cgroup."),
                &[("unit_name", unit_name)]
            ))
        })?;

        stop_and_wait(&self.registry, &self.event_pub, unit_name, &cgroup_path, timeout).await;
        Ok(())
    }

    async fn restart(&self, unit_name: &str, config: &[u8], invocation_id: &str) -> Result<()> {
        // systemd's scopes are transient wrappers: "restart" is a stop
        // followed by a start with the (unchanged) transient PIDs.
        self.stop(unit_name).await?;
        self.start(unit_name, config, invocation_id).await
    }

    async fn reload(&self, _unit_name: &str, _config: &[u8]) -> Result<()> {
        bail!(sysa::l10n::t_("Scopes cannot be reloaded."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::new_registry;
    use prost::Message;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::{mpsc, oneshot};
    use sysa::proto::{ScopeConfig, UnitConfig};
    use sysa::worker_ipc::EventPublisher;

    fn test_controller() -> (ScopeController, ScopeRegistry) {
        let registry = new_registry();
        let (tx, _rx) = mpsc::unbounded_channel::<bytes::Bytes>();
        let pending: Arc<std::sync::Mutex<HashMap<u64, oneshot::Sender<sysa::proto::UnitStateUpdateAck>>>> =
            Arc::new(std::sync::Mutex::new(HashMap::new()));
        let publisher = EventPublisher::new(tx, "system-e-1", pending);
        (ScopeController::new(registry.clone(), publisher), registry)
    }

    fn config_with_pids(pids: Vec<u32>) -> Vec<u8> {
        let cfg = UnitConfig {
            unit_name: "test.scope".to_string(),
            description: String::new(),
            service: None,
            socket: None,
            mount: None,
            automount: None,
            timer: None,
            device: None,
            path: None,
            socket_units: Vec::new(),
            scope: Some(ScopeConfig {
                pids,
                timeout_stop_secs: 90,
                runtime_max_secs: 0,
                kill_signal: String::new(),
                send_sighup: false,
                controller: String::new(),
                slice: "system.slice".to_string(),
            }),
        };
        let mut buf = bytes::BytesMut::new();
        cfg.encode(&mut buf).expect("encode");
        buf.to_vec()
    }

    #[tokio::test]
    async fn start_refuses_scope_without_pids() {
        let (controller, _registry) = test_controller();
        let err = controller
            .start("test.scope", &config_with_pids(vec![]), "")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no PIDs"), "{}", err);
    }

    #[tokio::test]
    async fn start_records_pids_and_runs() {
        let (controller, registry) = test_controller();
        controller
            .start("test.scope", &config_with_pids(vec![1234, 5678]), "inv-1")
            .await
            .expect("start succeeds");
        let reg = registry.lock();
        let inst = reg.get("test.scope").expect("instance recorded");
        assert_eq!(inst.state, ScopeState::Running);
        assert_eq!(inst.pids, vec![1234, 5678]);
        assert_eq!(
            inst.cgroup_path.as_deref(),
            Some("/sys/fs/cgroup/system.slice/test.scope")
        );
    }

    #[tokio::test]
    async fn reload_is_rejected() {
        let (controller, _registry) = test_controller();
        let err = controller
            .reload("test.scope", &[])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cannot be reloaded"));
    }
}
