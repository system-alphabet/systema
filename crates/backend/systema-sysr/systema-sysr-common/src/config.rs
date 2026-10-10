//! The resource-control configuration model.
//!
//! [`ResourceConfig`] mirrors the `resource_control` projection of the
//! unified [`UnitIR`](https://docs.rs/systema-sysf/latest/systema_sysf/ir/struct.ResourceControl.html)
//! exactly: it is populated from the protobuf `ResourceConfig` message that
//! System A pushes in `UnitResourceEvent`, never parsed from unit files on
//! disk.  String fields preserve the value as written in the unit file
//! (`"50%"`, `"1G"`, `"0-3"`); the Linux backend normalises them when
//! writing to cgroupfs.

use crate::paths::{cpu_quota_to_cpu_max_period, parse_memory_size, parse_cpu_period_us};

/// Parser default for `TasksMax=` when the directive is absent (unlimited).
pub const DEFAULT_TASKS_MAX: u32 = u32::MAX;

/// A resource-control configuration: the subset of systemd's
/// resource-control directives (see `systemd.resource-control(5)`) that
/// System R enforces on cgroup v2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceConfig {
    /// `CPUQuota=` — e.g. `"50%"`.
    pub cpu_quota: String,
    /// `CPUQuotaPeriodSec=` — e.g. `"100ms"`; empty means the default.
    pub cpu_quota_period: String,
    /// `CPUWeight=` — relative CPU weight (1..=10000, default 100).
    pub cpu_weight: u32,
    /// `StartupCPUWeight=` — weight used during early boot.
    pub startup_cpu_weight: u32,
    /// `CPUSetCPUs=` — e.g. `"0-3"`.
    pub cpu_set_cpus: String,
    /// `CPUSetMemoryNodes=` — e.g. `"0"`.
    pub cpu_set_memory_nodes: String,
    /// `MemoryMin=` — e.g. `"128M"`.
    pub memory_min: String,
    /// `MemoryLow=` — e.g. `"256M"`.
    pub memory_low: String,
    /// `MemoryHigh=` — e.g. `"512M"`.
    pub memory_high: String,
    /// `MemoryMax=` — e.g. `"1G"`.
    pub memory_max: String,
    /// `MemorySwapMax=` — e.g. `"512M"`.
    pub memory_swap_max: String,
    /// `IOWeight=` — relative I/O weight (1..=10000, default 100).
    pub io_weight: u32,
    /// `StartupIOWeight=` — weight used during early boot.
    pub startup_io_weight: u32,
    /// Per-device I/O weights, e.g. `"/dev/sda 100"`.
    pub io_device_weight: Vec<String>,
    /// Per-device read bandwidth limits, e.g. `"/dev/sda 10M"`.
    pub io_read_bandwidth_max: Vec<String>,
    /// Per-device write bandwidth limits.
    pub io_write_bandwidth_max: Vec<String>,
    /// `TasksMax=` — maximum number of tasks (default unlimited).
    pub tasks_max: u32,
    /// `AllowedCPUs=` — e.g. `"0-3"`.
    pub allowed_cpus: String,
    /// `AllowedMemoryNodes=` — e.g. `"0"`.
    pub allowed_memory_nodes: String,
}

impl Default for ResourceConfig {
    fn default() -> Self {
        ResourceConfig {
            cpu_quota: String::new(),
            cpu_quota_period: String::new(),
            cpu_weight: 0,
            startup_cpu_weight: 0,
            cpu_set_cpus: String::new(),
            cpu_set_memory_nodes: String::new(),
            memory_min: String::new(),
            memory_low: String::new(),
            memory_high: String::new(),
            memory_max: String::new(),
            memory_swap_max: String::new(),
            io_weight: 0,
            startup_io_weight: 0,
            io_device_weight: Vec::new(),
            io_read_bandwidth_max: Vec::new(),
            io_write_bandwidth_max: Vec::new(),
            tasks_max: DEFAULT_TASKS_MAX,
            allowed_cpus: String::new(),
            allowed_memory_nodes: String::new(),
        }
    }
}

impl ResourceConfig {
    /// Project a protobuf `ResourceConfig` (as carried in
    /// [`UnitResourceEvent`](sysa::proto::UnitResourceEvent)) into the
    /// model.  Field-for-field copy: string values keep their unit-file form
    /// so the backend can normalise them when writing to cgroupfs.
    ///
    /// A proto `tasks_max` of `0` means "unset" (proto3 scalar default) and
    /// maps to the unlimited default.
    pub fn from_proto(p: &sysa::proto::ResourceConfig) -> Self {
        ResourceConfig {
            cpu_quota: p.cpu_quota.clone(),
            cpu_quota_period: p.cpu_quota_period.clone(),
            cpu_weight: p.cpu_weight,
            startup_cpu_weight: p.startup_cpu_weight,
            cpu_set_cpus: p.cpu_set_cpus.clone(),
            cpu_set_memory_nodes: p.cpu_set_memory_nodes.clone(),
            memory_min: p.memory_min.clone(),
            memory_low: p.memory_low.clone(),
            memory_high: p.memory_high.clone(),
            memory_max: p.memory_max.clone(),
            memory_swap_max: p.memory_swap_max.clone(),
            io_weight: p.io_weight,
            startup_io_weight: p.startup_io_weight,
            io_device_weight: p.io_device_weight.clone(),
            io_read_bandwidth_max: p.io_read_bandwidth_max.clone(),
            io_write_bandwidth_max: p.io_write_bandwidth_max.clone(),
            tasks_max: if p.tasks_max == 0 {
                DEFAULT_TASKS_MAX
            } else {
                p.tasks_max
            },
            allowed_cpus: p.allowed_cpus.clone(),
            allowed_memory_nodes: p.allowed_memory_nodes.clone(),
        }
    }

    /// True when no resource-control directive is set.
    ///
    /// Defaults (weights of 0, `tasks_max` of 0 or [`DEFAULT_TASKS_MAX`])
    /// count as unset so a unit without resource directives produces an
    /// "empty" config that writes nothing to cgroupfs.
    pub fn is_empty(&self) -> bool {
        self.cpu_quota.is_empty()
            && self.cpu_quota_period.is_empty()
            && (self.cpu_weight == 0 || self.cpu_weight == 100)
            && (self.startup_cpu_weight == 0 || self.startup_cpu_weight == 100)
            && self.cpu_set_cpus.is_empty()
            && self.cpu_set_memory_nodes.is_empty()
            && self.memory_min.is_empty()
            && self.memory_low.is_empty()
            && self.memory_high.is_empty()
            && self.memory_max.is_empty()
            && self.memory_swap_max.is_empty()
            && (self.io_weight == 0 || self.io_weight == 100)
            && (self.startup_io_weight == 0 || self.startup_io_weight == 100)
            && self.io_device_weight.is_empty()
            && self.io_read_bandwidth_max.is_empty()
            && self.io_write_bandwidth_max.is_empty()
            && (self.tasks_max == 0 || self.tasks_max == DEFAULT_TASKS_MAX)
            && self.allowed_cpus.is_empty()
            && self.allowed_memory_nodes.is_empty()
    }

    /// A valid CPU weight in the cgroup v2 range (1..=10000), falling back
    /// to `StartupCPUWeight=` only when `CPUWeight=` is unset (0).  A
    /// set-but-invalid `CPUWeight=` yields `None`.
    pub fn cpu_weight_v2(&self) -> Option<u32> {
        if self.cpu_weight != 0 {
            validate_weight(self.cpu_weight)
        } else {
            validate_weight(self.startup_cpu_weight)
        }
    }

    /// A valid I/O weight in the cgroup v2 range (1..=10000), falling back
    /// to `StartupIOWeight=` only when `IOWeight=` is unset (0).  A
    /// set-but-invalid `IOWeight=` yields `None`.
    pub fn io_weight_v2(&self) -> Option<u32> {
        if self.io_weight != 0 {
            validate_weight(self.io_weight)
        } else {
            validate_weight(self.startup_io_weight)
        }
    }

    /// Parsed `MemoryMin=` in bytes, or `None` when unset/unparseable.
    pub fn memory_min_bytes(&self) -> Option<u64> {
        parse_memory_size(&self.memory_min)
    }

    /// Parsed `MemoryLow=` in bytes.
    pub fn memory_low_bytes(&self) -> Option<u64> {
        parse_memory_size(&self.memory_low)
    }

    /// Parsed `MemoryHigh=` in bytes.
    pub fn memory_high_bytes(&self) -> Option<u64> {
        parse_memory_size(&self.memory_high)
    }

    /// Parsed `MemoryMax=` in bytes.
    pub fn memory_max_bytes(&self) -> Option<u64> {
        parse_memory_size(&self.memory_max)
    }

    /// Parsed `MemorySwapMax=` in bytes, or `None` when unset/unparseable.
    pub fn memory_swap_max_bytes(&self) -> Option<u64> {
        parse_memory_size(&self.memory_swap_max)
    }

    /// Parsed CPU quota as a cgroup v2 `cpu.max` payload, honouring
    /// `CPUQuotaPeriodSec=` (default period 100ms).
    pub fn cpu_max(&self) -> Option<String> {
        let period = parse_cpu_period_us(&self.cpu_quota_period)
            .unwrap_or(crate::CPU_MAX_PERIOD_US);
        cpu_quota_to_cpu_max_period(&self.cpu_quota, period)
    }

    /// `TasksMax=` as a cgroup v2 `pids.max` value.
    ///
    /// Unlimited (the default) yields `None`; a value of `0` means
    /// "no limit" in systemd and maps to the literal `"max"`.
    pub fn pids_max(&self) -> Option<String> {
        match self.tasks_max {
            DEFAULT_TASKS_MAX => None,
            0 => Some("max".to_string()),
            n => Some(n.to_string()),
        }
    }
}

/// Validate a cgroup v2 weight (1..=10000).
pub fn validate_weight(v: u32) -> Option<u32> {
    (1..=10000).contains(&v).then_some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ResourceConfig {
        ResourceConfig::default()
    }

    #[test]
    fn empty_config() {
        assert!(cfg().is_empty());
        let mut c = cfg();
        c.cpu_quota = "50%".to_string();
        assert!(!c.is_empty());
    }

    #[test]
    fn weights_fall_back_to_startup() {
        let mut c = cfg();
        c.cpu_weight = 0;
        c.startup_cpu_weight = 200;
        assert_eq!(c.cpu_weight_v2(), Some(200));
        c.cpu_weight = 300;
        assert_eq!(c.cpu_weight_v2(), Some(300));
        c.cpu_weight = 10001;
        assert_eq!(c.cpu_weight_v2(), None);
    }

    #[test]
    fn memory_helpers() {
        let mut c = cfg();
        c.memory_max = "1G".to_string();
        c.memory_high = "512M".to_string();
        c.memory_swap_max = "256M".to_string();
        assert_eq!(c.memory_max_bytes(), Some(1 << 30));
        assert_eq!(c.memory_high_bytes(), Some(512 << 20));
        assert_eq!(c.memory_swap_max_bytes(), Some(256 << 20));
        assert_eq!(c.memory_low_bytes(), None);
    }

    #[test]
    fn cpu_max_helper() {
        let mut c = cfg();
        c.cpu_quota = "50%".to_string();
        assert_eq!(c.cpu_max(), Some("50000 100000".to_string()));
        c.cpu_quota_period = "50ms".to_string();
        assert_eq!(c.cpu_max(), Some("25000 50000".to_string()));
    }

    #[test]
    fn pids_max_helper() {
        let mut c = cfg();
        assert_eq!(c.pids_max(), None);
        c.tasks_max = 0;
        assert_eq!(c.pids_max(), Some("max".to_string()));
        c.tasks_max = 512;
        assert_eq!(c.pids_max(), Some("512".to_string()));
    }

    #[test]
    fn from_proto_maps_fields_and_defaults() {
        use sysa::proto::ResourceConfig as Proto;
        let p = Proto {
            cpu_quota: "50%".to_string(),
            memory_max: "1G".to_string(),
            tasks_max: 0,
            ..Default::default()
        };
        let c = ResourceConfig::from_proto(&p);
        assert_eq!(c.cpu_quota, "50%");
        assert_eq!(c.memory_max, "1G");
        assert_eq!(c.tasks_max, DEFAULT_TASKS_MAX);
        assert_eq!(c.cpu_max(), Some("50000 100000".to_string()));
        assert_eq!(c.memory_max_bytes(), Some(1 << 30));

        let p = Proto {
            tasks_max: 256,
            ..Default::default()
        };
        let c = ResourceConfig::from_proto(&p);
        assert_eq!(c.tasks_max, 256);
    }
}
