//! `[Path]` directive parsing for the path worker.

use sysa::proto::UnitConfig;

/// The directives of a `[Path]` section.
#[derive(Debug, Clone, Default)]
pub struct PathConfig {
    pub path_exists: Vec<String>,
    pub path_exists_glob: Vec<String>,
    pub path_changed: Vec<String>,
    pub path_modified: Vec<String>,
    pub directory_not_empty: Vec<String>,
    pub unit: String,
    pub make_directory: bool,
    pub directory_mode: String,
    pub trigger_limit_interval_sec: u32,
    pub trigger_limit_burst: u32,
}

impl PathConfig {
    pub fn from_unit_config(config: &UnitConfig) -> Option<Self> {
        if config.config_json.is_empty() {
            return None;
        }
        let map = sysa::ir::config::parse_json(&config.config_json).ok()?;
        Some(Self::from_map(&map))
    }

    fn from_map(map: &sysa::ir::ConfigMap) -> Self {
        use sysa::ir::config::{get_bool, get_string, get_strings, get_u32_or};

        PathConfig {
            path_exists: get_strings(map, "PathExists"),
            path_exists_glob: get_strings(map, "PathExistsGlob"),
            path_changed: get_strings(map, "PathChanged"),
            path_modified: get_strings(map, "PathModified"),
            directory_not_empty: get_strings(map, "DirectoryNotEmpty"),
            unit: get_string(map, "Unit").unwrap_or_default(),
            make_directory: get_bool(map, "MakeDirectory"),
            directory_mode: get_string(map, "DirectoryMode").unwrap_or_default(),
            trigger_limit_interval_sec: get_u32_or(map, "TriggerLimitIntervalSec", 0),
            trigger_limit_burst: get_u32_or(map, "TriggerLimitBurst", 0),
        }
    }
}
