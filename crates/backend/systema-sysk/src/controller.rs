use crate::activation::ActivationRegistry;
use crate::socket::{self, SocketManager};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::Arc;
use sysa::controller::{decode_unit_config, UnitController, UnitStatus};
use sysa::worker_ipc::EventPublisher;

pub struct SocketController {
    manager: SocketManager,
    activation: Arc<ActivationRegistry>,
    events: EventPublisher,
}

impl SocketController {
    pub fn new(
        manager: SocketManager,
        activation: Arc<ActivationRegistry>,
        events: EventPublisher,
    ) -> Self {
        SocketController {
            manager,
            activation,
            events,
        }
    }

    /// Resolve the service a socket unit activates (explicit `Service=`
    /// wins, else the "foo.socket" -> "foo.service" convention).
    fn service_name(sc: &sysa::proto::SocketConfig, unit_name: &str) -> String {
        if sc.service.is_empty() {
            unit_name.replace(".socket", ".service")
        } else {
            sc.service.clone()
        }
    }
}

#[async_trait::async_trait]
impl UnitController for SocketController {
    async fn status(&self, unit_name: &str) -> Result<UnitStatus> {
        let guard = self.manager.lock();
        let is_active = guard.contains_key(unit_name);
        drop(guard);
        Ok(UnitStatus {
            unit_name: unit_name.to_string(),
            active_state: if is_active {
                "active".to_string()
            } else {
                "inactive".to_string()
            },
            sub_state: if is_active {
                "listening".to_string()
            } else {
                "dead".to_string()
            },
            main_pid: 0,
            invocation_id: String::new(),
            extensions: HashMap::new(),
        })
    }

    async fn sync_state(&self) -> Vec<UnitStatus> {
        let guard = self.manager.lock();
        guard
            .keys()
            .map(|name| UnitStatus {
                unit_name: name.clone(),
                active_state: "active".to_string(),
                sub_state: "listening".to_string(),
                main_pid: 0,
                invocation_id: String::new(),
                extensions: HashMap::new(),
            })
            .collect()
    }

    async fn start(&self, unit_name: &str, config: &[u8], _invocation_id: &str) -> Result<()> {
        let cfg = decode_unit_config(config)?;
        let sc = cfg
            .socket
            .as_ref()
            .context(sysa::l10n::t_("no SocketConfig in UnitConfig"))?;
        socket::start_socket(&self.manager, unit_name, sc)?;
        let service = Self::service_name(sc, unit_name);
        if sc.accept {
            socket::spawn_accept_loops(&self.manager, unit_name);
        } else {
            // Data-arrival activation: watch the bound fds for readability.
            // The monitor never reads, so the fd handed to the service
            // keeps every byte; it just decides when to start the service.
            let fds = socket::get_listener_fds(&self.manager, unit_name);
            if !fds.is_empty() {
                self.activation.start_unit(unit_name, &service, &fds);
            }
            // Track the associated service so state transitions can
            // suppress/re-arm the monitors (systemd running ↔ listening).
            self.events.subscribe_units(std::slice::from_ref(&service));
        }
        Ok(())
    }

    async fn stop(&self, unit_name: &str) -> Result<()> {
        self.activation.stop_unit(unit_name);
        socket::stop_socket(&self.manager, unit_name)?;
        Ok(())
    }

    async fn restart(&self, unit_name: &str, config: &[u8], invocation_id: &str) -> Result<()> {
        self.activation.stop_unit(unit_name);
        let _ = socket::stop_socket(&self.manager, unit_name);
        self.start(unit_name, config, invocation_id).await
    }

    async fn reload(&self, _unit_name: &str, _config: &[u8]) -> Result<()> {
        Ok(())
    }
}
