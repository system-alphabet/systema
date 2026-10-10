//! Unit file types for System A.
//!
//! These mirror the relevant subset of systemd unit configuration needed for
//! Phase 1 (service management + target activation).

use std::collections::HashSet;

use serde::Serialize;

/// The kind of a systemd unit.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum UnitKind {
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
    Unknown(String),
}

impl UnitKind {
    pub fn from_extension(name: &str) -> Self {
        let ext = name.rsplit('.').next().unwrap_or("");
        match ext {
            "service" => UnitKind::Service,
            "target" => UnitKind::Target,
            "mount" => UnitKind::Mount,
            "automount" => UnitKind::Automount,
            "timer" => UnitKind::Timer,
            "socket" => UnitKind::Socket,
            "slice" => UnitKind::Slice,
            "scope" => UnitKind::Scope,
            "swap" => UnitKind::Swap,
            "path" => UnitKind::Path,
            "device" => UnitKind::Device,
            "power" => UnitKind::Power,
            other => UnitKind::Unknown(other.to_string()),
        }
    }

    /// Returns the worker unit-type string used in IPC registration.
    pub fn worker_type(&self) -> &str {
        match self {
            UnitKind::Service => "service",
            UnitKind::Target => "target",
            UnitKind::Mount => "mount",
            UnitKind::Automount => "automount",
            UnitKind::Timer => "timer",
            UnitKind::Socket => "socket",
            UnitKind::Slice => "slice",
            UnitKind::Scope => "scope",
            UnitKind::Swap => "swap",
            UnitKind::Path => "path",
            UnitKind::Device => "device",
            UnitKind::Power => "power",
            UnitKind::Unknown(s) => s.as_str(),
        }
    }
}

impl Serialize for UnitKind {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.worker_type())
    }
}

// ---------------------------------------------------------------------------
// ExecCommand
// ---------------------------------------------------------------------------

/// A parsed `Exec*=` directive value with all modifier prefix flags extracted.
///
/// Systemd supports the following single-character prefixes before the
/// executable path, possibly combined in any order:
///
/// | Prefix | Meaning |
/// |--------|---------|
/// | `-`    | Ignore a non-zero exit code (don't mark unit as failed). |
/// | `+`    | Run with full privileges (as root, regardless of `User=`/`Group=`). |
/// | `@`    | Pass the following token as `argv[0]` to the process. |
/// | `:`    | Do not kill the process when the unit stops (no `SIGKILL`). |
/// | `!`    | Apply `NoNewPrivileges=yes` to the spawned process. |
/// | `!!`   | Like `!`, but only in the seccomp sense (two consecutive `!`). |
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecCommand {
    /// The original value as written in the unit file (including any prefixes).
    pub raw: String,
    /// Resolved executable path (no prefix modifiers, first word of the command).
    pub program: String,
    /// Command-line arguments (all words after the program).
    pub args: Vec<String>,
    /// `-` prefix: do not treat a non-zero exit code as failure.
    pub ignore_failure: bool,
    /// `+` prefix: run with elevated / root privileges.
    pub privileged: bool,
    /// `@` prefix: use the next token as `argv[0]` instead of `program`.
    pub no_env_lookup: bool,
    /// `:` prefix: do not SIGKILL the process when the unit is deactivated.
    pub no_kill_on_stop: bool,
    /// `!` or `!!` prefix: apply `NoNewPrivileges`.
    pub no_new_privileges: bool,
}

impl ExecCommand {
    /// Parse a raw `Exec*=` line, stripping modifier prefixes and splitting
    /// the remaining text into `program` + `args`.
    pub fn parse(raw: impl Into<String>) -> Self {
        let raw: String = raw.into();
        let trimmed = raw.trim_start();

        let mut cursor = trimmed;
        let mut ignore_failure = false;
        let mut privileged = false;
        let mut no_env_lookup = false;
        let mut no_kill_on_stop = false;
        let mut no_new_privileges = false;

        // Collect prefix characters until we hit something that isn't one.
        loop {
            match cursor.as_bytes().first() {
                Some(b'-') => {
                    ignore_failure = true;
                    cursor = &cursor[1..];
                }
                Some(b'+') => {
                    privileged = true;
                    cursor = &cursor[1..];
                }
                Some(b'@') => {
                    no_env_lookup = true;
                    cursor = &cursor[1..];
                }
                Some(b':') => {
                    no_kill_on_stop = true;
                    cursor = &cursor[1..];
                }
                Some(b'!') => {
                    no_new_privileges = true;
                    cursor = &cursor[1..];
                }
                _ => break,
            }
        }

        let parts = shell_words(cursor);
        let program = parts.first().cloned().unwrap_or_default();
        let args = parts.into_iter().skip(1).collect();

        ExecCommand {
            raw,
            program,
            args,
            ignore_failure,
            privileged,
            no_env_lookup,
            no_kill_on_stop,
            no_new_privileges,
        }
    }

    /// Returns a plain command string (program + space-joined args) without
    /// any modifier prefixes.  Suitable for display or for passing to a shell.
    pub fn command_line(&self) -> String {
        if self.args.is_empty() {
            self.program.clone()
        } else {
            format!("{} {}", self.program, self.args.join(" "))
        }
    }
}

impl Serialize for ExecCommand {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("ExecCommand", 9)?;
        st.serialize_field("raw", &self.raw)?;
        st.serialize_field("program", &self.program)?;
        st.serialize_field("args", &self.args)?;
        st.serialize_field("ignore_failure", &self.ignore_failure)?;
        st.serialize_field("privileged", &self.privileged)?;
        st.serialize_field("no_env_lookup", &self.no_env_lookup)?;
        st.serialize_field("no_kill_on_stop", &self.no_kill_on_stop)?;
        st.serialize_field("no_new_privileges", &self.no_new_privileges)?;
        st.end()
    }
}

// ---------------------------------------------------------------------------
// Shell-word splitter (private helper used by ExecCommand::parse)
// ---------------------------------------------------------------------------

/// Split a command-line string into words, respecting single and double
/// quotes and backslash escapes.
fn shell_words(s: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;

    for ch in s.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' if !in_single => {
                escaped = true;
            }
            '\'' if !in_double => {
                in_single = !in_single;
            }
            '"' if !in_single => {
                in_double = !in_double;
            }
            ' ' | '\t' if !in_single && !in_double => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            other => {
                current.push(other);
            }
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

// ---------------------------------------------------------------------------
// [Unit] section
// ---------------------------------------------------------------------------

/// The action taken when a unit exits successfully (`[Unit] SuccessAction=`).
///
/// In systemd, the `power*`/`reboot*`/`halt*`/`kexec*`/`suspend*`/`hibernate*`
/// variants are executed by PID 1 when the unit terminates successfully
/// (e.g. `systemd-poweroff.service` carries `SuccessAction=poweroff-force`,
/// which is how `systemctl poweroff` actually powers the machine down).
///
/// In System A the power variants map to a `.power` unit
/// ([`Self::power_unit_name`]); starting that unit dispatches the transition
/// to System Init (the in-process owner of the `power` unit type).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SuccessAction {
    #[default]
    None,
    Poweroff,
    PoweroffForce,
    PoweroffImmediate,
    Reboot,
    RebootForce,
    RebootImmediate,
    Halt,
    HaltForce,
    HaltImmediate,
    Kexec,
    KexecForce,
    KexecImmediate,
    Suspend,
    SuspendForce,
    SuspendImmediate,
    Hibernate,
    HibernateForce,
    HibernateImmediate,
    Exit,
    ExitForce,
    ExitImmediate,
}

impl SuccessAction {
    /// Return the `.power` unit name for this action, or `None` if it does
    /// not map to a power unit (e.g. `Exit`/`ExitForce`).
    pub fn power_unit_name(&self) -> Option<&'static str> {
        match self {
            Self::Poweroff | Self::PoweroffForce | Self::PoweroffImmediate => {
                Some("poweroff.power")
            }
            Self::Reboot | Self::RebootForce | Self::RebootImmediate => Some("reboot.power"),
            Self::Halt | Self::HaltForce | Self::HaltImmediate => Some("halt.power"),
            Self::Kexec | Self::KexecForce | Self::KexecImmediate => Some("kexec.power"),
            Self::Suspend | Self::SuspendForce | Self::SuspendImmediate => Some("suspend.power"),
            Self::Hibernate | Self::HibernateForce | Self::HibernateImmediate => {
                Some("hibernate.power")
            }
            Self::None | Self::Exit | Self::ExitForce | Self::ExitImmediate => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "",
            Self::Poweroff => "poweroff",
            Self::PoweroffForce => "poweroff-force",
            Self::PoweroffImmediate => "poweroff-immediate",
            Self::Reboot => "reboot",
            Self::RebootForce => "reboot-force",
            Self::RebootImmediate => "reboot-immediate",
            Self::Halt => "halt",
            Self::HaltForce => "halt-force",
            Self::HaltImmediate => "halt-immediate",
            Self::Kexec => "kexec",
            Self::KexecForce => "kexec-force",
            Self::KexecImmediate => "kexec-immediate",
            Self::Suspend => "suspend",
            Self::SuspendForce => "suspend-force",
            Self::SuspendImmediate => "suspend-immediate",
            Self::Hibernate => "hibernate",
            Self::HibernateForce => "hibernate-force",
            Self::HibernateImmediate => "hibernate-immediate",
            Self::Exit => "exit",
            Self::ExitForce => "exit-force",
            Self::ExitImmediate => "exit-immediate",
        }
    }
}

impl From<&str> for SuccessAction {
    fn from(s: &str) -> Self {
        match s.to_lowercase().trim() {
            "poweroff" => Self::Poweroff,
            "poweroff-force" => Self::PoweroffForce,
            "poweroff-immediate" => Self::PoweroffImmediate,
            "reboot" => Self::Reboot,
            "reboot-force" => Self::RebootForce,
            "reboot-immediate" => Self::RebootImmediate,
            "halt" => Self::Halt,
            "halt-force" => Self::HaltForce,
            "halt-immediate" => Self::HaltImmediate,
            "kexec" => Self::Kexec,
            "kexec-force" => Self::KexecForce,
            "kexec-immediate" => Self::KexecImmediate,
            "suspend" => Self::Suspend,
            "suspend-force" => Self::SuspendForce,
            "suspend-immediate" => Self::SuspendImmediate,
            "hibernate" => Self::Hibernate,
            "hibernate-force" => Self::HibernateForce,
            "hibernate-immediate" => Self::HibernateImmediate,
            "exit" => Self::Exit,
            "exit-force" => Self::ExitForce,
            "exit-immediate" => Self::ExitImmediate,
            _ => Self::None,
        }
    }
}

impl Serialize for SuccessAction {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

/// Common `[Unit]` section fields shared by all unit types.
#[derive(Debug, Clone, Serialize)]
pub struct UnitSection {
    pub description: String,
    pub documentation: Vec<String>,
    /// Units that must be active before this unit can start.
    #[serde(with = "crate::types::sorted_set")]
    pub requires: HashSet<String>,
    /// Units that should be active before this unit starts (non-fatal).
    #[serde(with = "crate::types::sorted_set")]
    pub wants: HashSet<String>,
    /// Units that conflict with this unit.
    #[serde(with = "crate::types::sorted_set")]
    pub conflicts: HashSet<String>,
    /// Ordering: start after these units.
    #[serde(with = "crate::types::sorted_set")]
    pub after: HashSet<String>,
    /// Ordering: start before these units.
    #[serde(with = "crate::types::sorted_set")]
    pub before: HashSet<String>,
    /// If these units are stopped, also stop this unit.
    #[serde(with = "crate::types::sorted_set")]
    pub part_of: HashSet<String>,
    /// Bind the lifecycle to these units (if they stop, stop this one).
    #[serde(with = "crate::types::sorted_set")]
    pub binds_to: HashSet<String>,
    /// Like Requires but the dependency must already be active (not started).
    #[serde(with = "crate::types::sorted_set")]
    pub requisite: HashSet<String>,
    /// Continuously maintain activation of these units.
    #[serde(with = "crate::types::sorted_set")]
    pub upholds: HashSet<String>,
    /// Units to activate when this unit succeeds.
    #[serde(with = "crate::types::sorted_set")]
    pub on_success: HashSet<String>,
    /// Units to activate when this unit fails.
    #[serde(with = "crate::types::sorted_set")]
    pub on_failure: HashSet<String>,
    /// Action to perform when the unit succeeds (e.g. `poweroff-force`).
    pub success_action: SuccessAction,
    /// When this unit is reloaded, also reload these units.
    #[serde(with = "crate::types::sorted_set")]
    pub propagates_reload_to: HashSet<String>,
    /// `RequiresMountsFor=` — require every mount unit covering these paths.
    pub requires_mounts_for: Vec<String>,
    /// `WantsMountsFor=` — want every mount unit covering these paths.
    pub wants_mounts_for: Vec<String>,
    pub default_dependencies: bool,
    /// `AllowIsolate=` — whether the unit may be targeted by `isolate`
    /// mode jobs (defaults to false, matching systemd).
    pub allow_isolate: bool,
    /// `Slice=` — the parent slice this unit belongs to
    /// (e.g. `"system.slice"`).  Empty when unset.
    pub slice: String,

    // ------------------------------------------------------------------
    // Start rate limiting (systemd v229+: these live in the [Unit] section)
    // ------------------------------------------------------------------
    /// `StartLimitIntervalSec=` — interval in seconds (0 disables limiting).
    pub start_limit_interval_sec: u32,
    /// `StartLimitBurst=` — max start attempts within the interval (0 disables).
    pub start_limit_burst: u32,
    /// `StartLimitAction=` — action taken when the start rate limit is exceeded.
    pub start_limit_action: StartLimitAction,

    // ------------------------------------------------------------------
    // Condition checks — parsed; evaluated by the scheduler before start.
    // ------------------------------------------------------------------
    /// `ConditionPathExists=` — skip start if path does not exist.
    /// A `!`-prefixed value negates the check.
    pub condition_path_exists: Vec<String>,
    /// `ConditionPathExistsGlob=` — skip start if no path matches the glob.
    pub condition_path_exists_glob: Vec<String>,
    /// `ConditionFileNotEmpty=` — skip start if file is missing or empty.
    pub condition_file_not_empty: Vec<String>,
    /// `ConditionDirectoryNotEmpty=` — skip start if directory is empty/missing.
    pub condition_directory_not_empty: Vec<String>,
    /// `ConditionHost=` — match against hostname or machine ID.
    pub condition_host: Vec<String>,
    /// `ConditionKernelCommandLine=` — match against `/proc/cmdline` token.
    pub condition_kernel_command_line: Vec<String>,
    /// `ConditionVirtualization=` — e.g. `no`, `yes`, `kvm`, `docker`.
    pub condition_virtualization: Vec<String>,
    /// `ConditionSecurity=` — e.g. `selinux`, `apparmor`, `ima`.
    pub condition_security: Vec<String>,
    /// `ConditionCapability=` — kernel capability name.
    pub condition_capability: Vec<String>,
    /// `ConditionACPower=` — `yes` or `no`.
    pub condition_ac_power: Vec<String>,
    /// `ConditionNeedsUpdate=` — path to check for updates (e.g. `/etc`).
    pub condition_needs_update: Vec<String>,
    /// `ConditionFirstBoot=` — `yes` or `no`.
    pub condition_first_boot: Vec<String>,
    /// `ConditionEnvironment=` — environment variable (optionally `=value`).
    pub condition_environment: Vec<String>,
    /// `ConditionMemory=` — memory size comparison, e.g. `>=1G`.
    pub condition_memory: Vec<String>,
    /// `ConditionKernelModuleLoaded=` — check if a kernel module is loaded.
    /// A `!`-prefixed value negates the check (skip if loaded).
    pub condition_kernel_module_loaded: Vec<String>,

    // ------------------------------------------------------------------
    // Assert checks — like Condition but cause a hard failure if not met.
    // ------------------------------------------------------------------
    /// `AssertPathExists=`
    pub assert_path_exists: Vec<String>,
    /// `AssertPathExistsGlob=`
    pub assert_path_exists_glob: Vec<String>,
    /// `AssertFileNotEmpty=`
    pub assert_file_not_empty: Vec<String>,
    /// `AssertDirectoryNotEmpty=`
    pub assert_directory_not_empty: Vec<String>,
    /// `AssertHost=`
    pub assert_host: Vec<String>,
    /// `AssertFirstBoot=`
    pub assert_first_boot: Vec<String>,
    /// `AssertMemory=`
    pub assert_memory: Vec<String>,
}

impl Default for UnitSection {
    /// `DefaultDependencies=` defaults to enabled (systemd unit.c sets it
    /// on allocation), so units constructed programmatically get the same
    /// behavior as parsed ones.
    fn default() -> Self {
        UnitSection {
            description: String::new(),
            documentation: Vec::new(),
            requires: HashSet::new(),
            wants: HashSet::new(),
            conflicts: HashSet::new(),
            after: HashSet::new(),
            before: HashSet::new(),
            part_of: HashSet::new(),
            binds_to: HashSet::new(),
            requisite: HashSet::new(),
            upholds: HashSet::new(),
            on_success: HashSet::new(),
            on_failure: HashSet::new(),
            success_action: SuccessAction::None,
            propagates_reload_to: HashSet::new(),
            requires_mounts_for: Vec::new(),
            wants_mounts_for: Vec::new(),
            default_dependencies: true,
            allow_isolate: false,
            slice: String::new(),
            start_limit_interval_sec: 10,
            start_limit_burst: 5,
            start_limit_action: StartLimitAction::None,
            condition_path_exists: Vec::new(),
            condition_path_exists_glob: Vec::new(),
            condition_file_not_empty: Vec::new(),
            condition_directory_not_empty: Vec::new(),
            condition_host: Vec::new(),
            condition_kernel_command_line: Vec::new(),
            condition_virtualization: Vec::new(),
            condition_security: Vec::new(),
            condition_capability: Vec::new(),
            condition_ac_power: Vec::new(),
            condition_needs_update: Vec::new(),
            condition_first_boot: Vec::new(),
            condition_environment: Vec::new(),
            condition_memory: Vec::new(),
            condition_kernel_module_loaded: Vec::new(),
            assert_path_exists: Vec::new(),
            assert_path_exists_glob: Vec::new(),
            assert_file_not_empty: Vec::new(),
            assert_directory_not_empty: Vec::new(),
            assert_host: Vec::new(),
            assert_first_boot: Vec::new(),
            assert_memory: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// [Install] section
// ---------------------------------------------------------------------------

/// `[Install]` section.
#[derive(Debug, Clone, Default, Serialize)]
pub struct InstallSection {
    /// Targets that want this unit (used for enable/disable).
    #[serde(with = "crate::types::sorted_set")]
    pub wanted_by: HashSet<String>,
    #[serde(with = "crate::types::sorted_set")]
    pub required_by: HashSet<String>,
    #[serde(with = "crate::types::sorted_set")]
    pub also: HashSet<String>,
    pub alias: Vec<String>,
}

// ---------------------------------------------------------------------------
// [Service] section
// ---------------------------------------------------------------------------

/// `[Service]` section.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ServiceSection {
    pub service_type: ServiceType,
    pub exec_start: Vec<ExecCommand>,
    pub exec_start_pre: Vec<ExecCommand>,
    pub exec_start_post: Vec<ExecCommand>,
    pub exec_stop: Vec<ExecCommand>,
    pub exec_stop_post: Vec<ExecCommand>,
    pub exec_reload: Vec<ExecCommand>,
    pub working_directory: String,
    pub user: String,
    pub group: String,
    pub pam_name: String,
    pub environment: Vec<String>,
    pub environment_file: Vec<String>,
    pub pid_file: String,
    pub restart: RestartPolicy,
    pub restart_sec: u32,
    pub timeout_start_sec: u32,
    pub timeout_stop_sec: u32,
    pub remain_after_exit: bool,
    pub bus_name: String,
    /// Socket units to activate via socket activation (`Sockets=`).
    pub sockets: Vec<String>,
    pub notify_access: String,
    pub watchdog_sec: u32,
    pub kill_signal: String,
    pub kill_mode: String,
    pub standard_input: String,
    pub standard_output: String,
    pub standard_error: String,
    pub tty_path: String,
    pub restart_steps: u32,
    pub restart_max_delay_sec: u32,
    /// Resource-control directives from the `[Service]` section.
    pub rc: ResourceControl,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ServiceType {
    #[default]
    Simple,
    Forking,
    Oneshot,
    Dbus,
    Notify,
    NotifyReload,
    Idle,
}

impl ServiceType {
    pub fn as_str(&self) -> &str {
        match self {
            ServiceType::Simple => "simple",
            ServiceType::Forking => "forking",
            ServiceType::Oneshot => "oneshot",
            ServiceType::Dbus => "dbus",
            ServiceType::Notify => "notify",
            ServiceType::NotifyReload => "notify-reload",
            ServiceType::Idle => "idle",
        }
    }
}

impl From<&str> for ServiceType {
    fn from(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "simple" => ServiceType::Simple,
            "forking" => ServiceType::Forking,
            "oneshot" => ServiceType::Oneshot,
            "dbus" => ServiceType::Dbus,
            "notify" => ServiceType::Notify,
            "notify-reload" => ServiceType::NotifyReload,
            "idle" => ServiceType::Idle,
            _ => ServiceType::Simple,
        }
    }
}

impl Serialize for ServiceType {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
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

impl From<&str> for RestartPolicy {
    fn from(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "on-success" => RestartPolicy::OnSuccess,
            "on-failure" => RestartPolicy::OnFailure,
            "on-abnormal" => RestartPolicy::OnAbnormal,
            "on-watchdog" => RestartPolicy::OnWatchdog,
            "on-abort" => RestartPolicy::OnAbort,
            "always" => RestartPolicy::Always,
            _ => RestartPolicy::No,
        }
    }
}

impl Serialize for RestartPolicy {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

/// Describes what action to take when the start rate limit is exceeded.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum StartLimitAction {
    #[default]
    None,
    Reboot,
    RebootForce,
    RebootImmediate,
    Poweroff,
    Exit,
}

impl StartLimitAction {
    pub fn as_str(&self) -> &str {
        match self {
            StartLimitAction::None => "none",
            StartLimitAction::Reboot => "reboot",
            StartLimitAction::RebootForce => "reboot-force",
            StartLimitAction::RebootImmediate => "reboot-immediate",
            StartLimitAction::Poweroff => "poweroff",
            StartLimitAction::Exit => "exit",
        }
    }

    /// Return the `.power` unit name whose start performs this transition,
    /// or `None` for actions that map to no power unit (`None`, `Exit`).
    ///
    /// Like [`SuccessAction::power_unit_name`], this funnels every
    /// start-rate-limit outage through the same `.power` dispatch path as
    /// `SuccessAction=` and direct unit starts, so System Init (which owns
    /// `libsystema-sysp`) is the sole executor of system transitions.
    pub fn power_unit_name(&self) -> Option<&'static str> {
        match self {
            StartLimitAction::Reboot
            | StartLimitAction::RebootForce
            | StartLimitAction::RebootImmediate => Some("reboot.power"),
            StartLimitAction::Poweroff => Some("poweroff.power"),
            StartLimitAction::None | StartLimitAction::Exit => None,
        }
    }
}

impl From<&str> for StartLimitAction {
    fn from(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "reboot" => StartLimitAction::Reboot,
            "reboot-force" => StartLimitAction::RebootForce,
            "reboot-immediate" => StartLimitAction::RebootImmediate,
            "poweroff" => StartLimitAction::Poweroff,
            "exit" => StartLimitAction::Exit,
            _ => StartLimitAction::None,
        }
    }
}

impl Serialize for StartLimitAction {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

/// Classifies how a service process terminated, for restart-policy evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitKind {
    /// Process exited normally with a code.
    ExitCode(i32),
    /// Process was killed by a signal.
    Signal(i32),
    /// Operation timed out.
    Timeout,
    /// Watchdog triggered.
    Watchdog,
}

/// A fully parsed systemd unit file.
#[derive(Debug, Clone)]
pub struct UnitFile {
    /// The canonical unit name, e.g. "sshd.service".
    pub name: String,
    pub kind: UnitKind,
    /// True for transient units created at runtime (e.g. via
    /// `StartTransientUnit`, like logind's session scopes).  Transient units
    /// have no on-disk unit file; their configuration is supplied inline.
    pub transient: bool,
    pub unit: UnitSection,
    pub install: InstallSection,
    /// Present only for service units.
    pub service: Option<ServiceSection>,
    /// Present only for mount units.
    pub mount: Option<MountSection>,
    /// Present only for automount units.
    pub automount: Option<AutomountSection>,
    /// Present only for timer units.
    pub timer: Option<TimerSection>,
    /// Present only for socket units.
    pub socket: Option<SocketSection>,
    /// Present only for swap units.
    pub swap: Option<SwapSection>,
    /// Present only for path units.
    pub path: Option<PathSection>,
    /// Present only for slice units.
    pub slice: Option<SliceSection>,
    /// Present only for scope units.
    pub scope: Option<ScopeSection>,
    /// Present only for device units.
    pub device: Option<DeviceSection>,
}

impl UnitFile {
    pub fn new(name: impl Into<String>) -> Self {
        let name = name.into();
        let kind = UnitKind::from_extension(&name);
        UnitFile {
            name,
            kind,
            transient: false,
            unit: UnitSection::default(),
            install: InstallSection::default(),
            service: None,
            mount: None,
            automount: None,
            timer: None,
            socket: None,
            swap: None,
            path: None,
            slice: None,
            scope: None,
            device: None,
        }
    }
}

// ---------------------------------------------------------------------------
// [Mount] section
// ---------------------------------------------------------------------------

/// `[Mount]` section for `.mount` units.
#[derive(Debug, Clone, Default, Serialize)]
pub struct MountSection {
    /// Device or remote filesystem to mount (`What=`).
    pub what: String,
    /// Mount point path (`Where=`).
    #[serde(rename = "where")]
    pub where_: String,
    /// Filesystem type, e.g. `ext4`, `nfs` (`Type=`).
    #[serde(rename = "type")]
    pub type_: String,
    /// Mount options passed to `mount(8)` (`Options=`).
    pub options: String,
    /// Time to wait for the mount operation in seconds (`TimeoutSec=`).
    pub timeout_sec: u32,
    /// Unmount lazily when the mount point becomes unreachable (`LazyUnmount=`).
    pub lazy_unmount: bool,
    /// Force unmount even if the filesystem is busy (`ForceUnmount=`).
    pub force_unmount: bool,
    /// Permission mode for the mount point directory (`DirectoryMode=`).
    pub directory_mode: String,
    /// Ignore unknown mount options (`SloppyOptions=`).
    pub sloppy_options: bool,
}

// ---------------------------------------------------------------------------
// [Automount] section
// ---------------------------------------------------------------------------

/// `[Automount]` section for `.automount` units.
#[derive(Debug, Clone, Default, Serialize)]
pub struct AutomountSection {
    /// Mount point path (`Where=`).
    #[serde(rename = "where")]
    pub where_: String,
    /// Extra mount options passed to the autofs mount (`ExtraOptions=`).
    pub extra_options: String,
    /// Idle timeout in seconds after which the mount is unmounted (`TimeoutIdleSec=`).
    pub timeout_idle_sec: u32,
    /// Permission mode for the mount point directory (`DirectoryMode=`).
    pub directory_mode: String,
}

// ---------------------------------------------------------------------------
// [Timer] section
// ---------------------------------------------------------------------------

/// `[Timer]` section for `.timer` units.
#[derive(Debug, Clone, Default, Serialize)]
pub struct TimerSection {
    /// Run N seconds after the timer unit was activated (`OnActiveSec=`).
    pub on_active_sec: Option<u32>,
    /// Run N seconds after boot (`OnBootSec=`).
    pub on_boot_sec: Option<u32>,
    /// Run N seconds after systemd started (`OnStartupSec=`).
    pub on_startup_sec: Option<u32>,
    /// Run N seconds after the activated unit last became active (`OnUnitActiveSec=`).
    pub on_unit_active_sec: Option<u32>,
    /// Run N seconds after the activated unit last became inactive (`OnUnitInactiveSec=`).
    pub on_unit_inactive_sec: Option<u32>,
    /// Calendar-based schedules, e.g. `"*-*-* 02:00:00"` (`OnCalendar=`).
    pub on_calendar: Vec<String>,
    /// Accuracy of the timer in seconds (`AccuracySec=`; default 60).
    pub accuracy_sec: u32,
    /// Randomised delay added to the trigger time (`RandomizedDelaySec=`).
    pub randomized_delay_sec: u32,
    /// Unit to activate (default: same stem with `.service` suffix) (`Unit=`).
    pub unit: String,
    /// Save last trigger time across reboots (`Persistent=`).
    pub persistent: bool,
    /// Wake the system from suspend to fire the timer (`WakeSystem=`).
    pub wake_system: bool,
    /// Keep the timer active after it has elapsed once (`RemainAfterElapse=`).
    pub remain_after_elapse: bool,
}

// ---------------------------------------------------------------------------
// [Socket] section
// ---------------------------------------------------------------------------

/// `[Socket]` section for `.socket` units.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SocketSection {
    /// Stream (TCP / Unix stream) listening addresses (`ListenStream=`).
    pub listen_stream: Vec<String>,
    /// Datagram (UDP / Unix datagram) listening addresses (`ListenDatagram=`).
    pub listen_datagram: Vec<String>,
    /// Sequential-packet Unix socket paths (`ListenSequentialPacket=`).
    pub listen_sequential_packet: Vec<String>,
    /// FIFO paths to create (`ListenFIFO=`).
    pub listen_fifo: Vec<String>,
    /// Netlink socket families (`ListenNetlink=`).
    pub listen_netlink: Vec<String>,
    /// Special file paths (`ListenSpecial=`).
    pub listen_special: Vec<String>,
    /// Accept a connection per service instance (`Accept=`).
    pub accept: bool,
    /// Service unit to activate on data arrival (`Service=`).  Empty means
    /// derive the name from the socket unit ("foo.socket" -> "foo.service").
    pub service: String,
    /// User for socket file ownership (`SocketUser=`).
    pub socket_user: String,
    /// Group for socket file ownership (`SocketGroup=`).
    pub socket_group: String,
    /// Socket file permission mode, e.g. `0666` (`SocketMode=`).
    pub socket_mode: String,
    /// Permission mode for automatically created socket directories (`DirectoryMode=`).
    pub directory_mode: String,
    /// Maximum number of concurrent connections (`MaxConnections=`).
    pub max_connections: u32,
    /// Listen backlog (`Backlog=`).
    pub backlog: u32,
    /// IPv6-only binding: `default`, `both`, or `ipv6-only` (`BindIPv6Only=`).
    pub bind_ipv6_only: String,
    /// Bind to a non-local address (`FreeBind=`).
    pub free_bind: bool,
    /// Enable `IP_TRANSPARENT` (`Transparent=`).
    pub transparent: bool,
    /// Enable `SO_BROADCAST` (`Broadcast=`).
    pub broadcast: bool,
    /// Pass SCM_CREDENTIALS ancillary data (`PassCredentials=`).
    pub pass_credentials: bool,
    /// Pass SCM_SECURITY ancillary data (`PassSecurity=`).
    pub pass_security: bool,
    /// Time to wait for socket activation in seconds (`TimeoutSec=`).
    pub timeout_sec: u32,
}

// ---------------------------------------------------------------------------
// [Swap] section
// ---------------------------------------------------------------------------

/// `[Swap]` section for `.swap` units.
#[derive(Debug, Clone, Serialize)]
pub struct SwapSection {
    /// Swap device or file (`What=`).
    pub what: String,
    /// Swap priority (`Priority=`; default -1 meaning kernel default).
    pub priority: i32,
    /// Options passed to `swapon(8)` (`Options=`).
    pub options: String,
    /// Time to wait for the swap operation in seconds (`TimeoutSec=`).
    pub timeout_sec: u32,
}

impl Default for SwapSection {
    fn default() -> Self {
        SwapSection {
            what: String::new(),
            priority: -1,
            options: String::new(),
            timeout_sec: 90,
        }
    }
}

// ---------------------------------------------------------------------------
// Resource control
// ---------------------------------------------------------------------------

/// Resource-control (cgroup) directives shared by unit types that own a
/// cgroup: `[Service]`, `[Slice]`, `[Scope]`, and friends.
///
/// Mirror of `systemd.resource-control(5)`. String values keep their
/// original unit-file form (e.g. `"50%"`, `"1G"`, `"100ms"`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct ResourceControl {
    /// CPU quota relative to one CPU (`CPUQuota=`), e.g. `"50%"` or `"100ms"`.
    pub cpu_quota: String,
    /// Period over which `CPUQuota` applies (`CPUQuotaPeriodSec=`).
    pub cpu_quota_period: String,
    /// Relative CPU weight, default 100 (`CPUWeight=`).
    pub cpu_weight: u32,
    /// CPU weight used during boot (`StartupCPUWeight=`).
    pub startup_cpu_weight: u32,
    /// CPU set (`CPUSetCPUs=`).
    pub cpu_set_cpus: String,
    /// Memory nodes (`CPUSetMemoryNodes=`).
    pub cpu_set_memory_nodes: String,
    /// Hard floor on memory usage (`MemoryMin=`).
    pub memory_min: String,
    /// Gentle floor on memory usage (`MemoryLow=`).
    pub memory_low: String,
    /// Soft memory limit (`MemoryHigh=`).
    pub memory_high: String,
    /// Hard memory limit (`MemoryMax=`), e.g. `"1G"`.
    pub memory_max: String,
    /// Swap usage limit (`MemorySwapMax=`).
    pub memory_swap_max: String,
    /// IO weight, default 100 (`IOWeight=`).
    pub io_weight: u32,
    /// IO weight used during boot (`StartupIOWeight=`).
    pub startup_io_weight: u32,
    /// Per-device IO weights (`IODeviceWeight=DEV WEIGHT`).
    pub io_device_weight: Vec<String>,
    /// Per-device read bandwidth limits (`IOReadBandwidthMax=DEV BYTES`).
    pub io_read_bandwidth_max: Vec<String>,
    /// Per-device write bandwidth limits (`IOWriteBandwidthMax=DEV BYTES`).
    pub io_write_bandwidth_max: Vec<String>,
    /// Maximum number of tasks/threads (`TasksMax=`), default `u32::MAX`.
    pub tasks_max: u32,
    /// CPUs this unit may run on (`AllowedCPUs=`).
    pub allowed_cpus: String,
    /// Memory nodes this unit may use (`AllowedMemoryNodes=`).
    pub allowed_memory_nodes: String,
}

// ---------------------------------------------------------------------------
// [Slice] section
// ---------------------------------------------------------------------------

/// `[Slice]` section for `.slice` units.
///
/// Slices are cgroup-based resource management units. They don't have
/// their own processes but group other units (services, scopes, etc.)
/// for resource control.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SliceSection {
    /// Resource-control directives from the `[Slice]` section.
    pub rc: ResourceControl,
}

// ---------------------------------------------------------------------------
// [Scope] section
// ---------------------------------------------------------------------------

/// `[Scope]` section for `.scope` units.
///
/// Scopes are cgroup-based units that wrap externally created processes
/// (not spawned by systemd). They are used for resource management of
/// processes started by other means.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ScopeSection {
    /// Processes to include in the scope (`PIDs=`).
    pub pids: Vec<String>,
    /// Timeout for the scope in seconds (`TimeoutStopSec=`).
    pub timeout_stop_sec: u32,
    /// Runtime timeout in seconds (`RuntimeMaxSec=`).
    pub runtime_max_sec: u32,
    /// Whether to kill all processes when the scope is stopped (`KillMode=`).
    pub kill_mode: String,
    /// Signal to send when stopping (`KillSignal=`).
    pub kill_signal: String,
    /// Whether to send SIGHUP before SIGKILL (`SendSIGHUP=`).
    pub send_sighup: bool,
    /// Resource-control directives (same directives as `[Slice]`).
    pub rc: ResourceControl,
}

// ---------------------------------------------------------------------------
// [Device] section
// ---------------------------------------------------------------------------

/// `[Device]` section for `.device` units.
///
/// Device units represent device files in `/dev/`. They are typically
/// created automatically by systemd when devices appear, but can also
/// be defined in unit files for udev rule integration.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DeviceSection {
    /// udev property name to match (`Property=`).
    pub property: Vec<String>,
    /// sysfs path to match (`SysfsPath=`).
    pub sysfs_path: String,
    /// Device name pattern (`DeviceName=`).
    pub device_name: String,
    /// Shell-style pattern for device name (`DevicePath=`).
    pub device_path: String,
}

// ---------------------------------------------------------------------------
// [Path] section
// ---------------------------------------------------------------------------

/// `[Path]` section for `.path` units.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PathSection {
    /// Activate when the path exists (`PathExists=`).
    pub path_exists: Vec<String>,
    /// Activate when a path matching the glob exists (`PathExistsGlob=`).
    pub path_exists_glob: Vec<String>,
    /// Activate when the path is created or modified (`PathChanged=`).
    pub path_changed: Vec<String>,
    /// Activate when the path is modified (`PathModified=`).
    pub path_modified: Vec<String>,
    /// Activate when the directory is non-empty (`DirectoryNotEmpty=`).
    pub directory_not_empty: Vec<String>,
    /// Unit to activate (default: same stem with `.service` suffix) (`Unit=`).
    pub unit: String,
    /// Create watched directory if missing (`MakeDirectory=`).
    pub make_directory: bool,
    /// Permission mode for auto-created directories (`DirectoryMode=`).
    pub directory_mode: String,
    /// Rate-limit interval in seconds (`TriggerLimitIntervalSec=`).
    pub trigger_limit_interval_sec: u32,
    /// Burst count for the rate limiter (`TriggerLimitBurst=`).
    pub trigger_limit_burst: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_extended_unit_kinds() {
        assert_eq!(UnitKind::from_extension("demo.socket"), UnitKind::Socket);
        assert_eq!(UnitKind::from_extension("demo.slice"), UnitKind::Slice);
        assert_eq!(UnitKind::from_extension("demo.scope"), UnitKind::Scope);
    }

    #[test]
    fn classifies_new_unit_kinds() {
        assert_eq!(UnitKind::from_extension("dev-sda1.swap"), UnitKind::Swap);
        assert_eq!(UnitKind::from_extension("watch.path"), UnitKind::Path);
        assert_eq!(UnitKind::from_extension("dev-sda.device"), UnitKind::Device);
    }

    #[test]
    fn worker_type_round_trips() {
        assert_eq!(UnitKind::Swap.worker_type(), "swap");
        assert_eq!(UnitKind::Path.worker_type(), "path");
        assert_eq!(UnitKind::Device.worker_type(), "device");
    }

    #[test]
    fn exec_command_no_prefix() {
        let cmd = ExecCommand::parse("/usr/sbin/sshd -D");
        assert_eq!(cmd.program, "/usr/sbin/sshd");
        assert_eq!(cmd.args, vec!["-D"]);
        assert!(!cmd.ignore_failure);
        assert!(!cmd.privileged);
        assert!(!cmd.no_env_lookup);
        assert!(!cmd.no_kill_on_stop);
        assert!(!cmd.no_new_privileges);
        assert_eq!(cmd.raw, "/usr/sbin/sshd -D");
    }

    #[test]
    fn exec_command_ignore_failure_prefix() {
        let cmd = ExecCommand::parse("-/usr/bin/cleanup");
        assert!(cmd.ignore_failure);
        assert_eq!(cmd.program, "/usr/bin/cleanup");
        assert!(cmd.args.is_empty());
    }

    #[test]
    fn exec_command_privileged_prefix() {
        let cmd = ExecCommand::parse("+/usr/sbin/chown root /run/x");
        assert!(cmd.privileged);
        assert_eq!(cmd.program, "/usr/sbin/chown");
        assert_eq!(cmd.args, vec!["root", "/run/x"]);
    }

    #[test]
    fn exec_command_at_prefix() {
        let cmd = ExecCommand::parse("@/usr/lib/foo/foo argv0 arg1");
        assert!(cmd.no_env_lookup);
        assert_eq!(cmd.program, "/usr/lib/foo/foo");
    }

    #[test]
    fn exec_command_no_kill_prefix() {
        let cmd = ExecCommand::parse(":/usr/bin/bar");
        assert!(cmd.no_kill_on_stop);
    }

    #[test]
    fn exec_command_no_new_privileges_prefix() {
        let cmd = ExecCommand::parse("!/usr/bin/baz");
        assert!(cmd.no_new_privileges);
        assert_eq!(cmd.program, "/usr/bin/baz");
    }

    #[test]
    fn exec_command_double_bang_prefix() {
        let cmd = ExecCommand::parse("!!/usr/bin/qux");
        assert!(cmd.no_new_privileges);
        assert_eq!(cmd.program, "/usr/bin/qux");
    }

    #[test]
    fn exec_command_combined_prefixes() {
        let cmd = ExecCommand::parse("-+/usr/bin/cmd arg1 arg2");
        assert!(cmd.ignore_failure);
        assert!(cmd.privileged);
        assert_eq!(cmd.program, "/usr/bin/cmd");
        assert_eq!(cmd.args, vec!["arg1", "arg2"]);
    }

    #[test]
    fn exec_command_empty() {
        let cmd = ExecCommand::parse("");
        assert_eq!(cmd.program, "");
        assert!(cmd.args.is_empty());
    }

    #[test]
    fn exec_command_quoted_args() {
        let cmd = ExecCommand::parse("/usr/bin/cmd 'hello world' \"foo bar\"");
        assert_eq!(cmd.program, "/usr/bin/cmd");
        assert_eq!(cmd.args, vec!["hello world", "foo bar"]);
    }

    #[test]
    fn exec_command_line_no_args() {
        let cmd = ExecCommand::parse("/usr/bin/daemon");
        assert_eq!(cmd.command_line(), "/usr/bin/daemon");
    }

    #[test]
    fn exec_command_line_with_args() {
        let cmd = ExecCommand::parse("-/usr/sbin/sshd -D -f /etc/ssh/sshd_config");
        assert_eq!(
            cmd.command_line(),
            "/usr/sbin/sshd -D -f /etc/ssh/sshd_config"
        );
    }

    #[test]
    fn shell_words_quoted() {
        let words = shell_words("foo 'bar baz' \"qux quux\"");
        assert_eq!(words, vec!["foo", "bar baz", "qux quux"]);
    }

    #[test]
    fn shell_words_escaped() {
        let words = shell_words(r"foo\ bar baz");
        assert_eq!(words, vec!["foo bar", "baz"]);
    }
}

// ---------------------------------------------------------------------------
// Serde helpers
// ---------------------------------------------------------------------------

/// `#[serde(with = "crate::types::sorted_set")]`
///
/// Serializes a `HashSet<String>` as a sorted array so that JSON/YAML dumps
/// of the allocator cache are deterministic across runs.
pub mod sorted_set {
    use super::HashSet;
    use serde::{Serialize, Serializer};

    pub fn serialize<S: Serializer>(set: &HashSet<String>, s: S) -> Result<S::Ok, S::Error> {
        let mut items: Vec<&String> = set.iter().collect();
        items.sort();
        items.serialize(s)
    }
}
