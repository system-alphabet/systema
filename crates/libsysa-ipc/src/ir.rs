//! Unified unit intermediate representation, abstracted across all init
//! systems (systemd, SysV, OpenRC, Runit, ...).
//!
//! Lives in the IPC kernel because it is the payload the unit-declare and
//! staging flows exchange: the model is serialized to JSON bytes and
//! carried inside the protobuf `units_json` fields of the wire messages
//! (`proto/common.proto`, `proto/workload.proto`, `proto/control.proto`).
//! The transport itself stays model-agnostic — the two-plane boundary
//! (`common.proto` as the only shared import) still applies, and the type
//! contract is versioned as source text shared by the producer (System F)
//! and its consumers (System A and the workers).

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
    /// `Type=` — the startup/readiness model (`simple`, `notify`,
    /// `notify-reload`, `oneshot`, `dbus`, `forking`, `idle`).  Carried
    /// verbatim from the definition: consumers must not re-derive it from
    /// `ExecStart=`, because the notify/dbus types decide when a start job
    /// completes (and `notify` additionally gets `$NOTIFY_SOCKET`).
    #[serde(default)]
    pub service_type: String,
    /// `BusName=` required by `Type=dbus` and used to detect service readiness.
    #[serde(default)]
    pub bus_name: String,
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
    /// `StateDirectory=` fragments (below /var/lib in the system domain);
    /// the worker creates them, chowns them to the service user and exports
    /// `$STATE_DIRECTORY` (colon-joined) — systemd.exec(5) directory
    /// directives.  Mode 0 means "unset" → the worker's 0755 default.
    pub state_directory: Vec<String>,
    pub state_directory_mode: u32,
    /// `LogsDirectory=` fragments (below /var/log) → `$LOGS_DIRECTORY`.
    pub logs_directory: Vec<String>,
    pub logs_directory_mode: u32,
    /// `RuntimeDirectory=` fragments (below /run) → `$RUNTIME_DIRECTORY`.
    pub runtime_directory: Vec<String>,
    pub runtime_directory_mode: u32,
    /// `ConfigurationDirectory=` fragments (below /etc) →
    /// `$CONFIGURATION_DIRECTORY`.
    pub configuration_directory: Vec<String>,
    pub configuration_directory_mode: u32,
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
    /// The unit takes ownership of its cgroup subtree (`Delegate=`): the
    /// cgroup is chowned to the unit's User=/Group= and marked delegated.
    pub delegate: bool,
    /// Controllers left enabled below the unit's cgroup when delegating
    /// (`DelegateControllers=`); empty = all currently enabled ones.
    pub delegate_controllers: Vec<String>,
    /// Subdirectory created (and owned by the delegate) under the unit's
    /// cgroup at delegation time (`DelegateSubgroup=`), e.g. `init.scope`.
    pub delegate_subgroup: String,
}

/// Socket-specific configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SocketConfig {
    pub listen_stream: Vec<String>,
    pub listen_datagram: Vec<String>,
    #[serde(default)]
    pub listen_sequential_packet: Vec<String>,
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

/// Path-specific configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PathConfig {
    pub path_exists: Vec<String>,
    pub path_exists_glob: Vec<String>,
    pub path_changed: Vec<String>,
    pub path_modified: Vec<String>,
    pub directory_not_empty: Vec<String>,
    pub unit: String,
    pub make_directory: bool,
    pub directory_mode: String,
    pub trigger_limit_interval_sec: u32,
    pub trigger_limit_burst: u32,
}

/// Template metadata for a template definition or one of its instances.
///
/// A template unit (`getty@.service`) is a definition, not a runnable unit:
/// its values keep the instance specifiers (`%i`, `%I`) unexpanded so that a
/// consumer can materialize any instance from it.  An instance
/// (`getty@tty3.service`) records the parameters that were substituted and the
/// template it specializes.  A producer that reports an instance has already
/// merged that instance's own drop-ins into it, which is what makes a reported
/// instance win over a template instantiation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateInfo {
    /// True when this IR *is* the template definition (`getty@.service`),
    /// rather than an instance of one.
    #[serde(default)]
    pub definition: bool,

    /// The template this IR specializes (`getty@.service` for
    /// `getty@tty3.service`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub of: Option<String>,

    /// The parameters substituted for the template, in order.  systemd carries
    /// at most one (the `%i` instance string); a vector is the reserved shape
    /// for backends that template on more than one value.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parameters: Vec<String>,
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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    ///
    /// The systemd finder no longer reports aliases here: it resolves them
    /// against the filesystem (which name actually exists, and in which
    /// search-path directory) and reports one [`Self::alias_of`] record per
    /// alias name instead.  The field stays for producers that only declare
    /// aliases.
    #[serde(default)]
    pub aliases: Vec<String>,

    /// Set on an *alias record*: this IR does not define a unit, it declares
    /// that `id` is another name for the canonical unit `alias_of`.
    ///
    /// Alias ownership is decided by the Finder, which is the component that
    /// sees the filesystem and therefore knows whether a name is materialised
    /// as a symlink or only declared via `[Install] Alias=`, and which
    /// search-path directory wins.  System A only records the mapping: it
    /// never chooses between competing declarations.
    ///
    /// Consumers must not create a unit for an alias record — the alias
    /// shares the target's jobs, state and object identity.  No other field
    /// of an alias record is meaningful.
    #[serde(default)]
    pub alias_of: Option<String>,

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
    #[serde(default)]
    pub path: Option<PathConfig>,

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

    /// Set by the Finder: whether the unit is enabled, i.e. whether the unit
    /// file set makes it part of the boot set.
    ///
    /// Enablement is a fact about the filesystem, not about the unit file
    /// body: `[Install] WantedBy=` in [`Self::wanted_by`] is only the recipe
    /// `systemctl enable` follows, never proof that it ran.  Only the Finder
    /// sees the search paths, so it owns the answer (the same way it owns
    /// [`Self::alias_of`]); System A stores and reports it verbatim.
    ///
    /// `None` when the producer does not report enablement (dynamic units,
    /// partial updates).  A producer that reports a unit as enabled must also
    /// be able to report it as disabled: the value describes the file set,
    /// not this single IR.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,

    /// Full systemd `UnitFileState` for this unit, as reported by the Finder:
    /// `"enabled"`, `"enabled-runtime"`, `"linked"`, `"linked-runtime"`,
    /// `"static"`, `"indirect"` or `"disabled"`.
    ///
    /// [`Self::enabled`] is only the coarse "belongs to some target's boot
    /// set" verdict.  The finer distinctions (`/run` links, unit-file links,
    /// installable-but-unlinked, `Also=`-only) are filesystem facts that only
    /// the Finder can observe, so it reports the state explicitly and System
    /// A stores it verbatim.  `None` when the producer does not report it, in
    /// which case System A derives the state from [`Self::enabled`] and the
    /// install information it already has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit_file_state: Option<String>,

    /// Template metadata: set on a template definition (`getty@.service`) or
    /// on an instance of one.  `None` for ordinary units.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<TemplateInfo>,
}

impl UnitIR {
    /// True when this IR is a template definition, not a runnable unit.
    pub fn is_template(&self) -> bool {
        self.template.as_ref().is_some_and(|t| t.definition)
    }

    /// True when this IR is an instance of a template.
    pub fn is_template_instance(&self) -> bool {
        self.template
            .as_ref()
            .is_some_and(|t| !t.definition && t.of.is_some())
    }

    /// The template this IR specializes, if it is an instance.
    pub fn template_of(&self) -> Option<&str> {
        self.template.as_ref().and_then(|t| t.of.as_deref())
    }

    /// The parameters substituted for the template, empty for non-instances.
    pub fn template_parameters(&self) -> &[String] {
        self.template
            .as_ref()
            .map(|t| t.parameters.as_slice())
            .unwrap_or(&[])
    }
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
    /// Whether this is a *triggering* condition (prefixed with `|`).
    ///
    /// systemd semantics: when at least one triggering condition exists, the
    /// unit starts if any triggering condition applies **and** every regular
    /// condition applies.  Without triggering conditions all conditions must
    /// apply (logical AND).
    #[serde(default)]
    pub trigger: bool,
}

impl Condition {
    /// Build a condition from a raw directive value, honoring the `|` trigger
    /// and `!` negation prefixes (`|` comes first, then `!`, per systemd.unit).
    pub fn from_value(kind: &str, value: &str) -> Self {
        let (trigger, rest) = match value.strip_prefix('|') {
            Some(rest) => (true, rest),
            None => (false, value),
        };
        let (negate, val) = match rest.strip_prefix('!') {
            Some(rest) => (true, rest.to_string()),
            None => (false, rest.to_string()),
        };
        Condition {
            kind: kind.to_string(),
            value: val,
            negate,
            trigger,
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

#[cfg(test)]
mod tests {
    use super::ServiceConfig;

    #[test]
    fn service_config_defaults_bus_name_when_reading_legacy_ir() {
        let mut value = serde_json::to_value(ServiceConfig::default()).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .remove("bus_name")
            .expect("serialized bus_name");

        let config: ServiceConfig = serde_json::from_value(value).unwrap();
        assert!(config.bus_name.is_empty());
    }
}
