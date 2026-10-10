//! Per-unit `Service` interface (bridge).

use std::sync::Arc;

use zbus::interface;

use super::BridgeContext;

pub struct ServiceObject {
    pub ctx: Arc<BridgeContext>,
    pub unit_name: String,
}

#[interface(name = "org.freedesktop.systemd1.Service")]
impl ServiceObject {
    #[zbus(property)]
    fn r#type(&self) -> String {
        let t = self.snapshot().service_type;
        if t.is_empty() { "simple".to_string() } else { t }
    }

    #[zbus(property)]
    fn main_pid(&self) -> u32 {
        self.snapshot().main_pid
    }

    #[zbus(property)]
    fn control_pid(&self) -> u32 {
        self.snapshot().main_pid
    }

    #[zbus(property)]
    fn bus_name(&self) -> String {
        self.snapshot().bus_name
    }

    #[zbus(property)]
    fn exec_main_pid(&self) -> u32 {
        self.snapshot().main_pid
    }

    #[zbus(property)]
    fn exec_main_status(&self) -> i32 {
        self.last_exit_code().unwrap_or(0)
    }

    #[zbus(property)]
    fn result(&self) -> String {
        match self.last_exit_code() {
            Some(code) if code != 0 => "exit-code".to_string(),
            _ => "success".to_string(),
        }
    }

    #[zbus(property)]
    fn restart(&self) -> String {
        let r = self.snapshot().restart;
        if r.is_empty() { "no".to_string() } else { r }
    }

    #[zbus(property)]
    fn restart_u_sec(&self) -> u64 {
        self.snapshot().restart_sec as u64 * 1_000_000
    }

    #[zbus(property)]
    fn notify_access(&self) -> String {
        let n = self.snapshot().notify_access;
        if n.is_empty() { "none".to_string() } else { n }
    }

    #[zbus(property)]
    fn restart_randomized_delay_u_sec(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn l_u_o_session(&self) -> Vec<String> {
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

impl ServiceObject {
    fn snapshot(&self) -> sysa::proto::UnitSnapshot {
        self.ctx
            .mirror
            .read()
            .get(&self.unit_name)
            .cloned()
            .unwrap_or_default()
    }

    fn last_exit_code(&self) -> Option<i32> {
        self.snapshot()
            .extensions
            .get("last_exit_code")
            .and_then(|code| code.parse::<i32>().ok())
    }
}