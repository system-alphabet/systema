//! Per-unit D-Bus objects (bridge).
//!
//! Serves `org.freedesktop.systemd1.Unit` for every mirrored unit, reading
//! everything from the control-port mirror snapshot (never the allocator).

use std::sync::Arc;

use zbus::interface;
use zvariant::OwnedObjectPath;

use super::manager::job_object_path;
use super::BridgeContext;

/// D-Bus object representing a single loaded unit.
pub struct UnitObject {
    pub ctx: Arc<BridgeContext>,
    pub unit_name: String,
}

#[interface(name = "org.freedesktop.systemd1.Unit")]
impl UnitObject {
    #[zbus(property)]
    fn id(&self) -> String {
        self.unit_name.clone()
    }

    #[zbus(property)]
    fn names(&self) -> Vec<String> {
        vec![self.unit_name.clone()]
    }

    #[zbus(property)]
    fn description(&self) -> String {
        self.snapshot().description
    }

    #[zbus(property)]
    fn documentation(&self) -> Vec<String> {
        self.snapshot().documentation
    }

    #[zbus(property)]
    fn load_state(&self) -> String {
        self.snapshot().load_state
    }

    #[zbus(property)]
    fn active_state(&self) -> String {
        self.snapshot().active_state
    }

    #[zbus(property)]
    fn sub_state(&self) -> String {
        if self.snapshot().sub_state.is_empty() {
            "dead".to_string()
        } else {
            self.snapshot().sub_state
        }
    }

    #[zbus(property)]
    fn following(&self) -> String {
        String::new()
    }

    #[zbus(property)]
    fn unit_file_state(&self) -> String {
        self.snapshot().unit_file_state
    }

    #[zbus(property)]
    fn unit_file_preset(&self) -> String {
        "disabled".to_string()
    }

    #[zbus(property)]
    fn fragment_path(&self) -> String {
        for dir in sysa::paths::instance().unit_search_paths.iter() {
            let path = std::path::Path::new(dir).join(&self.unit_name);
            if path.exists() {
                return path.to_string_lossy().into_owned();
            }
        }
        String::new()
    }

    #[zbus(property)]
    fn source_path(&self) -> String {
        String::new()
    }

    #[zbus(property)]
    fn job(&self) -> (u32, OwnedObjectPath) {
        let job = self
            .ctx
            .mirror
            .read()
            .running_job_for(&self.unit_name)
            .map(|j| (j.job_id, j.job_type.clone()));
        match job {
            Some((id, _)) => (id as u32, job_object_path(id)),
            None => (0, OwnedObjectPath::try_from("/").unwrap()),
        }
    }

    #[zbus(property)]
    fn can_start(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_stop(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_reload(&self) -> bool {
        !self.snapshot().exec_reload.is_empty()
    }

    #[zbus(property)]
    fn can_isolate(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn can_freeze(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn requires(&self) -> Vec<String> {
        self.snapshot().requires
    }

    #[zbus(property)]
    fn wants(&self) -> Vec<String> {
        self.snapshot().wants
    }

    #[zbus(property)]
    fn after(&self) -> Vec<String> {
        self.snapshot().after
    }

    #[zbus(property)]
    fn before(&self) -> Vec<String> {
        Vec::new()
    }

    #[zbus(property)]
    fn triggers(&self) -> Vec<OwnedObjectPath> {
        Vec::new()
    }

    #[zbus(property)]
    fn triggered_by(&self) -> Vec<OwnedObjectPath> {
        Vec::new()
    }

    #[zbus(property)]
    fn requires_mounts_for(&self) -> Vec<String> {
        Vec::new()
    }

    #[zbus(property)]
    fn propagates_reload_to(&self) -> Vec<String> {
        Vec::new()
    }

    #[zbus(property)]
    fn reload_propagated_from(&self) -> Vec<String> {
        Vec::new()
    }

    #[zbus(property)]
    fn transient(&self) -> bool {
        self.snapshot().transient
    }

    #[zbus(property)]
    fn perpetual(&self) -> bool {
        self.unit_name == crate::mirror::ROOT_SLICE_NAME
    }

    #[zbus(property)]
    fn need_daemon_reload(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn job_timeout_u_sec(&self) -> u64 {
        u64::MAX
    }

    #[zbus(property)]
    fn job_running_timeout_u_sec(&self) -> u64 {
        u64::MAX
    }

    #[zbus(property)]
    fn condition_result(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn assert_result(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn activation_details(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    #[zbus(property)]
    fn refs(&self) -> Vec<String> {
        self.snapshot().refs
    }

    #[zbus(property)]
    fn active_enter_timestamp(&self) -> u64 {
        self.snapshot().active_enter_timestamp
    }

    #[zbus(property)]
    fn inactive_enter_timestamp(&self) -> u64 {
        self.snapshot().inactive_enter_timestamp
    }

    #[zbus(property)]
    fn invocation_id(&self) -> Vec<u8> {
        let id = self.snapshot().invocation_id;
        uuid::Uuid::parse_str(&id)
            .map(|u| u.as_bytes().to_vec())
            .unwrap_or_else(|_| vec![0u8; 16])
    }

    // ------------------------------------------------------------------
    // Resource control (served from the snapshot's ResourceConfig)
    // ------------------------------------------------------------------

    #[zbus(property, name = "MemoryMin")]
    fn memory_min(&self) -> u64 {
        self.resource().map(|rc| parse_size_bytes(&rc.memory_min)).unwrap_or(0)
    }

    #[zbus(property, name = "MemoryLow")]
    fn memory_low(&self) -> u64 {
        self.resource().map(|rc| parse_size_bytes(&rc.memory_low)).unwrap_or(0)
    }

    #[zbus(property, name = "MemoryHigh")]
    fn memory_high(&self) -> u64 {
        self.resource().map(|rc| parse_size_bytes(&rc.memory_high)).unwrap_or(0)
    }

    #[zbus(property, name = "MemoryMax")]
    fn memory_max(&self) -> u64 {
        self.resource().map(|rc| parse_size_bytes(&rc.memory_max)).unwrap_or(0)
    }

    #[zbus(property, name = "MemorySwapMax")]
    fn memory_swap_max(&self) -> u64 {
        self.resource().map(|rc| parse_size_bytes(&rc.memory_swap_max)).unwrap_or(0)
    }

    #[zbus(property, name = "CPUQuotaUSec")]
    fn cpu_quota_u_sec(&self) -> u64 {
        self.resource().map(|rc| parse_cpu_quota_usec(&rc.cpu_quota)).unwrap_or(0)
    }

    #[zbus(property, name = "CPUQuotaPeriodUSec")]
    fn cpu_quota_period_u_sec(&self) -> u64 {
        self.resource()
            .map(|rc| parse_usec_value(&rc.cpu_quota_period, 100_000))
            .unwrap_or(0)
    }

    #[zbus(property, name = "CPUWeight")]
    fn cpu_weight(&self) -> u64 {
        self.resource().map(|rc| rc.cpu_weight as u64).unwrap_or(100)
    }

    #[zbus(property, name = "StartupCPUWeight")]
    fn startup_cpu_weight(&self) -> u64 {
        self.resource().map(|rc| rc.startup_cpu_weight as u64).unwrap_or(100)
    }

    #[zbus(property, name = "IOWeight")]
    fn io_weight(&self) -> u64 {
        self.resource().map(|rc| rc.io_weight as u64).unwrap_or(100)
    }

    #[zbus(property, name = "StartupIOWeight")]
    fn startup_io_weight(&self) -> u64 {
        self.resource().map(|rc| rc.startup_io_weight as u64).unwrap_or(100)
    }

    #[zbus(property, name = "TasksMax")]
    fn tasks_max(&self) -> u64 {
        self.resource().map(|rc| rc.tasks_max as u64).unwrap_or(u64::MAX)
    }

    #[zbus(property, name = "AllowedCPUs")]
    fn allowed_cpus(&self) -> String {
        self.resource().map(|rc| rc.allowed_cpus.clone()).unwrap_or_default()
    }

    #[zbus(property, name = "AllowedMemoryNodes")]
    fn allowed_memory_nodes(&self) -> String {
        self.resource().map(|rc| rc.allowed_memory_nodes.clone()).unwrap_or_default()
    }

    #[zbus(property, name = "CPUSetCPUs")]
    fn cpu_set_cpus(&self) -> String {
        self.resource().map(|rc| rc.cpu_set_cpus.clone()).unwrap_or_default()
    }

    #[zbus(property, name = "CPUSetMemoryNodes")]
    fn cpu_set_memory_nodes(&self) -> String {
        self.resource().map(|rc| rc.cpu_set_memory_nodes.clone()).unwrap_or_default()
    }

    // ------------------------------------------------------------------
    // Runtime cgroup metrics (served from the snapshot's metrics)
    // ------------------------------------------------------------------

    #[zbus(property, name = "ControlGroup")]
    fn control_group(&self) -> String {
        self.metrics().control_group
    }

    #[zbus(property, name = "ControlGroupId")]
    fn control_group_id(&self) -> u64 {
        self.metrics().control_group_id
    }

    #[zbus(property, name = "MemoryCurrent")]
    fn memory_current(&self) -> u64 {
        self.metric("MemoryCurrent")
    }

    #[zbus(property, name = "MemoryPeak")]
    fn memory_peak(&self) -> u64 {
        self.metric("MemoryPeak")
    }

    #[zbus(property, name = "MemorySwapCurrent")]
    fn memory_swap_current(&self) -> u64 {
        self.metric("MemorySwapCurrent")
    }

    #[zbus(property, name = "CPUUsageNSec")]
    fn cpu_usage_n_sec(&self) -> u64 {
        self.metric("CPUUsageNSec")
    }

    #[zbus(property, name = "TasksCurrent")]
    fn tasks_current(&self) -> u64 {
        self.metric("TasksCurrent")
    }

    #[zbus(property, name = "OOMKills")]
    fn oom_kills(&self) -> u64 {
        self.metric("OOMKills")
    }

    #[zbus(property, name = "IOReadBytes")]
    fn io_read_bytes(&self) -> u64 {
        self.metric("IOReadBytes")
    }

    #[zbus(property, name = "IOReadOperations")]
    fn io_read_operations(&self) -> u64 {
        self.metric("IOReadOperations")
    }

    #[zbus(property, name = "IOWriteBytes")]
    fn io_write_bytes(&self) -> u64 {
        self.metric("IOWriteBytes")
    }

    #[zbus(property, name = "IOWriteOperations")]
    fn io_write_operations(&self) -> u64 {
        self.metric("IOWriteOperations")
    }

    #[zbus(property, name = "EffectiveTasksMax")]
    fn effective_tasks_max(&self) -> u64 {
        self.metric("EffectiveTasksMax")
    }

    #[zbus(property, name = "EffectiveMemoryMax")]
    fn effective_memory_max(&self) -> u64 {
        self.metric("EffectiveMemoryMax")
    }

    /// List the processes running directly inside the unit's cgroup.
    fn get_processes(&self) -> Vec<(String, u32, String)> {
        self.metrics()
            .processes
            .into_iter()
            .map(|p| (p.subpath, p.pid, p.name))
            .collect()
    }

    fn get_triggering_units(&self) -> Vec<OwnedObjectPath> {
        Vec::new()
    }

    /// Reset the unit's failed state via the control plane.
    async fn reset_failed(&self) -> zbus::fdo::Result<()> {
        let req = sysa::proto::ResetFailedUnitRequest {
            name: self.unit_name.clone(),
        };
        let reply: sysa::proto::SimpleManagerResult = self
            .ctx
            .client
            .call("manager.reset_failed_unit", &req)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        if reply.success {
            Ok(())
        } else {
            Err(zbus::fdo::Error::Failed(reply.message))
        }
    }
}

/// Internal helpers, not exposed on D-Bus.
impl UnitObject {
    fn snapshot(&self) -> sysa::proto::UnitSnapshot {
        self.ctx
            .mirror
            .read()
            .get(&self.unit_name)
            .cloned()
            .unwrap_or_default()
    }

    fn resource(&self) -> Option<sysa::proto::ResourceConfig> {
        let snap = self.snapshot();
        match snap.kind.as_str() {
            "service" | "slice" | "scope" => snap.resource,
            _ => None,
        }
    }

    fn metrics(&self) -> sysa::proto::UnitCgroupMetrics {
        self.snapshot().metrics.unwrap_or_default()
    }

    fn metric(&self, key: &str) -> u64 {
        self.metrics()
            .metrics
            .get(key)
            .copied()
            .unwrap_or(u64::MAX)
    }
}

/// Parse a systemd size value ("1G", "500M", "1024K", "infinity") into bytes.
fn parse_size_bytes(value: &str) -> u64 {
    let v = value.trim();
    if v.is_empty() || v.ends_with('%') {
        return 0;
    }
    if v.eq_ignore_ascii_case("infinity") {
        return u64::MAX;
    }
    let v = v.strip_suffix('B').unwrap_or(v);
    let (digits, suffix) = match v.chars().last() {
        Some(c) if c.is_ascii_alphabetic() => (&v[..v.len() - 1], c.to_ascii_uppercase()),
        _ => (v, '\0'),
    };
    let base: u64 = match digits.trim().parse() {
        Ok(n) => n,
        Err(_) => return 0,
    };
    let mult: u64 = match suffix {
        'K' => 1 << 10,
        'M' => 1 << 20,
        'G' => 1 << 30,
        'T' => 1 << 40,
        'P' => 1 << 50,
        'E' => 1 << 60,
        _ => 1,
    };
    base.saturating_mul(mult)
}

/// Parse a `CPUQuota=` value into µs per period (100 ms default).
fn parse_cpu_quota_usec(value: &str) -> u64 {
    let v = value.trim();
    if v.is_empty() {
        return 0;
    }
    if v.eq_ignore_ascii_case("infinity") || v.eq_ignore_ascii_case("default") {
        return u64::MAX;
    }
    if let Some(pct) = v.strip_suffix('%') {
        return match pct.trim().parse::<f64>() {
            Ok(p) if p >= 0.0 => ((p / 100.0) * 100_000.0) as u64,
            _ => u64::MAX,
        };
    }
    parse_usec_value(v, 0)
}

/// Parse a systemd time value into microseconds.
fn parse_usec_value(value: &str, default: u64) -> u64 {
    let v = value.trim();
    if v.is_empty() {
        return default;
    }
    if v.eq_ignore_ascii_case("infinity") {
        return u64::MAX;
    }
    if v.eq_ignore_ascii_case("default") {
        return default;
    }
    let (digits, mult) = if let Some(d) = v.strip_suffix("ms") {
        (d, 1_000)
    } else if let Some(d) = v.strip_suffix("min") {
        (d, 60_000_000)
    } else if let Some(d) = v.strip_suffix('s') {
        (d, 1_000_000)
    } else if let Some(d) = v.strip_suffix('h') {
        (d, 3_600_000_000)
    } else {
        (v, 1)
    };
    match digits.trim().parse::<u64>() {
        Ok(n) => n.saturating_mul(mult),
        Err(_) => default,
    }
}