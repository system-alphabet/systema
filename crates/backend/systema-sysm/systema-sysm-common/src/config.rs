//! `[Mount]` / `[Automount]` directive parsing for the mount worker.

use sysa::proto::UnitConfig;

/// The directives of a `[Mount]` section.
#[derive(Debug, Clone, Default)]
pub struct MountConfig {
    pub what: String,
    pub r#where: String,
    pub r#type: String,
    pub options: String,
    pub timeout_sec: u32,
    pub lazy_unmount: bool,
    pub force_unmount: bool,
    pub directory_mode: String,
    pub sloppy_options: bool,
}

/// The directives of an `[Automount]` section.
#[derive(Debug, Clone, Default)]
pub struct AutomountConfig {
    pub r#where: String,
    pub extra_options: String,
    pub timeout_idle_sec: u32,
    pub directory_mode: String,
}

/// Parse the mount and automount sections out of one unit config.
///
/// An automount unit's map also carries its companion `.mount` directives
/// (System A merges them before dispatch), so both sections can come from the
/// same map.  `None` means the map does not describe that kind of unit.
pub fn parse(
    unit_name: &str,
    config: &UnitConfig,
) -> (Option<MountConfig>, Option<AutomountConfig>) {
    if config.config_json.is_empty() {
        return (None, None);
    }
    let Ok(map) = sysa::ir::config::parse_json(&config.config_json) else {
        return (None, None);
    };

    let mount = (unit_name.ends_with(".mount") || map.contains_key("What")).then(|| {
        use sysa::ir::config::get_bool;
        use sysa::ir::config::get_string;
        use sysa::ir::config::get_u32_or;

        MountConfig {
            what: get_string(&map, "What").unwrap_or_default(),
            r#where: get_string(&map, "Where").unwrap_or_default(),
            r#type: get_string(&map, "Type").unwrap_or_default(),
            options: get_string(&map, "Options").unwrap_or_default(),
            timeout_sec: get_u32_or(&map, "TimeoutSec", 0),
            lazy_unmount: get_bool(&map, "LazyUnmount"),
            force_unmount: get_bool(&map, "ForceUnmount"),
            directory_mode: get_string(&map, "DirectoryMode").unwrap_or_default(),
            sloppy_options: get_bool(&map, "SloppyOptions"),
        }
    });

    let automount = (unit_name.ends_with(".automount")
        || map.contains_key("ExtraOptions")
        || map.contains_key("TimeoutIdleSec"))
    .then(|| {
        use sysa::ir::config::get_string;
        use sysa::ir::config::get_u32_or;

        AutomountConfig {
            r#where: get_string(&map, "Where").unwrap_or_default(),
            extra_options: get_string(&map, "ExtraOptions").unwrap_or_default(),
            timeout_idle_sec: get_u32_or(&map, "TimeoutIdleSec", 0),
            directory_mode: get_string(&map, "DirectoryMode").unwrap_or_default(),
        }
    });

    (mount, automount)
}
