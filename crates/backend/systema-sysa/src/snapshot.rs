//! Control-plane unit snapshots.
//!
//! Builds [`UnitSnapshot`] protobuf messages from a read lock on
//! [`AllocatorState`], giving the System Wrapper bridge flavors everything
//! they need to serve the `org.freedesktop.systemd1.Unit` (and per-type)
//! interfaces without reaching into the allocator.  Snapshot building is
//! pure and lock-confined: it takes no locks beyond the single read lock the
//! call site already holds.

use std::collections::HashMap;

use sysa::proto::{JobEvent, JobInfo, ListJobsResult, ResourceConfig, UnitSnapshot};

use crate::state::{AllocatorState, JobResultKind, JobStatus};
use crate::unit::types::ResourceControl;

/// Convert a parsed `ResourceControl` block into the protobuf projection.
fn rc_to_proto(rc: &ResourceControl) -> ResourceConfig {
    ResourceConfig {
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

/// The resource-control block of a unit, regardless of unit kind.
fn unit_rc(unit: &crate::unit::types::UnitFile) -> Option<&ResourceControl> {
    unit.service
        .as_ref()
        .map(|s| &s.rc)
        .or_else(|| unit.slice.as_ref().map(|s| &s.rc))
        .or_else(|| unit.scope.as_ref().map(|s| &s.rc))
}

/// `[Service]` projection consumed by the bridge's Service object.
fn service_snapshot(unit: &crate::unit::types::UnitFile, snap: &mut UnitSnapshot) {
    let Some(s) = unit.service.as_ref() else { return };
    snap.service_type = s.service_type.as_str().to_string();
    snap.restart = s.restart.as_str().to_string();
    snap.restart_sec = s.restart_sec as u64;
    snap.notify_access = s.notify_access.clone();
    snap.bus_name = s.bus_name.clone();
}

/// `[Scope]` projection consumed by the bridge's Scope object.
fn scope_snapshot(unit: &crate::unit::types::UnitFile, snap: &mut UnitSnapshot) {
    let Some(s) = unit.scope.as_ref() else { return };
    snap.scope_timeout_stop_sec = s.timeout_stop_sec as u64;
    snap.scope_runtime_max_sec = s.runtime_max_sec as u64;
}

/// `[Mount]` projection consumed by the bridge's Mount object.
fn mount_snapshot(unit: &crate::unit::types::UnitFile, snap: &mut UnitSnapshot) {
    let Some(m) = unit.mount.as_ref() else { return };
    snap.mount_where = m.where_.clone();
    snap.mount_what = m.what.clone();
    snap.mount_options = m.options.clone();
    snap.mount_timeout_sec = m.timeout_sec as u64;
}

/// The `ExecReload=` command line, or empty when unset/not a service unit.
fn exec_reload_line(unit: &crate::unit::types::UnitFile) -> String {
    unit.service
        .as_ref()
        .and_then(|s| s.exec_reload.first())
        .map(|c| c.raw.clone())
        .unwrap_or_default()
}

/// systemd's `UnitFileState`: transient overrides everything, then an
/// `[Install]`-level WantedBy link set marks the unit as enabled.
fn unit_file_state(unit: &crate::unit::types::UnitFile) -> String {
    if unit.transient {
        return "transient".to_string();
    }
    if unit.install.wanted_by.is_empty() {
        "static".to_string()
    } else {
        "enabled".to_string()
    }
}

/// Build a full snapshot of one unit.
///
/// When the unit is unknown, a minimal snapshot with `load_state =
/// "not-found"` is returned with `success = false` semantics left to the
/// caller (the snapshot itself is always produced so the bridge can
/// represent the unit as `not-found` + `inactive`, matching systemd).
pub fn unit_snapshot(state: &AllocatorState, name: &str) -> UnitSnapshot {
    let canonical = state.resolve_unit_name(name);
    let mut snap = UnitSnapshot {
        name: canonical.clone(),
        kind: String::new(),
        load_state: "not-found".to_string(),
        active_state: "inactive".to_string(),
        sub_state: String::new(),
        transient: false,
        description: String::new(),
        documentation: Vec::new(),
        requires: Vec::new(),
        wants: Vec::new(),
        after: Vec::new(),
        wanted_by: Vec::new(),
        exec_reload: String::new(),
        main_pid: 0,
        invocation_id: String::new(),
        active_enter_timestamp: 0,
        inactive_enter_timestamp: 0,
        extensions: HashMap::new(),
        pids: Vec::new(),
        controller: String::new(),
        running_job_id: 0,
        unit_file_state: "not-found".to_string(),
        resource: None,
        metrics: None,
        refs: Vec::new(),
        service_type: String::new(),
        restart: String::new(),
        restart_sec: 0,
        notify_access: String::new(),
        bus_name: String::new(),
        scope_timeout_stop_sec: 0,
        scope_runtime_max_sec: 0,
        mount_where: String::new(),
        mount_what: String::new(),
        mount_options: String::new(),
        mount_timeout_sec: 0,
    };

    let Some(unit) = state.units.get(&canonical) else {
        return snap;
    };

    snap.kind = unit.kind.worker_type().to_string();
    snap.load_state = "loaded".to_string();
    snap.transient = unit.transient;
    snap.unit_file_state = unit_file_state(unit);
    snap.description = unit.unit.description.clone();
    snap.documentation = unit.unit.documentation.clone();
    snap.requires = sorted(&unit.unit.requires);
    snap.wants = sorted(&unit.unit.wants);
    snap.after = sorted(&unit.unit.after);
    snap.wanted_by = sorted(&unit.install.wanted_by);
    snap.exec_reload = exec_reload_line(unit);
    snap.resource = unit_rc(unit).map(rc_to_proto);
    service_snapshot(unit, &mut snap);
    scope_snapshot(unit, &mut snap);
    mount_snapshot(unit, &mut snap);

    if let Some(cached) = state.unit_states.get(&canonical) {
        snap.active_state = cached.active_state.clone();
        snap.sub_state = cached.sub_state.clone();
        snap.main_pid = cached.main_pid;
        snap.invocation_id = cached.invocation_id.clone();
        snap.active_enter_timestamp = cached.active_enter_timestamp;
        snap.inactive_enter_timestamp = cached.inactive_enter_timestamp;
        snap.extensions = cached.extensions.clone();
        snap.pids = cached.pids.clone();
        snap.controller = cached.controller.clone();
    }

    if let Some(metrics) = state.cgroup_metrics.get(&canonical) {
        snap.metrics = Some(metrics.clone());
    }

    for (job_id, job) in &state.jobs {
        if job.unit_name == canonical && job.status == JobStatus::Running {
            snap.running_job_id = *job_id;
            break;
        }
    }

    snap.refs = state.get_refs(&canonical);
    snap
}

/// Sort a set into a deterministic vector (protobuf messages must be
/// stable to make bridge mirrors converge).
fn sorted(set: &std::collections::HashSet<String>) -> Vec<String> {
    let mut v: Vec<String> = set.iter().cloned().collect();
    v.sort();
    v
}

/// Build the `manager.list_snapshots` reply payload.
pub fn list_snapshots(state: &AllocatorState) -> Vec<UnitSnapshot> {
    let mut units: Vec<String> = state.units.keys().cloned().collect();
    units.sort();
    units.into_iter().map(|n| unit_snapshot(state, &n)).collect()
}

/// Build the `manager.list_jobs` reply payload (running jobs only, like
/// systemd's `ListJobs`).
pub fn list_jobs(state: &AllocatorState) -> Vec<JobInfo> {
    let mut jobs: Vec<JobInfo> = state
        .jobs
        .iter()
        .filter(|(_, job)| job.status == JobStatus::Running)
        .map(|(job_id, job)| JobInfo {
            job_id: *job_id,
            unit_name: job.unit_name.clone(),
            job_type: job.kind.as_str().to_string(),
            status: "running".to_string(),
        })
        .collect();
    jobs.sort_by_key(|j| j.job_id);
    jobs
}

/// Build the `job.completed` event payload.  `result` is one of
/// `JobResultKind::as_str()` values ("done", "failed", "cancelled", ...).
pub fn job_completion_event(job_id: u64, unit_name: &str, result: &JobResultKind) -> JobEvent {
    JobEvent {
        job_id,
        unit_name: unit_name.to_string(),
        result: result.as_str().to_string(),
    }
}

/// Convenience: build a `ListJobsResult` under a read lock.
pub fn list_jobs_result(state: &AllocatorState) -> ListJobsResult {
    ListJobsResult {
        success: true,
        message: String::new(),
        jobs: list_jobs(state),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::CachedUnitState;
    use crate::unit::types::{
        ExecCommand, RestartPolicy, ServiceSection, ServiceType, UnitFile, UnitKind,
    };
    use std::collections::{HashMap, HashSet};

    fn service_unit() -> UnitFile {
        let mut unit = UnitFile::new("sshd.service");
        unit.kind = UnitKind::Service;
        unit.unit.description = "OpenBSD Secure Shell server".to_string();
        unit.unit.documentation = vec!["man:sshd(8)".to_string()];
        unit.unit.requires.insert("sshd-keygen.service".to_string());
        unit.unit.wants.insert("syslog.service".to_string());
        unit.unit.after.insert("network.target".to_string());
        unit.install.wanted_by = HashSet::from(["sshd.socket".to_string()]);
        unit.service = Some(ServiceSection {
            service_type: ServiceType::Notify,
            exec_reload: vec![ExecCommand {
                raw: "/bin/kill -HUP $MAINPID".to_string(),
                program: "/bin/kill".to_string(),
                args: vec!["-HUP".to_string(), "$MAINPID".to_string()],
                ignore_failure: false,
                privileged: false,
                no_env_lookup: false,
                no_kill_on_stop: false,
                no_new_privileges: false,
            }],
            restart: RestartPolicy::Always,
            restart_sec: 100,
            notify_access: "main".to_string(),
            bus_name: "org.openssh.sshd".to_string(),
            rc: ResourceControl {
                cpu_quota: "50%".to_string(),
                memory_max: "1G".to_string(),
                tasks_max: 512,
                ..Default::default()
            },
            ..Default::default()
        });
        unit
    }

    fn cached_state() -> CachedUnitState {
        CachedUnitState {
            active_state: "active".to_string(),
            sub_state: "running".to_string(),
            main_pid: 12345,
            invocation_id: "0123456789abcdef0123456789abcdef".to_string(),
            active_enter_timestamp: 1_700_000_000_000_000,
            inactive_enter_timestamp: 0,
            extensions: HashMap::from([("last_exit_code".to_string(), "0".to_string())]),
            pids: Vec::new(),
            controller: String::new(),
        }
    }

    #[test]
    fn service_snapshot_maps_config_runtime_and_deps() {
        let mut state = AllocatorState::new();
        let unit = service_unit();
        state.units.insert(unit.name.clone(), unit);
        state
            .unit_states
            .insert("sshd.service".to_string(), cached_state());

        let snap = unit_snapshot(&state, "sshd.service");

        assert_eq!(snap.name, "sshd.service");
        assert_eq!(snap.kind, "service");
        assert_eq!(snap.load_state, "loaded");
        assert_eq!(snap.description, "OpenBSD Secure Shell server");
        assert_eq!(snap.documentation, vec!["man:sshd(8)".to_string()]);
        assert_eq!(snap.requires, vec!["sshd-keygen.service".to_string()]);
        assert_eq!(snap.wants, vec!["syslog.service".to_string()]);
        assert_eq!(snap.after, vec!["network.target".to_string()]);
        assert_eq!(snap.wanted_by, vec!["sshd.socket".to_string()]);

        // Service projection.
        assert_eq!(snap.service_type, "notify");
        assert_eq!(snap.restart, "always");
        assert_eq!(snap.restart_sec, 100);
        assert_eq!(snap.notify_access, "main");
        assert_eq!(snap.bus_name, "org.openssh.sshd");
        assert_eq!(snap.exec_reload, "/bin/kill -HUP $MAINPID");

        // Runtime projection from CachedUnitState.
        assert_eq!(snap.active_state, "active");
        assert_eq!(snap.sub_state, "running");
        assert_eq!(snap.main_pid, 12345);
        assert_eq!(
            snap.invocation_id,
            "0123456789abcdef0123456789abcdef"
        );
        assert_eq!(snap.active_enter_timestamp, 1_700_000_000_000_000);
        assert_eq!(
            snap.extensions.get("last_exit_code").map(String::as_str),
            Some("0")
        );

        // Resource projection.
        let resource = snap.resource.expect("resource block present");
        assert_eq!(resource.cpu_quota, "50%");
        assert_eq!(resource.memory_max, "1G");
        assert_eq!(resource.tasks_max, 512);
    }

    #[test]
    fn snapshot_deps_are_sorted_for_mirror_convergence() {
        let mut state = AllocatorState::new();
        let mut unit = service_unit();
        // Deliberately insert in reverse order.
        unit.unit.after = HashSet::from([
            "z.target".to_string(),
            "a.target".to_string(),
            "m.target".to_string(),
        ]);
        state.units.insert(unit.name.clone(), unit);

        let snap = unit_snapshot(&state, "sshd.service");
        assert_eq!(snap.after, vec!["a.target", "m.target", "z.target"]);
    }

    #[test]
    fn mount_snapshot_maps_mount_section() {
        use crate::unit::types::MountSection;

        let mut state = AllocatorState::new();
        let mut unit = UnitFile::new("boot.mount");
        unit.kind = UnitKind::Mount;
        unit.mount = Some(MountSection {
            what: "/dev/sda1".to_string(),
            where_: "/boot".to_string(),
            type_: "ext4".to_string(),
            options: "rw,relatime".to_string(),
            timeout_sec: 90,
            ..Default::default()
        });
        state.units.insert(unit.name.clone(), unit);

        let snap = unit_snapshot(&state, "boot.mount");
        assert_eq!(snap.kind, "mount");
        assert_eq!(snap.load_state, "loaded");
        assert_eq!(snap.mount_where, "/boot");
        assert_eq!(snap.mount_what, "/dev/sda1");
        assert_eq!(snap.mount_options, "rw,relatime");
        assert_eq!(snap.mount_timeout_sec, 90);
    }

    #[test]
    fn scope_snapshot_maps_scope_section() {
        use crate::unit::types::ScopeSection;

        let mut state = AllocatorState::new();
        let mut unit = UnitFile::new("session-1.scope");
        unit.kind = UnitKind::Scope;
        unit.scope = Some(ScopeSection {
            timeout_stop_sec: 30,
            runtime_max_sec: 3600,
            ..Default::default()
        });
        state.units.insert(unit.name.clone(), unit);

        let snap = unit_snapshot(&state, "session-1.scope");
        assert_eq!(snap.kind, "scope");
        assert_eq!(snap.scope_timeout_stop_sec, 30);
        assert_eq!(snap.scope_runtime_max_sec, 3600);
    }

    #[test]
    fn unknown_unit_returns_not_found_inactive() {
        let state = AllocatorState::new();
        let snap = unit_snapshot(&state, "missing.service");
        assert_eq!(snap.name, "missing.service");
        assert_eq!(snap.load_state, "not-found");
        assert_eq!(snap.active_state, "inactive");
        assert_eq!(snap.unit_file_state, "not-found");
    }

    #[test]
    fn snapshot_resolves_alias_to_canonical_name() {
        let mut state = AllocatorState::new();
        let mut unit = service_unit();
        unit.install.alias = vec!["ssh.service".to_string()];
        state.units.insert(unit.name.clone(), unit);
        state.rebuild_alias_map();

        let snap = unit_snapshot(&state, "ssh.service");
        assert_eq!(snap.name, "sshd.service");
    }

    #[test]
    fn snapshot_reports_running_job_id() {
        let mut state = AllocatorState::new();
        let unit = service_unit();
        state.units.insert(unit.name.clone(), unit);
        state.jobs.insert(
            7,
            crate::state::Job {
                id: 7,
                unit_name: "sshd.service".to_string(),
                kind: crate::state::JobKind::Start,
                status: crate::state::JobStatus::Running,
                timeout_abort: None,
            },
        );

        let snap = unit_snapshot(&state, "sshd.service");
        assert_eq!(snap.running_job_id, 7);
    }

    #[test]
    fn unit_file_state_reflects_transient_and_enabled() {
        let mut state = AllocatorState::new();

        // Transient overrides everything.
        let mut transient = service_unit();
        transient.transient = true;
        transient.install.wanted_by = HashSet::from(["multi-user.target".to_string()]);
        state.units.insert(transient.name.clone(), transient);
        assert_eq!(
            unit_snapshot(&state, "sshd.service").unit_file_state,
            "transient"
        );

        // Enabled = has install-level WantedBy links.
        state.units.clear();
        let mut enabled = service_unit();
        enabled.install.wanted_by = HashSet::from(["multi-user.target".to_string()]);
        state.units.insert(enabled.name.clone(), enabled);
        assert_eq!(
            unit_snapshot(&state, "sshd.service").unit_file_state,
            "enabled"
        );

        // Static = no WantedBy links.
        state.units.clear();
        let mut sibling = service_unit();
        sibling.install.wanted_by = HashSet::new();
        state.units.insert(sibling.name.clone(), sibling);
        assert_eq!(
            unit_snapshot(&state, "sshd.service").unit_file_state,
            "static"
        );
    }
}