//! Per-unit `Socket` interface (bridge).

use std::sync::Arc;

use zbus::interface;

use super::BridgeContext;

pub struct SocketObject {
    pub ctx: Arc<BridgeContext>,
    pub unit_name: String,
}

#[interface(name = "org.freedesktop.systemd1.Socket")]
impl SocketObject {
    #[zbus(property)]
    fn result(&self) -> String {
        match self.snapshot().extensions.get("last_exit_code") {
            Some(code) if code != "0" => "exit-code".to_string(),
            _ => "success".to_string(),
        }
    }

    #[zbus(property)]
    fn n_accepted(&self) -> u32 {
        0
    }

    #[zbus(property)]
    fn n_connections(&self) -> u32 {
        0
    }

    #[zbus(property)]
    fn control_pid(&self) -> u32 {
        self.snapshot().main_pid
    }

    #[zbus(property)]
    fn x_attr_entry_point(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    #[zbus(property)]
    fn x_attr_listen(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    #[zbus(property)]
    fn x_attr_accept(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    #[zbus(property)]
    fn c_p_u_set_partition(&self) -> String {
        "member".to_string()
    }

    #[zbus(property)]
    fn o_o_m_rules(&self) -> Vec<String> {
        Vec::new()
    }
}

impl SocketObject {
    fn snapshot(&self) -> sysa::proto::UnitSnapshot {
        self.ctx
            .mirror
            .read()
            .get(&self.unit_name)
            .cloned()
            .unwrap_or_default()
    }
}