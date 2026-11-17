//! `[Scope]` directive parsing for the scope worker.

use sysa::proto::UnitConfig;

/// The directives of a `[Scope]` section.
#[derive(Debug, Clone, Default)]
pub struct ScopeConfig {
    pub pids: Vec<u32>,
    pub timeout_stop_secs: u32,
    pub runtime_max_secs: u32,
    pub kill_signal: String,
    pub send_sighup: bool,
    /// Parent slice, carried from `[Unit] Slice=`.
    pub slice: String,
}

impl ScopeConfig {
    pub fn from_unit_config(config: &UnitConfig) -> Option<Self> {
        if config.config_json.is_empty() {
            return None;
        }
        let map = sysa::ir::config::parse_json(&config.config_json).ok()?;
        Some(Self::from_map(&map))
    }

    fn from_map(map: &sysa::ir::ConfigMap) -> Self {
        use sysa::ir::config::{get_bool, get_string, get_strings, get_u32_or};

        ScopeConfig {
            pids: get_strings(map, "PIDs")
                .iter()
                .filter_map(|pid| pid.trim().parse().ok())
                .collect(),
            timeout_stop_secs: get_u32_or(map, "TimeoutStopSec", 0),
            runtime_max_secs: get_u32_or(map, "RuntimeMaxSec", 0),
            kill_signal: get_string(map, "KillSignal").unwrap_or_default(),
            send_sighup: get_bool(map, "SendSIGHUP"),
            slice: get_string(map, "Slice").unwrap_or_default(),
        }
    }
}
