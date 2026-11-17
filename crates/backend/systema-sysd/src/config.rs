//! `[Device]` match-rule parsing for the device worker.

use sysa::proto::UnitConfig;

/// The match rules of a `[Device]` section.
#[derive(Debug, Clone, Default)]
pub struct DeviceConfig {
    pub device_name: String,
    pub device_path: String,
    pub sysfs_path: String,
    pub property: Vec<String>,
}

impl DeviceConfig {
    pub fn from_unit_config(config: &UnitConfig) -> Option<Self> {
        if config.config_json.is_empty() {
            return None;
        }
        let map = sysa::ir::config::parse_json(&config.config_json).ok()?;
        Some(Self::from_map(&map))
    }

    fn from_map(map: &sysa::ir::ConfigMap) -> Self {
        use sysa::ir::config::{get_string, get_strings};

        DeviceConfig {
            device_name: get_string(map, "DeviceName").unwrap_or_default(),
            device_path: get_string(map, "DevicePath").unwrap_or_default(),
            sysfs_path: get_string(map, "SysfsPath").unwrap_or_default(),
            property: get_strings(map, "Property"),
        }
    }
}
