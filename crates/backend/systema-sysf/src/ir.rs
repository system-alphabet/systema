use std::collections::HashSet;

use serde::{Deserialize, Serialize};

/// Unified unit type, abstracted across all init systems.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum UnitType {
    Service,
    Target,
    Mount,
    Automount,
    Timer,
    Socket,
    Slice,
    Scope,
    Swap,
    Path,
    Device,
    Power,
    /// Custom/unknown unit type from a foreign init system.
    Other(String),
}

impl UnitType {
    pub fn as_str(&self) -> &str {
        match self {
            UnitType::Service => "service",
            UnitType::Target => "target",
            UnitType::Mount => "mount",
            UnitType::Automount => "automount",
            UnitType::Timer => "timer",
            UnitType::Socket => "socket",
            UnitType::Slice => "slice",
            UnitType::Scope => "scope",
            UnitType::Swap => "swap",
            UnitType::Path => "path",
            UnitType::Device => "device",
            UnitType::Power => "power",
            UnitType::Other(s) => s.as_str(),
        }
    }
}

/// Dependency relationships between units.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DependencySet {
    /// Units that must start before this one.
    pub after: HashSet<String>,
    /// Units that should start before this one.
    pub before: HashSet<String>,
    /// Hard dependencies: these units must be active.
    pub requires: HashSet<String>,
    /// Soft dependencies: start if possible, ignore on failure.
    pub wants: HashSet<String>,
    /// Conflicting units: stop these when this unit starts.
    pub conflicts: HashSet<String>,
    /// Lifecycle binding: stop this unit if the bound unit stops.
    pub binds_to: HashSet<String>,
    /// Like Requires, but the dep must already be active.
    pub requisite: HashSet<String>,
    /// Units that are part of this unit (stop propagation).
    pub part_of: HashSet<String>,
    /// Units to keep continuously activated.
    pub upholds: HashSet<String>,
    /// Trigger targets on success.
    pub on_success: HashSet<String>,
    /// Trigger targets on failure.
    pub on_failure: HashSet<String>,
    /// Reload propagation targets.
    pub propagates_reload_to: HashSet<String>,
    /// Action to perform when the unit succeeds (e.g. `poweroff-force`).
    pub success_action: String,
    pub default_dependencies: bool,
}

/// The command line and modifiers for an executable directive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecCommand {
    /// Full raw command string (with any format-specific prefixes).
    pub raw: String,
    /// Resolved executable path.
    pub program: String,
    /// Command-line arguments.
    pub args: Vec<String>,
    /// Ignore non-zero exit code.
    pub ignore_failure: bool,
    /// Run with elevated privileges.
    pub privileged: bool,
}

/// Service-specific configuration in a unified form.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServiceConfig {
    pub exec_start: Vec<ExecCommand>,
    pub exec_stop: Vec<ExecCommand>,
    pub exec_reload: Vec<ExecCommand>,
    pub exec_start_pre: Vec<ExecCommand>,
    pub exec_start_post: Vec<ExecCommand>,
    pub exec_stop_post: Vec<ExecCommand>,
    pub working_directory: String,
    pub user: String,
    pub group: String,
    pub pam_name: String,
    pub environment: Vec<String>,
    pub environment_file: Vec<String>,
    pub restart_policy: RestartPolicy,
    pub restart_sec: u32,
    pub timeout_start_sec: u32,
    pub timeout_stop_sec: u32,
    pub remain_after_exit: bool,
    pub watchdog_sec: u32,
    pub kill_signal: String,
    pub kill_mode: String,
    pub standard_input: String,
    pub standard_output: String,
    pub standard_error: String,
    pub tty_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum RestartPolicy {
    #[default]
    No,
    OnSuccess,
    OnFailure,
    OnAbnormal,
    OnWatchdog,
    OnAbort,
    Always,
}

impl RestartPolicy {
    pub fn as_str(&self) -> &str {
        match self {
            RestartPolicy::No => "no",
            RestartPolicy::OnSuccess => "on-success",
            RestartPolicy::OnFailure => "on-failure",
            RestartPolicy::OnAbnormal => "on-abnormal",
            RestartPolicy::OnWatchdog => "on-watchdog",
            RestartPolicy::OnAbort => "on-abort",
            RestartPolicy::Always => "always",
        }
    }
}

/// Mount-specific configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MountConfig {
    pub what: String,
    pub where_: String,
    pub type_: String,
    pub options: String,
    pub timeout_sec: u32,
}

/// Automount-specific configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AutomountConfig {
    pub where_: String,
    pub extra_options: String,
    pub timeout_idle_sec: u32,
    pub directory_mode: String,
}

/// Timer-specific configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TimerConfig {
    pub on_active_sec: Option<u32>,
    pub on_boot_sec: Option<u32>,
    pub on_startup_sec: Option<u32>,
    pub on_unit_active_sec: Option<u32>,
    pub on_unit_inactive_sec: Option<u32>,
    pub on_calendar: Vec<String>,
    pub accuracy_sec: u32,
    pub randomized_delay_sec: u32,
    pub unit: String,
    pub persistent: bool,
}

/// Resource-control (cgroup) limits for a unit.
///
/// Mirrors `systemd.resource-control(5)`. String values are kept in their
/// original unit-file form (e.g. `"50%"`, `"1G"`, `"100ms"`) so downstream
/// consumers (System R, D-Bus) can parse them with the precision they need.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceControl {
    /// CPU quota: `"50%"` of one CPU or an absolute time slice like `"100ms"`.
    pub cpu_quota: String,
    /// Period over which `CPUQuota` applies, e.g. `"100ms"`.
    pub cpu_quota_period: String,
    /// Relative CPU weight (default 100).
    pub cpu_weight: u32,
    /// CPU weight used during boot.
    pub startup_cpu_weight: u32,
    /// CPUs the unit's processes may be pinned to (`CPUSetCPUs=`).
    pub cpu_set_cpus: String,
    /// Memory nodes the unit's processes may be pinned to (`CPUSetMemoryNodes=`).
    pub cpu_set_memory_nodes: String,
    /// Hard floor on memory usage.
    pub memory_min: String,
    /// Gentle floor on memory usage.
    pub memory_low: String,
    /// Soft memory limit.
    pub memory_high: String,
    /// Hard memory limit, e.g. `"1G"`.
    pub memory_max: String,
    /// Swap usage limit.
    pub memory_swap_max: String,
    /// IO weight (default 100).
    pub io_weight: u32,
    /// IO weight used during boot.
    pub startup_io_weight: u32,
    /// Per-device IO weights, e.g. `"/dev/sda 100"`.
    pub io_device_weight: Vec<String>,
    /// Per-device read bandwidth limits, e.g. `"/dev/sda 1G"`.
    pub io_read_bandwidth_max: Vec<String>,
    /// Per-device write bandwidth limits.
    pub io_write_bandwidth_max: Vec<String>,
    /// Maximum number of tasks/threads in the unit's cgroup.
    pub tasks_max: u32,
    /// CPUs this unit may run on (`AllowedCPUs=`).
    pub allowed_cpus: String,
    /// Memory nodes this unit may use (`AllowedMemoryNodes=`).
    pub allowed_memory_nodes: String,
}

/// Socket-specific configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SocketConfig {
    pub listen_stream: Vec<String>,
    pub listen_datagram: Vec<String>,
    pub listen_fifo: Vec<String>,
    pub listen_netlink: Vec<String>,
    pub accept: bool,
    pub socket_mode: String,
    pub socket_user: String,
    pub socket_group: String,
    pub backlog: u32,
    pub service: String,
    pub directory_mode: String,
}

/// The unified intermediate representation (IR) for a single unit.
///
/// Every Finder (systemd, SysV, OpenRC, Runit, etc.) produces this type.
/// System Allocator works exclusively with `UnitIR`, never with raw config files.
///
/// Only `id` is mandatory.  Every other field is optional so a commit can be
/// a *partial* update: when the unit already exists in System A's runtime
/// cache, only the fields provided here are overwritten and everything else
/// is left untouched (e.g. a mount-table commit carrying only `mount`
/// config never clobbers the unit's `description`).  When the unit does not
/// exist yet, a new one is created from the provided fields; missing
/// required fields (`unit_type`) then cause an error returned to the worker
/// that submitted the commit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnitIR {
    /// Canonical unit identifier, e.g. "nginx.service", "network", "sshd".
    pub id: String,
    pub unit_type: Option<UnitType>,
    /// Human-readable description.
    pub description: Option<String>,
    /// Source format, e.g. "systemd", "sysv", "openrc", "runit".
    pub source_format: Option<String>,
    /// The file path this unit was loaded from, if applicable.
    pub source_path: Option<String>,

    /// Alias names that refer to this unit (e.g. `display-manager.service`
    /// for `lightdm.service`).  Aliases come from `[Install] Alias=` lines
    /// and from unit-file symlinks (a symlink's basename aliases the
    /// canonical unit it points at).
    #[serde(default)]
    pub aliases: Vec<String>,

    /// `[Unit] Slice=` — the parent slice this unit belongs to (e.g.
    /// `"system.slice"`).  `None` when the directive is absent or empty;
    /// consumers default to `system.slice` at apply time.
    pub slice: Option<String>,

    /// Dependencies on other units.  When provided, the whole dependency
    /// set replaces the previous one.
    pub dependencies: Option<DependencySet>,

    // Optional section-specific configs.
    pub service: Option<ServiceConfig>,
    pub mount: Option<MountConfig>,
    pub automount: Option<AutomountConfig>,
    pub timer: Option<TimerConfig>,
    pub socket: Option<SocketConfig>,

    /// Resource-control (cgroup) limits parsed from the unit file.
    #[serde(default)]
    pub resource_control: Option<ResourceControl>,

    /// Conditions that must be met for the unit to start.
    pub conditions: Option<Vec<Condition>>,
    /// Asserts that cause hard failure if not met.
    pub asserts: Option<Vec<Condition>>,

    /// Install section: which targets want this unit.
    pub wanted_by: Option<Vec<String>>,
    pub required_by: Option<Vec<String>>,
}

/// A condition or assert directive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Condition {
    /// The condition type, e.g. "PathExists", "Virtualization", "Host".
    pub kind: String,
    /// The value to check, possibly negated.
    pub value: String,
    /// Whether the check is negated (prefixed with `!`).
    pub negate: bool,
}

impl Condition {
    /// Build a condition from a raw directive value, honoring the `!`
    /// negation prefix.
    pub fn from_value(kind: &str, value: &str) -> Self {
        let (negate, val) = if let Some(rest) = value.strip_prefix('!') {
            (true, rest.to_string())
        } else {
            (false, value.to_string())
        };
        Condition {
            kind: kind.to_string(),
            value: val,
            negate,
        }
    }
}

impl DependencySet {
    pub fn is_empty(&self) -> bool {
        self.after.is_empty()
            && self.before.is_empty()
            && self.requires.is_empty()
            && self.wants.is_empty()
            && self.conflicts.is_empty()
            && self.binds_to.is_empty()
            && self.requisite.is_empty()
            && self.part_of.is_empty()
            && self.upholds.is_empty()
            && self.on_success.is_empty()
            && self.on_failure.is_empty()
            && self.propagates_reload_to.is_empty()
    }
}
