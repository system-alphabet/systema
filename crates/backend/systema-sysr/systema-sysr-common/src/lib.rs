//! Platform-independent resource-control logic shared by the System R
//! variants.
//!
//! This crate defines the [`ResourceConfig`] model (mirroring the UnitIR
//! `resource_control` projection that System A pushes to System R), the pure
//! conversions between systemd resource values and cgroup v2 files, the
//! [`ResourceController`] trait that every backend implements, and a
//! [`NoopController`] fallback used when no cgroup filesystem is available
//! (non-Linux platforms, containers without cgroupfs, ...).
//!
//! Only Linux ships a real backend (`systema-sysr.linux`, cgroup v2); on
//! every other platform resource control degrades gracefully to a no-op and
//! units still start unconstrained.

mod config;
mod controller;
mod paths;

pub use config::{ResourceConfig, DEFAULT_TASKS_MAX};
pub use controller::{
    CgroupBaseline, CgroupMetrics, CgroupProcess, NoopController, ResourceController, ResourceError,
};
pub use paths::{
    bytes_to_string, cpu_quota_to_cpu_max, cpu_quota_to_cpu_max_period, parse_cpu_period_us,
    parse_cpu_quota_percent, parse_memory_size, slice_cgroup_path, slice_name_components,
    split_device_directive, unit_cgroup_path, user_manager_cgroup_path, user_slice_cgroup_path,
    user_slice_name, user_slice_root_path, CGROUP_ROOT, USER_SLICE_NAME,
};

/// The number of microseconds in the cgroup v2 `cpu.max` period.  systemd
/// uses a fixed 100ms period; we mirror that.
pub const CPU_MAX_PERIOD_US: u64 = 100_000;
