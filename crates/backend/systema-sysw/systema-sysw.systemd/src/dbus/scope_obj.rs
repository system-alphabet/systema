//! Per-unit `Scope` interface (bridge).

use std::sync::Arc;

use zbus::interface;

use super::BridgeContext;

pub struct ScopeObject {
    pub ctx: Arc<BridgeContext>,
    pub unit_name: String,
}

#[interface(name = "org.freedesktop.systemd1.Scope")]
impl ScopeObject {
    #[zbus(property)]
    fn controller(&self) -> String {
        self.snapshot().controller
    }

    #[zbus(property)]
    fn timeout_stop_usec(&self) -> u64 {
        self.snapshot().scope_timeout_stop_sec as u64 * 1_000_000
    }

    #[zbus(property)]
    fn runtime_max_usec(&self) -> u64 {
        self.snapshot().scope_runtime_max_sec as u64 * 1_000_000
    }

    #[zbus(property)]
    fn result(&self) -> String {
        match self.snapshot().extensions.get("last_exit_code") {
            Some(code) if code != "0" => "exit-code".to_string(),
            _ => "success".to_string(),
        }
    }

    /// Abandon the scope via the control plane.
    async fn abandon(&self) -> zbus::fdo::Result<()> {
        let req = sysa::proto::AbandonScopeRequest {
            name: self.unit_name.clone(),
        };
        let reply: sysa::proto::SimpleManagerResult = self
            .ctx
            .client
            .call("manager.abandon_scope", &req)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        if reply.success {
            Ok(())
        } else {
            Err(zbus::fdo::Error::Failed(reply.message))
        }
    }
}

impl ScopeObject {
    fn snapshot(&self) -> sysa::proto::UnitSnapshot {
        self.ctx
            .mirror
            .read()
            .get(&self.unit_name)
            .cloned()
            .unwrap_or_default()
    }
}