use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use systema_sysf::ir::{
    self, AutomountConfig, Condition, DependencySet, ExecCommand, MountConfig, PathConfig,
    ServiceConfig, SocketConfig, TemplateInfo, TimerConfig, UnitIR, UnitType,
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
        for file in &files {
            merge_unit_ir(&mut map, file);
        }
        // Alias ownership is decided here, where the filesystem evidence is:
        // emit one alias record per name, so System A never sees two units
        // competing for the same name.
        emit_alias_records(&mut map, &files);
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
/// (highest-precedence) definition, exactly like systemd's `unit_file_find`;
/// the lower-priority copies are shadowed.  Aliases are not merged here:
/// [`emit_alias_records`] resolves them globally, because one name can be
/// claimed by several different units.
fn merge_unit_ir(map: &mut HashMap<String, UnitIR>, file: &UnitFile) {
    map.entry(file.name.clone())
        .or_insert_with(|| convert_unit_file(file));
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
        path: uf.path.as_ref().map(convert_path),
        conditions: Some(convert_conditions(uf)),
        asserts: Some(convert_asserts(uf)),
        wanted_by: Some(uf.install.wanted_by.iter().cloned().collect()),
        required_by: Some(uf.install.required_by.iter().cloned().collect()),
        // Aliases travel as `alias_of` records (see `emit_alias_records`): the
        // finder is the component that knows whether a name is backing a real
        // file and which search-path directory wins.
        aliases: Vec::new(),
        alias_of: None,
        resource_control: extract_resource_control(uf),
        template: convert_template_info(&uf.name),
    }
}

/// Describe a name as a template definition or an instance of one.
///
/// The naming convention carries the template relation (`getty@tty3.service`
/// is an instance of `getty@.service`), so the finder can report it without a
/// separate index.  A template's values keep `%i`/`%I` unexpanded (the parser
/// preserves them for template names) because only the consumer knows which
/// instance it will materialize.
fn convert_template_info(name: &str) -> Option<TemplateInfo> {
    if sysa::unit_name::is_template(name) {
        return Some(TemplateInfo {
            definition: true,
            of: None,
            parameters: Vec::new(),
        });
    }
    let template = sysa::unit_name::template_of(name)?;
    Some(TemplateInfo {
        definition: false,
        of: Some(template),
        parameters: sysa::unit_name::instance_of(name).into_iter().collect(),
    })
}

/// Resolve every alias name reported by the discovered unit files to exactly
/// one owner, and insert one alias record per name.
///
/// The evidence is the filesystem: a name materialised as a unit-file symlink
/// (`UnitFile::alias_links`) beats an `[Install] Alias=` declaration, and
/// among equals the first discovered file wins — the loader scans the search
/// paths in priority order.  Only the first file found under a given name
/// counts: a lower-priority duplicate of that name is shadowed, so its alias
/// declarations are not part of the loaded unit.
///
/// Deduplication is by *file* name, not by canonical name: several names may
/// point at one unit (`ssh.service` and `sshd.service`), and each of them is
/// a valid alias.  The loader records a symlink file's own basename as a link
/// alias, so the first entry of `alias_links` identifies the file.
///
/// A name that is itself a loaded unit keeps owning itself: no record is
/// emitted, and the real unit wins over any alias claim.
fn emit_alias_records(map: &mut HashMap<String, UnitIR>, files: &[UnitFile]) {
    // alias -> (rank, discovery index, canonical unit).  Rank 0 is a
    // materialised link, rank 1 a bare `[Install] Alias=` declaration.
    let mut claims: HashMap<&str, (u8, usize, &str)> = HashMap::new();
    let mut seen_files: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (index, file) in files.iter().enumerate() {
        // A template's `[Install] Alias=` names another template; those are
        // template aliases, not names for a unit, and are not resolved here.
        if sysa::unit_name::is_template(&file.name) {
            continue;
        }
        let file_name = file
            .alias_links
            .first()
            .map(String::as_str)
            .unwrap_or(file.name.as_str());
        if !seen_files.insert(file_name) {
            continue; // shadowed duplicate at a lower-priority search path
        }
        for alias in &file.install.alias {
            let claim = (
                u8::from(!file.alias_links.contains(alias)),
                index,
                file.name.as_str(),
            );
            match claims.get(alias.as_str()) {
                Some(&current) if (current.0, current.1) <= (claim.0, claim.1) => {}
                _ => {
                    claims.insert(alias.as_str(), claim);
                }
            }
        }
    }

    for (alias, (_, _, canonical)) in claims {
        if map.contains_key(alias) {
            continue; // a real unit owns the name; it is not an alias
        }
        map.insert(alias.to_string(), alias_record(alias, canonical));
    }
}

/// Build the alias record for `alias` → `canonical`.
///
/// It carries no definition: the alias shares the target's jobs, state and
/// object identity, so System A only records the name mapping.
fn alias_record(alias: &str, canonical: &str) -> UnitIR {
    UnitIR {
        id: alias.to_string(),
        unit_type: None,
        description: None,
        source_format: Some("systemd".to_string()),
        source_path: None,
        slice: None,
        dependencies: None,
        service: None,
        mount: None,
        automount: None,
        timer: None,
        socket: None,
        path: None,
        conditions: None,
        asserts: None,
        wanted_by: None,
        required_by: None,
        aliases: Vec::new(),
        alias_of: Some(canonical.to_string()),
        resource_control: None,
        template: None,
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
        delegate: rc.delegate,
        delegate_controllers: rc.delegate_controllers.clone(),
        delegate_subgroup: rc.delegate_subgroup.clone(),
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
        service_type: svc.service_type.as_str().to_string(),
        bus_name: svc.bus_name.clone(),
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
        state_directory: svc.state_directory.clone(),
        state_directory_mode: svc.state_directory_mode,
        logs_directory: svc.logs_directory.clone(),
        logs_directory_mode: svc.logs_directory_mode,
        runtime_directory: svc.runtime_directory.clone(),
        runtime_directory_mode: svc.runtime_directory_mode,
        configuration_directory: svc.configuration_directory.clone(),
        configuration_directory_mode: svc.configuration_directory_mode,
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
        listen_sequential_packet: sock.listen_sequential_packet.clone(),
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

fn convert_path(path: &crate::types::PathSection) -> PathConfig {
    PathConfig {
        path_exists: path.path_exists.clone(),
        path_exists_glob: path.path_exists_glob.clone(),
        path_changed: path.path_changed.clone(),
        path_modified: path.path_modified.clone(),
        directory_not_empty: path.directory_not_empty.clone(),
        unit: path.unit.clone(),
        make_directory: path.make_directory,
        directory_mode: path.directory_mode.clone(),
        trigger_limit_interval_sec: path.trigger_limit_interval_sec,
        trigger_limit_burst: path.trigger_limit_burst,
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

    /// A unit whose aliases are `[Install] Alias=` declarations only.
    fn unit_file(name: &str, aliases: &[&str]) -> UnitFile {
        let mut uf = UnitFile::new(name);
        uf.unit.description = format!("desc of {name}");
        uf.install.alias = aliases.iter().map(|s| s.to_string()).collect();
        uf
    }

    /// A unit whose aliases exist as unit-file symlinks on disk.
    fn linked_unit_file(name: &str, aliases: &[&str]) -> UnitFile {
        let mut uf = unit_file(name, aliases);
        uf.alias_links = aliases.iter().map(|s| s.to_string()).collect();
        uf
    }

    fn build(files: &[UnitFile]) -> HashMap<String, UnitIR> {
        let mut map = HashMap::new();
        for file in files {
            merge_unit_ir(&mut map, file);
        }
        emit_alias_records(&mut map, files);
        map
    }

    #[test]
    fn convert_unit_file_describes_templates_and_instances() {
        let tpl = convert_unit_file(&unit_file("getty@.service", &[]));
        let info = tpl.template.as_ref().expect("template metadata");
        assert!(tpl.is_template());
        assert!(info.definition);
        assert!(info.of.is_none());
        assert!(info.parameters.is_empty());

        let instance = convert_unit_file(&unit_file("getty@tty3.service", &[]));
        assert!(!instance.is_template());
        assert!(instance.is_template_instance());
        assert_eq!(instance.template_of(), Some("getty@.service"));
        assert_eq!(instance.template_parameters(), ["tty3"]);

        assert!(convert_unit_file(&unit_file("plain.service", &[]))
            .template
            .is_none());
    }

    #[test]
    fn convert_unit_file_reports_no_aliases_of_its_own() {
        let uf = linked_unit_file("lightdm.service", &["display-manager.service"]);
        let ir = convert_unit_file(&uf);
        assert_eq!(ir.id, "lightdm.service");
        assert!(ir.aliases.is_empty());
        assert!(ir.alias_of.is_none());
    }

    #[test]
    fn convert_unit_file_carries_the_service_type() {
        // Type= decides when a start job completes, so it must survive the
        // conversion into the IR instead of being re-inferred downstream.
        let mut uf = unit_file("user@.service", &[]);
        uf.service = Some(crate::types::ServiceSection {
            service_type: crate::types::ServiceType::NotifyReload,
            ..Default::default()
        });
        let ir = convert_unit_file(&uf);
        assert_eq!(
            ir.service.as_ref().expect("service config").service_type,
            "notify-reload"
        );
    }

    #[test]
    fn convert_unit_file_carries_dbus_bus_name() {
        let mut uf = unit_file("deepin-authenticate.service", &[]);
        uf.service = Some(crate::types::ServiceSection {
            service_type: crate::types::ServiceType::Dbus,
            bus_name: "org.deepin.dde.Authenticate1".to_string(),
            ..Default::default()
        });

        let ir = convert_unit_file(&uf);
        let service = ir.service.expect("service config");
        assert_eq!(service.service_type, "dbus");
        assert_eq!(service.bus_name, "org.deepin.dde.Authenticate1");
    }

    #[test]
    fn path_and_socket_config_survive_finder_serialization() {
        let path = crate::parser::parse_unit(
            "acpid.path",
            "[Path]\nPathExists=/run/acpid.socket\nPathChanged=/var/run/test\nUnit=acpid.service\nMakeDirectory=yes\n",
        )
        .unwrap();
        let socket = crate::parser::parse_unit(
            "systemd-udevd-control.socket",
            "[Socket]\nListenSequentialPacket=@/org/kernel/udev/udevd\nListenNetlink=kobject-uevent 1\n",
        )
        .unwrap();
        for unit in [path, socket] {
            let json = serde_json::to_vec(&convert_unit_file(&unit)).unwrap();
            let ir: UnitIR = serde_json::from_slice(&json).unwrap();
            if let Some(path) = ir.path {
                assert_eq!(path.path_exists, ["/run/acpid.socket"]);
                assert_eq!(path.path_changed, ["/var/run/test"]);
                assert_eq!(path.unit, "acpid.service");
                assert!(path.make_directory);
            } else {
                let socket = ir.socket.unwrap();
                assert_eq!(socket.listen_sequential_packet, ["@/org/kernel/udev/udevd"]);
                assert_eq!(socket.listen_netlink, ["kobject-uevent 1"]);
            }
        }
    }

    #[test]
    fn alias_record_prefers_a_materialised_link_over_a_declaration() {
        let files = vec![
            linked_unit_file("lightdm.service", &["display-manager.service"]),
            unit_file("ddm.service", &["display-manager.service"]),
        ];
        let map = build(&files);

        let record = map
            .get("display-manager.service")
            .expect("alias record emitted");
        assert_eq!(record.alias_of.as_deref(), Some("lightdm.service"));
        assert!(record.unit_type.is_none());
        assert!(record.service.is_none());
        // The canonical units carry no competing alias list.
        assert!(map["lightdm.service"].aliases.is_empty());
        assert!(map["ddm.service"].aliases.is_empty());
    }

    #[test]
    fn alias_record_uses_discovery_order_between_two_links() {
        // Search paths are scanned in priority order and the loader pushes
        // files as it finds them, so the first link wins.
        let files = vec![
            linked_unit_file("lightdm.service", &["display-manager.service"]),
            linked_unit_file("ddm.service", &["display-manager.service"]),
        ];
        let map = build(&files);
        assert_eq!(
            map["display-manager.service"].alias_of.as_deref(),
            Some("lightdm.service")
        );
    }

    #[test]
    fn install_alias_declaration_alone_becomes_an_alias_record() {
        let files = vec![unit_file("lightdm.service", &["display-manager.service"])];
        let map = build(&files);
        assert_eq!(
            map["display-manager.service"].alias_of.as_deref(),
            Some("lightdm.service")
        );
    }

    #[test]
    fn alias_from_a_shadowed_duplicate_is_ignored() {
        // /etc overrides /usr/lib: the shadowed file's declarations are not
        // part of the loaded unit, so they must not claim a name.
        let files = vec![
            unit_file("lightdm.service", &[]),
            unit_file("lightdm.service", &["display-manager.service"]),
        ];
        let map = build(&files);
        assert_eq!(map.len(), 1);
        assert!(!map.contains_key("display-manager.service"));
    }

    #[test]
    fn every_link_to_one_unit_is_reported() {
        // Two different names point at the same unit; neither may be lost to
        // deduplication (they are not duplicates of the same file).
        let files = vec![
            linked_unit_file("lightdm.service", &["display-manager.service"]),
            linked_unit_file("lightdm.service", &["x-display-manager.service"]),
        ];
        let map = build(&files);
        assert_eq!(
            map["display-manager.service"].alias_of.as_deref(),
            Some("lightdm.service")
        );
        assert_eq!(
            map["x-display-manager.service"].alias_of.as_deref(),
            Some("lightdm.service")
        );
    }

    #[test]
    fn alias_record_never_shadows_a_real_unit() {
        let files = vec![
            linked_unit_file("lightdm.service", &["display-manager.service"]),
            unit_file("display-manager.service", &[]),
        ];
        let map = build(&files);
        let real = map.get("display-manager.service").unwrap();
        assert_eq!(real.id, "display-manager.service");
        assert!(real.alias_of.is_none());
    }
}
