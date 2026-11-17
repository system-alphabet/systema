//! `[Timer]` directive parsing for the timer worker.

use sysa::proto::UnitConfig;

/// The directives of a `[Timer]` section.
#[derive(Debug, Clone, Default)]
pub struct TimerConfig {
    pub on_active_sec: Option<u32>,
    pub on_boot_sec: Option<u32>,
    pub on_startup_sec: Option<u32>,
    pub on_unit_active_sec: Option<u32>,
    pub on_unit_inactive_sec: Option<u32>,
    pub on_calendar: Vec<String>,
    pub randomized_delay_sec: u32,
    pub unit: String,
    pub persistent: bool,
}

impl TimerConfig {
    pub fn from_unit_config(config: &UnitConfig) -> Option<Self> {
        if config.config_json.is_empty() {
            return None;
        }
        let map = sysa::ir::config::parse_json(&config.config_json).ok()?;
        Some(Self::from_map(&map))
    }

    fn from_map(map: &sysa::ir::ConfigMap) -> Self {
        use sysa::ir::config::{get_bool, get_string, get_strings, get_u32, get_u32_or};

        TimerConfig {
            on_active_sec: get_u32(map, "OnActiveSec"),
            on_boot_sec: get_u32(map, "OnBootSec"),
            on_startup_sec: get_u32(map, "OnStartupSec"),
            on_unit_active_sec: get_u32(map, "OnUnitActiveSec"),
            on_unit_inactive_sec: get_u32(map, "OnUnitInactiveSec"),
            on_calendar: get_strings(map, "OnCalendar"),
            randomized_delay_sec: get_u32_or(map, "RandomizedDelaySec", 0),
            unit: get_string(map, "Unit").unwrap_or_default(),
            persistent: get_bool(map, "Persistent"),
        }
    }
}
