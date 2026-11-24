//! Conversion between a parsed [`UnitFile`] and the flattened directive map
//! that travels in `UnitIR::config` and `UnitConfig::config_json`.
//!
//! The map mirrors the unit file: keys are the directive names exactly as
//! written in the section that belongs to the unit's type, a single-valued
//! directive maps to a JSON string and a repeated one to an array of strings.
//! Both directions are driven by the same key vocabulary, so a Finder → IR →
//! System A → worker round trip preserves every directive the parser knows.
//!
//! Unknown keys are ignored when applying: a partial IR update only touches
//! the directives it actually carries.

use serde_json::{json, Value};
use systema_sysf::ir::{config, ConfigMap};

use crate::types::{
    AutomountSection, DeviceSection, ExecCommand, MountSection, PathSection, ResourceControl,
    RestartPolicy, ScopeSection, ServiceSection, ServiceType, SocketSection, SwapSection,
    TimerSection, UnitFile, UnitKind,
};

// ---------------------------------------------------------------------------
// UnitFile -> config map
// ---------------------------------------------------------------------------

/// Serialize the unit's own type-specific section into a flattened map.
///
/// An empty map means the type has no configuration (targets, power units) or
/// the section was never populated.
pub fn to_config_map(uf: &UnitFile) -> ConfigMap {
    let mut map = ConfigMap::new();
    match uf.kind {
        UnitKind::Service => {
            if let Some(section) = &uf.service {
                put_service(&mut map, section);
            }
        }
        UnitKind::Socket => {
            if let Some(section) = &uf.socket {
                put_socket(&mut map, section);
            }
        }
        UnitKind::Mount => {
            if let Some(section) = &uf.mount {
                put_mount(&mut map, section);
            }
        }
        UnitKind::Automount => {
            if let Some(section) = &uf.automount {
                put_automount(&mut map, section);
            }
        }
        UnitKind::Timer => {
            if let Some(section) = &uf.timer {
                put_timer(&mut map, section);
            }
        }
        UnitKind::Swap => {
            if let Some(section) = &uf.swap {
                put_swap(&mut map, section);
            }
        }
        UnitKind::Slice => {
            if let Some(section) = &uf.slice {
                put_resource_control(&mut map, &section.rc);
            }
        }
        UnitKind::Scope => {
            if let Some(section) = &uf.scope {
                put_scope(&mut map, section);
            }
        }
        UnitKind::Device => {
            if let Some(section) = &uf.device {
                put_device(&mut map, section);
            }
        }
        UnitKind::Path => {
            if let Some(section) = &uf.path {
                put_path(&mut map, section);
            }
        }
        UnitKind::Target | UnitKind::Power | UnitKind::Unknown(_) => {}
    }
    map
}

fn put_service(map: &mut ConfigMap, section: &ServiceSection) {
    put_str(map, "Type", section.service_type.as_str());
    put_str(map, "BusName", &section.bus_name);
    put_execs(map, "ExecStart", &section.exec_start);
    put_execs(map, "ExecStartPre", &section.exec_start_pre);
    put_execs(map, "ExecStartPost", &section.exec_start_post);
    put_execs(map, "ExecStop", &section.exec_stop);
    put_execs(map, "ExecStopPost", &section.exec_stop_post);
    put_execs(map, "ExecReload", &section.exec_reload);
    put_str(map, "WorkingDirectory", &section.working_directory);
    put_str(map, "User", &section.user);
    put_str(map, "Group", &section.group);
    put_strings(map, "AmbientCapabilities", &section.ambient_capabilities);
    put_opt_strings(
        map,
        "CapabilityBoundingSet",
        &section.capability_bounding_set,
    );
    put_bool(map, "NoNewPrivileges", section.no_new_privileges);
    put_str(map, "PAMName", &section.pam_name);
    put_strings(map, "Environment", &section.environment);
    put_strings(map, "EnvironmentFile", &section.environment_file);
    put_strings(map, "StateDirectory", &section.state_directory);
    put_u32(map, "StateDirectoryMode", section.state_directory_mode);
    put_strings(map, "LogsDirectory", &section.logs_directory);
    put_u32(map, "LogsDirectoryMode", section.logs_directory_mode);
    put_strings(map, "RuntimeDirectory", &section.runtime_directory);
    put_u32(map, "RuntimeDirectoryMode", section.runtime_directory_mode);
    put_strings(
        map,
        "ConfigurationDirectory",
        &section.configuration_directory,
    );
    put_u32(
        map,
        "ConfigurationDirectoryMode",
        section.configuration_directory_mode,
    );
    put_str(map, "PIDFile", &section.pid_file);
    put_str(map, "Restart", section.restart.as_str());
    put_u32(map, "RestartSec", section.restart_sec);
    put_u32(map, "TimeoutStartSec", section.timeout_start_sec);
    put_u32(map, "TimeoutStopSec", section.timeout_stop_sec);
    put_bool(map, "RemainAfterExit", section.remain_after_exit);
    put_strings(map, "Sockets", &section.sockets);
    put_str(map, "NotifyAccess", &section.notify_access);
    put_u32(map, "WatchdogSec", section.watchdog_sec);
    put_str(map, "KillSignal", &section.kill_signal);
    put_str(map, "KillMode", &section.kill_mode);
    put_str(map, "StandardInput", &section.standard_input);
    put_str(map, "StandardOutput", &section.standard_output);
    put_str(map, "StandardError", &section.standard_error);
    put_str(map, "TTYPath", &section.tty_path);
    put_u32(map, "RestartSteps", section.restart_steps);
    put_u32(map, "RestartMaxDelaySec", section.restart_max_delay_sec);
    put_resource_control(map, &section.rc);
}

fn put_socket(map: &mut ConfigMap, section: &SocketSection) {
    put_strings(map, "ListenStream", &section.listen_stream);
    put_strings(map, "ListenDatagram", &section.listen_datagram);
    put_strings(
        map,
        "ListenSequentialPacket",
        &section.listen_sequential_packet,
    );
    put_strings(map, "ListenFIFO", &section.listen_fifo);
    put_strings(map, "ListenNetlink", &section.listen_netlink);
    put_strings(map, "ListenSpecial", &section.listen_special);
    put_bool(map, "Accept", section.accept);
    put_str(map, "Service", &section.service);
    put_str(map, "FileDescriptorName", &section.file_descriptor_name);
    put_str(map, "SocketUser", &section.socket_user);
    put_str(map, "SocketGroup", &section.socket_group);
    put_str(map, "SocketMode", &section.socket_mode);
    put_str(map, "DirectoryMode", &section.directory_mode);
    put_u32(map, "MaxConnections", section.max_connections);
    put_u32(map, "Backlog", section.backlog);
    put_str(map, "BindIPv6Only", &section.bind_ipv6_only);
    put_bool(map, "FreeBind", section.free_bind);
    put_bool(map, "Transparent", section.transparent);
    put_bool(map, "Broadcast", section.broadcast);
    put_bool(map, "PassCredentials", section.pass_credentials);
    put_bool(map, "PassSecurity", section.pass_security);
    put_u32(map, "TimeoutSec", section.timeout_sec);
    put_strings(map, "Symlinks", &section.symlinks);
    put_strings(map, "XAttrEntryPoint", &section.xattr_entrypoint);
    put_strings(map, "XAttrListen", &section.xattr_listen);
}

fn put_mount(map: &mut ConfigMap, section: &MountSection) {
    put_str(map, "What", &section.what);
    put_str(map, "Where", &section.where_);
    put_str(map, "Type", &section.type_);
    put_str(map, "Options", &section.options);
    put_u32(map, "TimeoutSec", section.timeout_sec);
    put_bool(map, "LazyUnmount", section.lazy_unmount);
    put_bool(map, "ForceUnmount", section.force_unmount);
    put_str(map, "DirectoryMode", &section.directory_mode);
    put_bool(map, "SloppyOptions", section.sloppy_options);
}

fn put_automount(map: &mut ConfigMap, section: &AutomountSection) {
    put_str(map, "Where", &section.where_);
    put_str(map, "ExtraOptions", &section.extra_options);
    put_u32(map, "TimeoutIdleSec", section.timeout_idle_sec);
    put_str(map, "DirectoryMode", &section.directory_mode);
}

fn put_timer(map: &mut ConfigMap, section: &TimerSection) {
    put_opt_u32(map, "OnActiveSec", section.on_active_sec);
    put_opt_u32(map, "OnBootSec", section.on_boot_sec);
    put_opt_u32(map, "OnStartupSec", section.on_startup_sec);
    put_opt_u32(map, "OnUnitActiveSec", section.on_unit_active_sec);
    put_opt_u32(map, "OnUnitInactiveSec", section.on_unit_inactive_sec);
    put_strings(map, "OnCalendar", &section.on_calendar);
    put_u32(map, "AccuracySec", section.accuracy_sec);
    put_u32(map, "RandomizedDelaySec", section.randomized_delay_sec);
    put_str(map, "Unit", &section.unit);
    put_bool(map, "Persistent", section.persistent);
    put_bool(map, "WakeSystem", section.wake_system);
    put_bool(map, "RemainAfterElapse", section.remain_after_elapse);
}

fn put_swap(map: &mut ConfigMap, section: &SwapSection) {
    put_str(map, "What", &section.what);
    put_i32(map, "Priority", section.priority);
    put_str(map, "Options", &section.options);
    put_u32(map, "TimeoutSec", section.timeout_sec);
}

fn put_scope(map: &mut ConfigMap, section: &ScopeSection) {
    put_strings(map, "PIDs", &section.pids);
    put_u32(map, "TimeoutStopSec", section.timeout_stop_sec);
    put_u32(map, "RuntimeMaxSec", section.runtime_max_sec);
    put_str(map, "KillMode", &section.kill_mode);
    put_str(map, "KillSignal", &section.kill_signal);
    put_bool(map, "SendSIGHUP", section.send_sighup);
    put_resource_control(map, &section.rc);
}

fn put_device(map: &mut ConfigMap, section: &DeviceSection) {
    put_strings(map, "Property", &section.property);
    put_str(map, "SysfsPath", &section.sysfs_path);
    put_str(map, "DeviceName", &section.device_name);
    put_str(map, "DevicePath", &section.device_path);
}

fn put_path(map: &mut ConfigMap, section: &PathSection) {
    put_strings(map, "PathExists", &section.path_exists);
    put_strings(map, "PathExistsGlob", &section.path_exists_glob);
    put_strings(map, "PathChanged", &section.path_changed);
    put_strings(map, "PathModified", &section.path_modified);
    put_strings(map, "DirectoryNotEmpty", &section.directory_not_empty);
    put_str(map, "Unit", &section.unit);
    put_bool(map, "MakeDirectory", section.make_directory);
    put_str(map, "DirectoryMode", &section.directory_mode);
    put_u32(
        map,
        "TriggerLimitIntervalSec",
        section.trigger_limit_interval_sec,
    );
    put_u32(map, "TriggerLimitBurst", section.trigger_limit_burst);
}

fn put_resource_control(map: &mut ConfigMap, rc: &ResourceControl) {
    put_str(map, "CPUQuota", &rc.cpu_quota);
    put_str(map, "CPUQuotaPeriodSec", &rc.cpu_quota_period);
    put_u32(map, "CPUWeight", rc.cpu_weight);
    put_u32(map, "StartupCPUWeight", rc.startup_cpu_weight);
    put_str(map, "CPUSetCPUs", &rc.cpu_set_cpus);
    put_str(map, "CPUSetMemoryNodes", &rc.cpu_set_memory_nodes);
    put_str(map, "MemoryMin", &rc.memory_min);
    put_str(map, "MemoryLow", &rc.memory_low);
    put_str(map, "MemoryHigh", &rc.memory_high);
    put_str(map, "MemoryMax", &rc.memory_max);
    put_str(map, "MemorySwapMax", &rc.memory_swap_max);
    put_u32(map, "IOWeight", rc.io_weight);
    put_u32(map, "StartupIOWeight", rc.startup_io_weight);
    put_strings(map, "IODeviceWeight", &rc.io_device_weight);
    put_strings(map, "IOReadBandwidthMax", &rc.io_read_bandwidth_max);
    put_strings(map, "IOWriteBandwidthMax", &rc.io_write_bandwidth_max);
    put_u32(map, "TasksMax", rc.tasks_max);
    put_str(map, "AllowedCPUs", &rc.allowed_cpus);
    put_str(map, "AllowedMemoryNodes", &rc.allowed_memory_nodes);
    put_bool(map, "Delegate", rc.delegate);
    put_strings(map, "DelegateControllers", &rc.delegate_controllers);
    put_str(map, "DelegateSubgroup", &rc.delegate_subgroup);
}

fn put_str(map: &mut ConfigMap, key: &str, value: &str) {
    if !value.is_empty() {
        map.insert(key.to_string(), Value::String(value.to_string()));
    }
}

fn put_strings(map: &mut ConfigMap, key: &str, values: &[String]) {
    if !values.is_empty() {
        map.insert(key.to_string(), json!(values));
    }
}

/// Serialize an optional list so `None` (directive absent) and `Some([])`
/// (directive explicitly emptied) stay distinguishable across the wire.
fn put_opt_strings(map: &mut ConfigMap, key: &str, values: &Option<Vec<String>>) {
    if let Some(values) = values {
        if values.is_empty() {
            map.insert(key.to_string(), Value::String(String::new()));
        } else {
            map.insert(key.to_string(), json!(values));
        }
    }
}

fn put_execs(map: &mut ConfigMap, key: &str, commands: &[ExecCommand]) {
    if !commands.is_empty() {
        let raw: Vec<&str> = commands
            .iter()
            .map(|command| command.raw.as_str())
            .collect();
        map.insert(key.to_string(), json!(raw));
    }
}

fn put_bool(map: &mut ConfigMap, key: &str, value: bool) {
    map.insert(key.to_string(), Value::Bool(value));
}

fn put_u32(map: &mut ConfigMap, key: &str, value: u32) {
    map.insert(key.to_string(), json!(value));
}

fn put_i32(map: &mut ConfigMap, key: &str, value: i32) {
    map.insert(key.to_string(), json!(value));
}

fn put_opt_u32(map: &mut ConfigMap, key: &str, value: Option<u32>) {
    if let Some(value) = value {
        put_u32(map, key, value);
    }
}

// ---------------------------------------------------------------------------
// Config map -> UnitFile
// ---------------------------------------------------------------------------

/// Apply the directives of `map` to the unit's own section, creating it when
/// necessary.
///
/// Only keys present in the map are applied: this is what makes an IR commit
/// a partial update.  Directives that do not belong to the unit's type are
/// ignored.
pub fn apply_config_map(uf: &mut UnitFile, map: &ConfigMap) {
    match uf.kind {
        UnitKind::Service => apply_service(uf.service.get_or_insert_with(Default::default), map),
        UnitKind::Socket => apply_socket(uf.socket.get_or_insert_with(Default::default), map),
        UnitKind::Mount => apply_mount(uf.mount.get_or_insert_with(Default::default), map),
        UnitKind::Automount => {
            apply_automount(uf.automount.get_or_insert_with(Default::default), map)
        }
        UnitKind::Timer => apply_timer(uf.timer.get_or_insert_with(Default::default), map),
        UnitKind::Swap => apply_swap(uf.swap.get_or_insert_with(Default::default), map),
        UnitKind::Slice => {
            apply_resource_control(&mut uf.slice.get_or_insert_with(Default::default).rc, map)
        }
        UnitKind::Scope => apply_scope(uf.scope.get_or_insert_with(Default::default), map),
        UnitKind::Device => apply_device(uf.device.get_or_insert_with(Default::default), map),
        UnitKind::Path => apply_path(uf.path.get_or_insert_with(Default::default), map),
        UnitKind::Target | UnitKind::Power | UnitKind::Unknown(_) => {}
    }
}

fn apply_service(section: &mut ServiceSection, map: &ConfigMap) {
    if let Some(value) = config::get_string(map, "Type") {
        section.service_type = ServiceType::from(value.as_str());
    }
    apply_str(map, "BusName", &mut section.bus_name);
    apply_execs(map, "ExecStart", &mut section.exec_start);
    apply_execs(map, "ExecStartPre", &mut section.exec_start_pre);
    apply_execs(map, "ExecStartPost", &mut section.exec_start_post);
    apply_execs(map, "ExecStop", &mut section.exec_stop);
    apply_execs(map, "ExecStopPost", &mut section.exec_stop_post);
    apply_execs(map, "ExecReload", &mut section.exec_reload);
    apply_str(map, "WorkingDirectory", &mut section.working_directory);
    apply_str(map, "User", &mut section.user);
    apply_str(map, "Group", &mut section.group);
    apply_strings(
        map,
        "AmbientCapabilities",
        &mut section.ambient_capabilities,
    );
    apply_opt_strings(
        map,
        "CapabilityBoundingSet",
        &mut section.capability_bounding_set,
    );
    apply_bool(map, "NoNewPrivileges", &mut section.no_new_privileges);
    apply_str(map, "PAMName", &mut section.pam_name);
    apply_strings(map, "Environment", &mut section.environment);
    apply_strings(map, "EnvironmentFile", &mut section.environment_file);
    apply_strings(map, "StateDirectory", &mut section.state_directory);
    apply_u32(map, "StateDirectoryMode", &mut section.state_directory_mode);
    apply_strings(map, "LogsDirectory", &mut section.logs_directory);
    apply_u32(map, "LogsDirectoryMode", &mut section.logs_directory_mode);
    apply_strings(map, "RuntimeDirectory", &mut section.runtime_directory);
    apply_u32(
        map,
        "RuntimeDirectoryMode",
        &mut section.runtime_directory_mode,
    );
    apply_strings(
        map,
        "ConfigurationDirectory",
        &mut section.configuration_directory,
    );
    apply_u32(
        map,
        "ConfigurationDirectoryMode",
        &mut section.configuration_directory_mode,
    );
    apply_str(map, "PIDFile", &mut section.pid_file);
    if let Some(value) = config::get_string(map, "Restart") {
        section.restart = RestartPolicy::from(value.as_str());
    }
    apply_u32(map, "RestartSec", &mut section.restart_sec);
    apply_u32(map, "TimeoutStartSec", &mut section.timeout_start_sec);
    apply_u32(map, "TimeoutStopSec", &mut section.timeout_stop_sec);
    apply_bool(map, "RemainAfterExit", &mut section.remain_after_exit);
    apply_strings(map, "Sockets", &mut section.sockets);
    apply_str(map, "NotifyAccess", &mut section.notify_access);
    apply_u32(map, "WatchdogSec", &mut section.watchdog_sec);
    apply_str(map, "KillSignal", &mut section.kill_signal);
    apply_str(map, "KillMode", &mut section.kill_mode);
    apply_str(map, "StandardInput", &mut section.standard_input);
    apply_str(map, "StandardOutput", &mut section.standard_output);
    apply_str(map, "StandardError", &mut section.standard_error);
    apply_str(map, "TTYPath", &mut section.tty_path);
    apply_u32(map, "RestartSteps", &mut section.restart_steps);
    apply_u32(
        map,
        "RestartMaxDelaySec",
        &mut section.restart_max_delay_sec,
    );
    apply_resource_control(&mut section.rc, map);
}

fn apply_socket(section: &mut SocketSection, map: &ConfigMap) {
    apply_strings(map, "ListenStream", &mut section.listen_stream);
    apply_strings(map, "ListenDatagram", &mut section.listen_datagram);
    apply_strings(
        map,
        "ListenSequentialPacket",
        &mut section.listen_sequential_packet,
    );
    apply_strings(map, "ListenFIFO", &mut section.listen_fifo);
    apply_strings(map, "ListenNetlink", &mut section.listen_netlink);
    apply_strings(map, "ListenSpecial", &mut section.listen_special);
    apply_bool(map, "Accept", &mut section.accept);
    apply_str(map, "Service", &mut section.service);
    apply_str(map, "FileDescriptorName", &mut section.file_descriptor_name);
    apply_str(map, "SocketUser", &mut section.socket_user);
    apply_str(map, "SocketGroup", &mut section.socket_group);
    apply_str(map, "SocketMode", &mut section.socket_mode);
    apply_str(map, "DirectoryMode", &mut section.directory_mode);
    apply_u32(map, "MaxConnections", &mut section.max_connections);
    apply_u32(map, "Backlog", &mut section.backlog);
    apply_str(map, "BindIPv6Only", &mut section.bind_ipv6_only);
    apply_bool(map, "FreeBind", &mut section.free_bind);
    apply_bool(map, "Transparent", &mut section.transparent);
    apply_bool(map, "Broadcast", &mut section.broadcast);
    apply_bool(map, "PassCredentials", &mut section.pass_credentials);
    apply_bool(map, "PassSecurity", &mut section.pass_security);
    apply_u32(map, "TimeoutSec", &mut section.timeout_sec);
    apply_strings(map, "Symlinks", &mut section.symlinks);
    apply_strings(map, "XAttrEntryPoint", &mut section.xattr_entrypoint);
    apply_strings(map, "XAttrListen", &mut section.xattr_listen);
}

fn apply_mount(section: &mut MountSection, map: &ConfigMap) {
    apply_str(map, "What", &mut section.what);
    apply_str(map, "Where", &mut section.where_);
    apply_str(map, "Type", &mut section.type_);
    apply_str(map, "Options", &mut section.options);
    apply_u32(map, "TimeoutSec", &mut section.timeout_sec);
    apply_bool(map, "LazyUnmount", &mut section.lazy_unmount);
    apply_bool(map, "ForceUnmount", &mut section.force_unmount);
    apply_str(map, "DirectoryMode", &mut section.directory_mode);
    apply_bool(map, "SloppyOptions", &mut section.sloppy_options);
}

fn apply_automount(section: &mut AutomountSection, map: &ConfigMap) {
    apply_str(map, "Where", &mut section.where_);
    apply_str(map, "ExtraOptions", &mut section.extra_options);
    apply_u32(map, "TimeoutIdleSec", &mut section.timeout_idle_sec);
    apply_str(map, "DirectoryMode", &mut section.directory_mode);
}

fn apply_timer(section: &mut TimerSection, map: &ConfigMap) {
    if map.contains_key("OnActiveSec") {
        section.on_active_sec = config::get_u32(map, "OnActiveSec");
    }
    if map.contains_key("OnBootSec") {
        section.on_boot_sec = config::get_u32(map, "OnBootSec");
    }
    if map.contains_key("OnStartupSec") {
        section.on_startup_sec = config::get_u32(map, "OnStartupSec");
    }
    if map.contains_key("OnUnitActiveSec") {
        section.on_unit_active_sec = config::get_u32(map, "OnUnitActiveSec");
    }
    if map.contains_key("OnUnitInactiveSec") {
        section.on_unit_inactive_sec = config::get_u32(map, "OnUnitInactiveSec");
    }
    apply_strings(map, "OnCalendar", &mut section.on_calendar);
    apply_u32(map, "AccuracySec", &mut section.accuracy_sec);
    apply_u32(map, "RandomizedDelaySec", &mut section.randomized_delay_sec);
    apply_str(map, "Unit", &mut section.unit);
    apply_bool(map, "Persistent", &mut section.persistent);
    apply_bool(map, "WakeSystem", &mut section.wake_system);
    apply_bool(map, "RemainAfterElapse", &mut section.remain_after_elapse);
}

fn apply_swap(section: &mut SwapSection, map: &ConfigMap) {
    apply_str(map, "What", &mut section.what);
    if map.contains_key("Priority") {
        section.priority = config::get_i32_or(map, "Priority", section.priority);
    }
    apply_str(map, "Options", &mut section.options);
    apply_u32(map, "TimeoutSec", &mut section.timeout_sec);
}

fn apply_scope(section: &mut ScopeSection, map: &ConfigMap) {
    apply_strings(map, "PIDs", &mut section.pids);
    apply_u32(map, "TimeoutStopSec", &mut section.timeout_stop_sec);
    apply_u32(map, "RuntimeMaxSec", &mut section.runtime_max_sec);
    apply_str(map, "KillMode", &mut section.kill_mode);
    apply_str(map, "KillSignal", &mut section.kill_signal);
    apply_bool(map, "SendSIGHUP", &mut section.send_sighup);
    apply_resource_control(&mut section.rc, map);
}

fn apply_device(section: &mut DeviceSection, map: &ConfigMap) {
    apply_strings(map, "Property", &mut section.property);
    apply_str(map, "SysfsPath", &mut section.sysfs_path);
    apply_str(map, "DeviceName", &mut section.device_name);
    apply_str(map, "DevicePath", &mut section.device_path);
}

fn apply_path(section: &mut PathSection, map: &ConfigMap) {
    apply_strings(map, "PathExists", &mut section.path_exists);
    apply_strings(map, "PathExistsGlob", &mut section.path_exists_glob);
    apply_strings(map, "PathChanged", &mut section.path_changed);
    apply_strings(map, "PathModified", &mut section.path_modified);
    apply_strings(map, "DirectoryNotEmpty", &mut section.directory_not_empty);
    apply_str(map, "Unit", &mut section.unit);
    apply_bool(map, "MakeDirectory", &mut section.make_directory);
    apply_str(map, "DirectoryMode", &mut section.directory_mode);
    apply_u32(
        map,
        "TriggerLimitIntervalSec",
        &mut section.trigger_limit_interval_sec,
    );
    apply_u32(map, "TriggerLimitBurst", &mut section.trigger_limit_burst);
}

fn apply_resource_control(rc: &mut ResourceControl, map: &ConfigMap) {
    apply_str(map, "CPUQuota", &mut rc.cpu_quota);
    apply_str(map, "CPUQuotaPeriodSec", &mut rc.cpu_quota_period);
    apply_u32(map, "CPUWeight", &mut rc.cpu_weight);
    apply_u32(map, "StartupCPUWeight", &mut rc.startup_cpu_weight);
    apply_str(map, "CPUSetCPUs", &mut rc.cpu_set_cpus);
    apply_str(map, "CPUSetMemoryNodes", &mut rc.cpu_set_memory_nodes);
    apply_str(map, "MemoryMin", &mut rc.memory_min);
    apply_str(map, "MemoryLow", &mut rc.memory_low);
    apply_str(map, "MemoryHigh", &mut rc.memory_high);
    apply_str(map, "MemoryMax", &mut rc.memory_max);
    apply_str(map, "MemorySwapMax", &mut rc.memory_swap_max);
    apply_u32(map, "IOWeight", &mut rc.io_weight);
    apply_u32(map, "StartupIOWeight", &mut rc.startup_io_weight);
    apply_strings(map, "IODeviceWeight", &mut rc.io_device_weight);
    apply_strings(map, "IOReadBandwidthMax", &mut rc.io_read_bandwidth_max);
    apply_strings(map, "IOWriteBandwidthMax", &mut rc.io_write_bandwidth_max);
    apply_u32(map, "TasksMax", &mut rc.tasks_max);
    apply_str(map, "AllowedCPUs", &mut rc.allowed_cpus);
    apply_str(map, "AllowedMemoryNodes", &mut rc.allowed_memory_nodes);
    apply_bool(map, "Delegate", &mut rc.delegate);
    apply_strings(map, "DelegateControllers", &mut rc.delegate_controllers);
    apply_str(map, "DelegateSubgroup", &mut rc.delegate_subgroup);
}

fn apply_str(map: &ConfigMap, key: &str, target: &mut String) {
    if let Some(value) = config::get_string(map, key) {
        *target = value;
    }
}

fn apply_strings(map: &ConfigMap, key: &str, target: &mut Vec<String>) {
    if map.contains_key(key) {
        *target = config::get_strings(map, key);
    }
}

fn apply_opt_strings(map: &ConfigMap, key: &str, target: &mut Option<Vec<String>>) {
    if map.contains_key(key) {
        // The empty-string sentinel written by `put_opt_strings` represents
        // "present but empty"; `get_strings` would turn it into `[""]`.
        *target = Some(
            config::get_strings(map, key)
                .into_iter()
                .filter(|value| !value.is_empty())
                .collect(),
        );
    }
}

fn apply_execs(map: &ConfigMap, key: &str, target: &mut Vec<ExecCommand>) {
    if map.contains_key(key) {
        *target = config::get_strings(map, key)
            .iter()
            .map(|raw| ExecCommand::parse(raw.as_str()))
            .collect();
    }
}

fn apply_bool(map: &ConfigMap, key: &str, target: &mut bool) {
    if map.contains_key(key) {
        *target = config::get_bool(map, key);
    }
}

fn apply_u32(map: &ConfigMap, key: &str, target: &mut u32) {
    if let Some(value) = config::get_u32(map, key) {
        *target = value;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_directives_round_trip_with_explicit_empty_bounding_set() {
        let mut unit = UnitFile::new("caps.service");
        let svc = unit.service.get_or_insert_with(Default::default);
        svc.ambient_capabilities = vec!["CAP_NET_BIND_SERVICE".to_string()];
        svc.capability_bounding_set = Some(Vec::new());
        svc.no_new_privileges = true;

        let map = to_config_map(&unit);
        assert_eq!(
            map.get("AmbientCapabilities"),
            Some(&json!(["CAP_NET_BIND_SERVICE"]))
        );
        assert_eq!(
            map.get("CapabilityBoundingSet"),
            Some(&Value::String(String::new()))
        );
        assert_eq!(map.get("NoNewPrivileges"), Some(&Value::Bool(true)));

        let mut back = UnitFile::new("caps.service");
        apply_config_map(&mut back, &map);
        let back = back.service.unwrap();
        assert_eq!(
            back.ambient_capabilities,
            vec!["CAP_NET_BIND_SERVICE".to_string()]
        );
        assert_eq!(back.capability_bounding_set, Some(Vec::new()));
        assert!(back.no_new_privileges);
    }

    #[test]
    fn socket_symlinks_and_xattrs_round_trip() {
        let mut unit = UnitFile::new("journal.socket");
        let sock = unit.socket.get_or_insert_with(Default::default);
        sock.symlinks = vec!["/dev/log".to_string()];
        sock.xattr_entrypoint = vec!["user.varlink.entrypoint=/run/varlink".to_string()];
        sock.xattr_listen = vec!["user.varlink.listen=yes".to_string()];

        let map = to_config_map(&unit);
        assert_eq!(map.get("Symlinks"), Some(&json!(["/dev/log"])));
        assert_eq!(
            map.get("XAttrEntryPoint"),
            Some(&json!(["user.varlink.entrypoint=/run/varlink"]))
        );
        assert_eq!(
            map.get("XAttrListen"),
            Some(&json!(["user.varlink.listen=yes"]))
        );

        let mut back = UnitFile::new("journal.socket");
        apply_config_map(&mut back, &map);
        let back = back.socket.unwrap();
        assert_eq!(back.symlinks, ["/dev/log"]);
        assert_eq!(
            back.xattr_entrypoint,
            ["user.varlink.entrypoint=/run/varlink"]
        );
        assert_eq!(back.xattr_listen, ["user.varlink.listen=yes"]);
    }
}
