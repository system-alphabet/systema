use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use systema_sysf::ir::{
    self, AutomountConfig, Condition, DependencySet, ExecCommand, MountConfig, ServiceConfig,
    SocketConfig, TimerConfig, UnitIR, UnitType,
};
use systema_sysf::Finder;

use crate::loader;
use crate::types::{
    ExecCommand as SdExecCommand, ResourceControl as SdResourceControl,
    RestartPolicy as SdRestartPolicy, ServiceSection, UnitFile, UnitKind,
};

/// Systemd implementation of the [`Finder`] trait.
///
/// Parses `.service`, `.target`, `.mount`, `.timer`, `.socket`, etc.
/// unit files from standard systemd search paths and converts them
/// into the unified [`UnitIR`] representation.
pub struct SystemdFinder;

#[async_trait]
impl Finder for SystemdFinder {
    fn name(&self) -> &str {
        "systemd"
    }

    async fn find_all(&self) -> Result<HashMap<String, UnitIR>> {
        let files = loader::discover_all()?;
        let mut map: HashMap<String, UnitIR> = HashMap::new();
        for file in files {
            merge_unit_ir(&mut map, &file);
        }
        Ok(map)
    }

    async fn find_one(&self, id: &str) -> Result<Option<UnitIR>> {
        let file = loader::discover_one(id)?;
        Ok(file.as_ref().map(convert_unit_file))
    }
}

/// Merge one discovered unit file into the canonical-name map.
///
/// The same canonical unit can be discovered more than once (e.g. an `/etc`
/// symlink alias plus the real file under `/usr/lib`).  Keep the first
/// (highest-precedence) definition and only merge in any extra aliases it may
/// have missed.
fn merge_unit_ir(map: &mut HashMap<String, UnitIR>, file: &UnitFile) {
    let ir = convert_unit_file(file);
    match map.get_mut(&file.name) {
        Some(existing) => {
            for alias in &ir.aliases {
                if !existing.aliases.contains(alias) {
                    existing.aliases.push(alias.clone());
                }
            }
        }
        None => {
            map.insert(file.name.clone(), ir);
        }
    }
}

fn convert_unit_file(uf: &UnitFile) -> UnitIR {
    UnitIR {
        id: uf.name.clone(),
        unit_type: Some(convert_unit_kind(&uf.kind)),
        description: Some(uf.unit.description.clone()),
        source_format: Some("systemd".to_string()),
        source_path: None,
        slice: (!uf.unit.slice.is_empty()).then(|| uf.unit.slice.clone()),
        dependencies: Some(convert_dependencies(uf)),
        service: uf.service.as_ref().map(convert_service),
        mount: uf.mount.as_ref().map(convert_mount),
        automount: uf.automount.as_ref().map(convert_automount),
        timer: uf.timer.as_ref().map(convert_timer),
        socket: uf.socket.as_ref().map(convert_socket),
        conditions: Some(convert_conditions(uf)),
        asserts: Some(convert_asserts(uf)),
        wanted_by: Some(uf.install.wanted_by.iter().cloned().collect()),
        required_by: Some(uf.install.required_by.iter().cloned().collect()),
        aliases: uf.install.alias.clone(),
        resource_control: extract_resource_control(uf),
    }
}

/// Pull the parsed resource-control directives from the unit's cgroup-owned
/// section (`[Service]`, `[Slice]`, or `[Scope]`) into the unified IR.
fn extract_resource_control(uf: &UnitFile) -> Option<ir::ResourceControl> {
    let rc = match &uf.kind {
        UnitKind::Service => uf.service.as_ref().map(|s| &s.rc),
        UnitKind::Slice => uf.slice.as_ref().map(|s| &s.rc),
        UnitKind::Scope => uf.scope.as_ref().map(|s| &s.rc),
        _ => return None,
    };
    rc.map(convert_resource_control)
}

fn convert_resource_control(rc: &SdResourceControl) -> ir::ResourceControl {
    ir::ResourceControl {
        cpu_quota: rc.cpu_quota.clone(),
        cpu_quota_period: rc.cpu_quota_period.clone(),
        cpu_weight: rc.cpu_weight,
        startup_cpu_weight: rc.startup_cpu_weight,
        cpu_set_cpus: rc.cpu_set_cpus.clone(),
        cpu_set_memory_nodes: rc.cpu_set_memory_nodes.clone(),
        memory_min: rc.memory_min.clone(),
        memory_low: rc.memory_low.clone(),
        memory_high: rc.memory_high.clone(),
        memory_max: rc.memory_max.clone(),
        memory_swap_max: rc.memory_swap_max.clone(),
        io_weight: rc.io_weight,
        startup_io_weight: rc.startup_io_weight,
        io_device_weight: rc.io_device_weight.clone(),
        io_read_bandwidth_max: rc.io_read_bandwidth_max.clone(),
        io_write_bandwidth_max: rc.io_write_bandwidth_max.clone(),
        tasks_max: rc.tasks_max,
        allowed_cpus: rc.allowed_cpus.clone(),
        allowed_memory_nodes: rc.allowed_memory_nodes.clone(),
    }
}

fn convert_unit_kind(kind: &UnitKind) -> UnitType {
    match kind {
        UnitKind::Service => UnitType::Service,
        UnitKind::Target => UnitType::Target,
        UnitKind::Mount => UnitType::Mount,
        UnitKind::Automount => UnitType::Automount,
        UnitKind::Timer => UnitType::Timer,
        UnitKind::Socket => UnitType::Socket,
        UnitKind::Slice => UnitType::Slice,
        UnitKind::Scope => UnitType::Scope,
        UnitKind::Swap => UnitType::Swap,
        UnitKind::Path => UnitType::Path,
        UnitKind::Device => UnitType::Device,
        UnitKind::Power => UnitType::Power,
        UnitKind::Unknown(s) => UnitType::Other(s.clone()),
    }
}

fn convert_dependencies(uf: &UnitFile) -> DependencySet {
    DependencySet {
        after: uf.unit.after.clone(),
        before: uf.unit.before.clone(),
        requires: uf.unit.requires.clone(),
        wants: uf.unit.wants.clone(),
        conflicts: uf.unit.conflicts.clone(),
        binds_to: uf.unit.binds_to.clone(),
        requisite: uf.unit.requisite.clone(),
        part_of: uf.unit.part_of.clone(),
        upholds: uf.unit.upholds.clone(),
        on_success: uf.unit.on_success.clone(),
        on_failure: uf.unit.on_failure.clone(),
        propagates_reload_to: uf.unit.propagates_reload_to.clone(),
        success_action: uf.unit.success_action.as_str().to_string(),
        default_dependencies: uf.unit.default_dependencies,
    }
}

fn convert_exec(cmd: &SdExecCommand) -> ExecCommand {
    ExecCommand {
        raw: cmd.raw.clone(),
        program: cmd.program.clone(),
        args: cmd.args.clone(),
        ignore_failure: cmd.ignore_failure,
        privileged: cmd.privileged,
    }
}

fn convert_restart(policy: &SdRestartPolicy) -> ir::RestartPolicy {
    match policy {
        SdRestartPolicy::No => ir::RestartPolicy::No,
        SdRestartPolicy::OnSuccess => ir::RestartPolicy::OnSuccess,
        SdRestartPolicy::OnFailure => ir::RestartPolicy::OnFailure,
        SdRestartPolicy::OnAbnormal => ir::RestartPolicy::OnAbnormal,
        SdRestartPolicy::OnWatchdog => ir::RestartPolicy::OnWatchdog,
        SdRestartPolicy::OnAbort => ir::RestartPolicy::OnAbort,
        SdRestartPolicy::Always => ir::RestartPolicy::Always,
    }
}

fn convert_service(svc: &ServiceSection) -> ServiceConfig {
    ServiceConfig {
        exec_start: svc.exec_start.iter().map(convert_exec).collect(),
        exec_stop: svc.exec_stop.iter().map(convert_exec).collect(),
        exec_reload: svc.exec_reload.iter().map(convert_exec).collect(),
        exec_start_pre: svc.exec_start_pre.iter().map(convert_exec).collect(),
        exec_start_post: svc.exec_start_post.iter().map(convert_exec).collect(),
        exec_stop_post: svc.exec_stop_post.iter().map(convert_exec).collect(),
        working_directory: svc.working_directory.clone(),
        user: svc.user.clone(),
        group: svc.group.clone(),
        pam_name: svc.pam_name.clone(),
        environment: svc.environment.clone(),
        environment_file: svc.environment_file.clone(),
        restart_policy: convert_restart(&svc.restart),
        restart_sec: svc.restart_sec,
        timeout_start_sec: svc.timeout_start_sec,
        timeout_stop_sec: svc.timeout_stop_sec,
        remain_after_exit: svc.remain_after_exit,
        watchdog_sec: svc.watchdog_sec,
        kill_signal: svc.kill_signal.clone(),
        kill_mode: svc.kill_mode.clone(),
        standard_input: svc.standard_input.clone(),
        standard_output: svc.standard_output.clone(),
        standard_error: svc.standard_error.clone(),
        tty_path: svc.tty_path.clone(),
    }
}

fn convert_automount(amt: &crate::types::AutomountSection) -> AutomountConfig {
    AutomountConfig {
        where_: amt.where_.clone(),
        extra_options: amt.extra_options.clone(),
        timeout_idle_sec: amt.timeout_idle_sec,
        directory_mode: amt.directory_mode.clone(),
    }
}

fn convert_mount(mnt: &crate::types::MountSection) -> MountConfig {
    MountConfig {
        what: mnt.what.clone(),
        where_: mnt.where_.clone(),
        type_: mnt.type_.clone(),
        options: mnt.options.clone(),
        timeout_sec: mnt.timeout_sec,
    }
}

fn convert_timer(tmr: &crate::types::TimerSection) -> TimerConfig {
    TimerConfig {
        on_active_sec: tmr.on_active_sec,
        on_boot_sec: tmr.on_boot_sec,
        on_startup_sec: tmr.on_startup_sec,
        on_unit_active_sec: tmr.on_unit_active_sec,
        on_unit_inactive_sec: tmr.on_unit_inactive_sec,
        on_calendar: tmr.on_calendar.clone(),
        accuracy_sec: tmr.accuracy_sec,
        randomized_delay_sec: tmr.randomized_delay_sec,
        unit: tmr.unit.clone(),
        persistent: tmr.persistent,
    }
}

fn convert_socket(sock: &crate::types::SocketSection) -> SocketConfig {
    SocketConfig {
        listen_stream: sock.listen_stream.clone(),
        listen_datagram: sock.listen_datagram.clone(),
        listen_fifo: sock.listen_fifo.clone(),
        listen_netlink: sock.listen_netlink.clone(),
        accept: sock.accept,
        service: sock.service.clone(),
        socket_mode: sock.socket_mode.clone(),
        socket_user: sock.socket_user.clone(),
        socket_group: sock.socket_group.clone(),
        backlog: sock.backlog,
        directory_mode: sock.directory_mode.clone(),
    }
}

/// Build condition entries from UnitSection condition fields.
fn convert_conditions(uf: &UnitFile) -> Vec<Condition> {
    let mut conds = Vec::new();
    for v in &uf.unit.condition_path_exists {
        conds.push(Condition::from_value("PathExists", v));
    }
    for v in &uf.unit.condition_path_exists_glob {
        conds.push(Condition::from_value("PathExistsGlob", v));
    }
    for v in &uf.unit.condition_file_not_empty {
        conds.push(Condition::from_value("FileNotEmpty", v));
    }
    for v in &uf.unit.condition_directory_not_empty {
        conds.push(Condition::from_value("DirectoryNotEmpty", v));
    }
    for v in &uf.unit.condition_host {
        conds.push(Condition::from_value("Host", v));
    }
    for v in &uf.unit.condition_virtualization {
        conds.push(Condition::from_value("Virtualization", v));
    }
    for v in &uf.unit.condition_ac_power {
        conds.push(Condition::from_value("ACPower", v));
    }
    conds
}

/// Build condition entries from UnitSection assert fields.
fn convert_asserts(uf: &UnitFile) -> Vec<Condition> {
    let mut conds = Vec::new();
    for v in &uf.unit.assert_path_exists {
        conds.push(Condition::from_value("PathExists", v));
    }
    for v in &uf.unit.assert_file_not_empty {
        conds.push(Condition::from_value("FileNotEmpty", v));
    }
    for v in &uf.unit.assert_first_boot {
        conds.push(Condition::from_value("FirstBoot", v));
    }
    conds
}

impl SystemdFinder {
    pub fn new() -> Arc<Self> {
        Arc::new(SystemdFinder)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit_file(name: &str, aliases: &[&str]) -> UnitFile {
        let mut uf = UnitFile::new(name);
        uf.unit.description = format!("desc of {name}");
        uf.install.alias = aliases.iter().map(|s| s.to_string()).collect();
        uf
    }

    #[test]
    fn convert_unit_file_carries_aliases() {
        let uf = unit_file("lightdm.service", &["display-manager.service"]);
        let ir = convert_unit_file(&uf);
        assert_eq!(ir.id, "lightdm.service");
        assert_eq!(ir.aliases, vec!["display-manager.service"]);
    }

    #[test]
    fn convert_unit_file_no_aliases_is_empty() {
        let uf = unit_file("sshd.service", &[]);
        let ir = convert_unit_file(&uf);
        assert!(ir.aliases.is_empty());
    }

    #[test]
    fn find_all_merges_duplicate_canonical_names_and_their_aliases() {
        let mut map = HashMap::new();
        merge_unit_ir(
            &mut map,
            &unit_file("lightdm.service", &["display-manager.service"]),
        );
        merge_unit_ir(
            &mut map,
            &unit_file("lightdm.service", &["lightdm-extra.service"]),
        );
        // The first definition is kept; aliases are the union.
        assert_eq!(map.len(), 1);
        let merged = map.get("lightdm.service").unwrap();
        assert_eq!(merged.aliases.len(), 2);
        assert!(merged
            .aliases
            .contains(&"display-manager.service".to_string()));
        assert!(merged
            .aliases
            .contains(&"lightdm-extra.service".to_string()));
    }
}
