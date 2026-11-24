//! `[Service]` directive parsing for the service worker.
//!
//! System A forwards the section as a flattened map of systemd directives;
//! this module is the only place that knows which keys a service cares about.

use sysa::proto::UnitConfig;

/// The directives of a `[Service]` section.
#[derive(Debug, Clone, Default)]
pub struct ServiceConfig {
    pub exec_start: String,
    pub exec_start_pre: Vec<String>,
    pub exec_start_post: Vec<String>,
    pub exec_stop: Vec<String>,
    pub exec_stop_post: Vec<String>,
    pub exec_reload: Vec<String>,
    pub working_directory: String,
    pub user: String,
    pub group: String,
    pub ambient_capabilities: Vec<String>,
    pub capability_bounding_set: Option<Vec<String>>,
    pub no_new_privileges: bool,
    pub pam_name: String,
    pub environment: Vec<String>,
    pub service_type: String,
    pub timeout_start_secs: u32,
    pub timeout_stop_secs: u32,
    pub standard_input: String,
    pub standard_output: String,
    pub standard_error: String,
    pub tty_path: String,
    pub remain_after_exit: bool,
    pub bus_name: String,
    pub pid_file: String,
    pub state_directory: Vec<String>,
    pub state_directory_mode: u32,
    pub logs_directory: Vec<String>,
    pub logs_directory_mode: u32,
    pub runtime_directory: Vec<String>,
    pub runtime_directory_mode: u32,
    pub configuration_directory: Vec<String>,
    pub configuration_directory_mode: u32,
}

impl ServiceConfig {
    /// Parse the service section out of a unit config.
    ///
    /// `None` means System A sent no section at all (an ExecStart-less action
    /// service).
    pub fn from_unit_config(config: &UnitConfig) -> Option<Self> {
        if config.config_json.is_empty() {
            return None;
        }
        let map = sysa::ir::config::parse_json(&config.config_json).ok()?;
        Some(Self::from_map(&map))
    }

    fn from_map(map: &sysa::ir::ConfigMap) -> Self {
        use sysa::ir::config::{get_bool, get_mode, get_string, get_strings, get_u32_or};

        ServiceConfig {
            exec_start: get_strings(map, "ExecStart")
                .into_iter()
                .next()
                .unwrap_or_default(),
            exec_start_pre: get_strings(map, "ExecStartPre"),
            exec_start_post: get_strings(map, "ExecStartPost"),
            exec_stop: get_strings(map, "ExecStop"),
            exec_stop_post: get_strings(map, "ExecStopPost"),
            exec_reload: get_strings(map, "ExecReload"),
            working_directory: get_string(map, "WorkingDirectory").unwrap_or_default(),
            user: get_string(map, "User").unwrap_or_default(),
            group: get_string(map, "Group").unwrap_or_default(),
            ambient_capabilities: get_strings(map, "AmbientCapabilities"),
            capability_bounding_set: map.contains_key("CapabilityBoundingSet").then(|| {
                get_strings(map, "CapabilityBoundingSet")
                    .into_iter()
                    .filter(|value| !value.is_empty())
                    .collect()
            }),
            no_new_privileges: get_bool(map, "NoNewPrivileges"),
            pam_name: get_string(map, "PAMName").unwrap_or_default(),
            environment: get_strings(map, "Environment"),
            service_type: get_string(map, "Type").unwrap_or_default(),
            timeout_start_secs: get_u32_or(map, "TimeoutStartSec", 0),
            timeout_stop_secs: get_u32_or(map, "TimeoutStopSec", 0),
            standard_input: get_string(map, "StandardInput").unwrap_or_default(),
            standard_output: get_string(map, "StandardOutput").unwrap_or_default(),
            standard_error: get_string(map, "StandardError").unwrap_or_default(),
            tty_path: get_string(map, "TTYPath").unwrap_or_default(),
            remain_after_exit: get_bool(map, "RemainAfterExit"),
            bus_name: get_string(map, "BusName").unwrap_or_default(),
            pid_file: get_string(map, "PIDFile").unwrap_or_default(),
            state_directory: get_strings(map, "StateDirectory"),
            state_directory_mode: get_mode(map, "StateDirectoryMode").unwrap_or(0),
            logs_directory: get_strings(map, "LogsDirectory"),
            logs_directory_mode: get_mode(map, "LogsDirectoryMode").unwrap_or(0),
            runtime_directory: get_strings(map, "RuntimeDirectory"),
            runtime_directory_mode: get_mode(map, "RuntimeDirectoryMode").unwrap_or(0),
            configuration_directory: get_strings(map, "ConfigurationDirectory"),
            configuration_directory_mode: get_mode(map, "ConfigurationDirectoryMode").unwrap_or(0),
        }
    }

    /// Serialize back into the directive map (tests build unit configs this
    /// way).
    #[cfg(test)]
    pub fn to_config_json(&self) -> Vec<u8> {
        use serde_json::{json, Value};

        let mut map = sysa::ir::ConfigMap::new();
        let mut put = |key: &str, value: Value| {
            map.insert(key.to_string(), value);
        };
        if !self.exec_start.is_empty() {
            put("ExecStart", json!([self.exec_start]));
        }
        if !self.exec_start_pre.is_empty() {
            put("ExecStartPre", json!(self.exec_start_pre));
        }
        if !self.exec_start_post.is_empty() {
            put("ExecStartPost", json!(self.exec_start_post));
        }
        if !self.exec_stop.is_empty() {
            put("ExecStop", json!(self.exec_stop));
        }
        if !self.exec_stop_post.is_empty() {
            put("ExecStopPost", json!(self.exec_stop_post));
        }
        if !self.exec_reload.is_empty() {
            put("ExecReload", json!(self.exec_reload));
        }
        for (key, value) in [
            ("WorkingDirectory", &self.working_directory),
            ("User", &self.user),
            ("Group", &self.group),
            ("PAMName", &self.pam_name),
            ("Type", &self.service_type),
            ("StandardInput", &self.standard_input),
            ("StandardOutput", &self.standard_output),
            ("StandardError", &self.standard_error),
            ("TTYPath", &self.tty_path),
            ("BusName", &self.bus_name),
            ("PIDFile", &self.pid_file),
        ] {
            if !value.is_empty() {
                put(key, json!(value));
            }
        }
        if !self.environment.is_empty() {
            put("Environment", json!(self.environment));
        }
        if !self.ambient_capabilities.is_empty() {
            put("AmbientCapabilities", json!(self.ambient_capabilities));
        }
        if let Some(bounds) = &self.capability_bounding_set {
            if bounds.is_empty() {
                put("CapabilityBoundingSet", json!(""));
            } else {
                put("CapabilityBoundingSet", json!(bounds));
            }
        }
        if self.no_new_privileges {
            put("NoNewPrivileges", json!(true));
        }
        if !self.state_directory.is_empty() {
            put("StateDirectory", json!(self.state_directory));
        }
        if !self.logs_directory.is_empty() {
            put("LogsDirectory", json!(self.logs_directory));
        }
        if !self.runtime_directory.is_empty() {
            put("RuntimeDirectory", json!(self.runtime_directory));
        }
        if !self.configuration_directory.is_empty() {
            put(
                "ConfigurationDirectory",
                json!(self.configuration_directory),
            );
        }
        put("TimeoutStartSec", json!(self.timeout_start_secs));
        put("TimeoutStopSec", json!(self.timeout_stop_secs));
        put("RemainAfterExit", json!(self.remain_after_exit));
        put("StateDirectoryMode", json!(self.state_directory_mode));
        put("LogsDirectoryMode", json!(self.logs_directory_mode));
        put("RuntimeDirectoryMode", json!(self.runtime_directory_mode));
        put(
            "ConfigurationDirectoryMode",
            json!(self.configuration_directory_mode),
        );
        serde_json::to_vec(&map).expect("a directive map is always serializable")
    }
}
