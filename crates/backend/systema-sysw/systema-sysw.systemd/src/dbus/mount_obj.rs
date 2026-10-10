//! Per-unit `Mount` interface (bridge).

use std::sync::Arc;

use zbus::interface;

use super::BridgeContext;

pub struct MountObject {
    pub ctx: Arc<BridgeContext>,
    pub unit_name: String,
}

#[interface(name = "org.freedesktop.systemd1.Mount")]
impl MountObject {
    #[zbus(property)]
    fn where_(&self) -> String {
        self.snapshot().mount_where
    }

    #[zbus(property)]
    fn what(&self) -> String {
        self.snapshot().mount_what
    }

    #[zbus(property)]
    fn options(&self) -> String {
        self.snapshot().mount_options
    }

    #[zbus(property)]
    fn timeout_u_sec(&self) -> u64 {
        self.snapshot().mount_timeout_sec as u64 * 1_000_000
    }

    #[zbus(property)]
    fn control_pid(&self) -> u32 {
        self.snapshot().main_pid
    }

    #[zbus(property)]
    fn result(&self) -> String {
        match self.snapshot().extensions.get("last_exit_code") {
            Some(code) if code != "0" => "exit-code".to_string(),
            _ => "success".to_string(),
        }
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

impl MountObject {
    fn snapshot(&self) -> sysa::proto::UnitSnapshot {
        self.ctx
            .mirror
            .read()
            .get(&self.unit_name)
            .cloned()
            .unwrap_or_default()
    }
}