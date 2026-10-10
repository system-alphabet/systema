//! Power-transition management for System Init.
//!
//! System Init owns the `power` unit type end-to-end:
//!
//! * [`PowerAction`] encodes a `.power` unit name into a system transition.
//! * [`synthesize_power_definitions`] builds the [`UnitIR`] definitions that
//!   System Init registers with System A at boot (System A never needs a
//!   `power` worker).
//! * [`execute_action`] performs the real `reboot(2)` transition in-process
//!   - terminal transitions (poweroff/reboot/halt/kexec) never return.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use anyhow::Result;
use tracing::debug;

use systema_sysf::ir::{UnitIR, UnitType};

/// Policy controlling whether System Init may drive power transitions.
///
/// This governs whether the in-process `reboot(2)` path is actually invoked.
/// A non-PID-1 System Init (e.g. inside a container or during development)
/// should never accidentally reboot the host, so the default (`Auto`) gates
/// the transition on being PID 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PowerCtl {
    /// Control power only when running as PID 1.
    #[default]
    Auto,
    /// Always attempt power transitions regardless of PID.
    Always,
    /// Never perform power transitions.
    Never,
}

impl PowerCtl {
    /// Whether power control is enabled given the runtime policy and whether
    /// System Init is running as PID 1.
    pub fn enabled(self, is_pid_one: bool) -> bool {
        match self {
            PowerCtl::Auto => is_pid_one,
            PowerCtl::Always => true,
            PowerCtl::Never => false,
        }
    }
}

impl fmt::Display for PowerCtl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            PowerCtl::Auto => "auto",
            PowerCtl::Always => "always",
            PowerCtl::Never => "never",
        };
        f.write_str(s)
    }
}

impl FromStr for PowerCtl {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "auto" => Ok(PowerCtl::Auto),
            "always" => Ok(PowerCtl::Always),
            "never" => Ok(PowerCtl::Never),
            _ => anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_("unknown powerctl policy: {s} (expected auto, always, never)"),
                &[("s", &format!("{:?}", s))]
            )),
        }
    }
}

/// The power transition requested by a `.power` unit. The action is encoded
/// in the unit name (e.g. `poweroff.power`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerAction {
    Poweroff,
    Reboot,
    Halt,
    Kexec,
    Suspend,
    Hibernate,
}

impl PowerAction {
    /// Parse a power action from a unit name, stripping the `.power`
    /// extension.  Unknown names resolve to `None`.
    pub fn from_unit_name(unit_name: &str) -> Option<Self> {
        let stem = unit_name
            .strip_suffix(".power")
            .or_else(|| unit_name.split_once(".power").map(|(head, _)| head))
            .unwrap_or(unit_name);
        match stem {
            "poweroff" => Some(PowerAction::Poweroff),
            "halt" => Some(PowerAction::Halt),
            "kexec" => Some(PowerAction::Kexec),
            "reboot" => Some(PowerAction::Reboot),
            "suspend" => Some(PowerAction::Suspend),
            "hibernate" => Some(PowerAction::Hibernate),
            _ => None,
        }
    }
}

impl fmt::Display for PowerAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            PowerAction::Poweroff => "poweroff",
            PowerAction::Reboot => "reboot",
            PowerAction::Halt => "halt",
            PowerAction::Kexec => "kexec",
            PowerAction::Suspend => "suspend",
            PowerAction::Hibernate => "hibernate",
        };
        f.write_str(s)
    }
}

impl FromStr for PowerAction {
    type Err = anyhow::Error;

    /// Parse an action from its short ("reboot") or unit-name
    /// ("reboot.power") form.  Unknown names are an error.
    fn from_str(s: &str) -> Result<Self> {
        Self::from_unit_name(s).ok_or_else(|| {
            anyhow::anyhow!(sysa::l10n::fmt(
                sysa::l10n::t_("unknown power action: {s}"),
                &[("s", &format!("{:?}", s))]
            ))
        })
    }
}

/// The description used for the given power action (best effort).
fn power_description(action: PowerAction) -> String {
    match action {
        PowerAction::Poweroff => sysa::l10n::t_("System Power Off").to_string(),
        PowerAction::Reboot => sysa::l10n::t_("System Reboot").to_string(),
        PowerAction::Halt => sysa::l10n::t_("System Halt").to_string(),
        PowerAction::Kexec => sysa::l10n::t_("Reboot via kexec").to_string(),
        PowerAction::Suspend => sysa::l10n::t_("System Suspend").to_string(),
        PowerAction::Hibernate => sysa::l10n::t_("System Hibernate").to_string(),
    }
}

/// Build the minimal `UnitIR` of a `.power` unit definition.
fn power_unit_ir(unit_name: &str) -> Option<UnitIR> {
    let action = PowerAction::from_unit_name(unit_name)?;
    Some(UnitIR {
        id: unit_name.to_string(),
        unit_type: Some(UnitType::Power),
        description: Some(power_description(action)),
        source_format: Some("dynamic".to_string()),
        source_path: None,
        aliases: Vec::new(),
        slice: None,
        dependencies: None,
        service: None,
        mount: None,
        automount: None,
        timer: None,
        socket: None,
        resource_control: None,
        conditions: None,
        asserts: None,
        wanted_by: None,
        required_by: None,
    })
}

/// Synthesize the definitions of every requested `.power` unit name.
///
/// Returns `None` when **any** requested name is not a legal `.power` unit
/// ("poweroff", "reboot", "halt", "kexec", "suspend", "hibernate"; the
/// `.power` suffix is optional).
pub fn synthesize_power_definitions(unit_names: &[String]) -> Option<HashMap<String, UnitIR>> {
    if unit_names.is_empty() || unit_names.iter().any(|n| n.is_empty()) {
        return None;
    }
    let mut units: HashMap<String, UnitIR> = HashMap::new();
    for name in unit_names {
        let ir = power_unit_ir(name)?;
        units.insert(ir.id.clone(), ir);
    }
    debug!(
        "synthesized {} built-in .power unit definition(s): {:?}",
        units.len(),
        unit_names
    );
    Some(units)
}

/// The definitions of every canonical `.power` unit, keyed by unit name.
/// System Init registers these with System A during the control phase.
pub fn all_power_definitions() -> HashMap<String, UnitIR> {
    let names: Vec<String> = [
        "poweroff.power",
        "reboot.power",
        "halt.power",
        "kexec.power",
        "suspend.power",
        "hibernate.power",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    synthesize_power_definitions(&names).expect("canonical power names are all legal")
}

/// Execute a power transition using the host's default backend (the Linux
/// backend on Linux, the no-op backend elsewhere).  The `policy` gates
/// whether the transition is actually attempted: `Auto` requires PID 1,
/// `Always` bypasses the check, and `Never` refuses every transition.
///
/// Public entry point for in-process callers such as System Init.
pub fn execute_action(action: PowerAction, policy: PowerCtl) -> Result<()> {
    let is_pid_one = std::process::id() == 1;
    if !policy.enabled(is_pid_one) {
        anyhow::bail!(sysa::l10n::fmt(
            sysa::l10n::t_(
                "power transition '{action}' refused by --powerctl={policy} (pid {pid})"
            ),
            &[
                ("action", &action.to_string()),
                ("policy", &policy.to_string()),
                ("pid", &(std::process::id()).to_string())
            ]
        ));
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        LinuxPowerController.execute(action)
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        NoopController.execute(action)
    }
}

/// Backend for performing system power transitions.
///
/// Implementations are expected to be cheap and idempotent.  The Linux
/// backend performs the real `reboot(2)` transition; the no-op backend
/// reports the transition as unsupported so the unit is left inert.
pub trait PowerController: Send + Sync {
    /// Execute a power transition.
    ///
    /// On the Linux backend this calls the libc `reboot(2)` system call,
    /// which **does not return** on success (the system goes down).  The
    /// no-op backend returns an error reporting that no power backend is
    /// available.
    fn execute(&self, action: PowerAction) -> Result<()>;
}

/// The Linux `reboot(2)` backend.
///
/// `execute()` calls `libc::reboot` with the appropriate
/// `LINUX_REBOOT_CMD_*` constant; on success it does **not return** (the
/// machine goes down).  If the caller still observes a return value it means
/// the transition failed (an error result).
#[cfg(any(target_os = "linux", target_os = "android"))]
#[derive(Debug, Clone, Copy, Default)]
pub struct LinuxPowerController;

#[cfg(any(target_os = "linux", target_os = "android"))]
impl PowerController for LinuxPowerController {
    fn execute(&self, action: PowerAction) -> Result<()> {
        let cmd = match action {
            PowerAction::Poweroff => libc::LINUX_REBOOT_CMD_POWER_OFF,
            PowerAction::Reboot => libc::LINUX_REBOOT_CMD_RESTART,
            PowerAction::Halt => libc::LINUX_REBOOT_CMD_HALT,
            PowerAction::Kexec => libc::LINUX_REBOOT_CMD_KEXEC,
            PowerAction::Suspend => libc::LINUX_REBOOT_CMD_SW_SUSPEND,
            PowerAction::Hibernate => libc::LINUX_REBOOT_CMD_SW_SUSPEND,
        };

        // Calling reboot(2) requires CAP_SYS_BOOT or effective root.  A
        // non-zero return here means the transition failed (e.g. not enough
        // privilege), since a successful reboot never returns.
        //
        // Issued as syscall(2) instead of libc's `reboot(3)`: supplying the
        // two magic words is exactly what glibc's and bionic's wrappers do,
        // and `libc` only declares `reboot()` for linux-gnu — android has
        // `SYS_reboot` and the magic constants but no declaration.
        let ret = unsafe {
            libc::syscall(
                libc::SYS_reboot,
                libc::LINUX_REBOOT_MAGIC1,
                libc::LINUX_REBOOT_MAGIC2,
                cmd,
                0,
            )
        };
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            return Err(anyhow::anyhow!(sysa::l10n::fmt(sysa::l10n::t_("reboot(2) for power action '{action}' failed: {err} (are we running as root/CAP_SYS_BOOT?)"), &[("action", &action.to_string()), ("err", &err.to_string())])));
        }
        Ok(())
    }
}

/// A controller that refuses every transition.
///
/// Used on platforms without a power backend, so `execute_action` still has
/// something to call.  On Linux nothing constructs it — `LinuxPowerController`
/// is compiled in instead — hence the `allow`.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopController;

impl PowerController for NoopController {
    fn execute(&self, action: PowerAction) -> Result<()> {
        anyhow::bail!(sysa::l10n::fmt(
            sysa::l10n::t_("power action '{action}' not supported: no power backend available"),
            &[("action", &action.to_string())]
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_action_from_unit_name() {
        assert_eq!(
            PowerAction::from_unit_name("poweroff.power"),
            Some(PowerAction::Poweroff)
        );
        assert_eq!(
            PowerAction::from_unit_name("reboot.power"),
            Some(PowerAction::Reboot)
        );
        assert_eq!(
            PowerAction::from_unit_name("halt.power"),
            Some(PowerAction::Halt)
        );
        assert_eq!(
            PowerAction::from_unit_name("kexec.power"),
            Some(PowerAction::Kexec)
        );
        assert_eq!(
            PowerAction::from_unit_name("suspend.power"),
            Some(PowerAction::Suspend)
        );
        assert_eq!(
            PowerAction::from_unit_name("hibernate.power"),
            Some(PowerAction::Hibernate)
        );
    }

    #[test]
    fn parses_plain_action_names() {
        assert_eq!(
            PowerAction::from_unit_name("poweroff"),
            Some(PowerAction::Poweroff)
        );
        assert_eq!(
            PowerAction::from_unit_name("reboot"),
            Some(PowerAction::Reboot)
        );
    }

    #[test]
    fn rejects_unknown_names() {
        assert_eq!(PowerAction::from_unit_name("evil.power"), None);
        assert_eq!(PowerAction::from_unit_name("systemd-poweroff.service"), None);
        assert_eq!(PowerAction::from_unit_name(""), None);
    }

    #[test]
    fn display_round_trips() {
        for action in [
            PowerAction::Poweroff,
            PowerAction::Reboot,
            PowerAction::Halt,
            PowerAction::Kexec,
            PowerAction::Suspend,
            PowerAction::Hibernate,
        ] {
            assert_eq!(PowerAction::from_unit_name(&action.to_string()), Some(action));
        }
    }

    #[test]
    fn noop_controller_refuses_every_transition() {
        assert!(
            NoopController.execute(PowerAction::Poweroff).is_err(),
            "no-op backend must refuse every transition"
        );
    }

    #[test]
    fn synthesizes_poweroff_unit() {
        let units =
            synthesize_power_definitions(&["poweroff.power".to_string()]).expect("must synthesize");
        assert_eq!(units.len(), 1);
        let ir = units.get("poweroff.power").expect("unit present");
        assert_eq!(ir.unit_type, Some(UnitType::Power));
        assert_eq!(ir.description.as_deref(), Some("System Power Off"));
        assert_eq!(ir.source_format.as_deref(), Some("dynamic"));
    }

    #[test]
    fn synthesizes_all_actions() {
        let names: Vec<String> = [
            "poweroff.power",
            "reboot.power",
            "halt.power",
            "kexec.power",
            "suspend.power",
            "hibernate.power",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let units = synthesize_power_definitions(&names).expect("must synthesize");
        assert_eq!(units.len(), names.len());
        for name in &names {
            let ir = units.get(name).expect("unit present");
            assert_eq!(ir.unit_type, Some(UnitType::Power));
        }
    }

    #[test]
    fn refuses_non_power_names() {
        assert!(synthesize_power_definitions(&[]).is_none());
        assert!(synthesize_power_definitions(&["systemd-poweroff.service".to_string()]).is_none());
        assert!(
            synthesize_power_definitions(&["poweroff.power".to_string(), "evil.power".to_string()])
                .is_none(),
            "a single bad name must refuse the whole batch"
        );
    }

    #[test]
    fn all_definitions_registers_six_units() {
        let units = all_power_definitions();
        assert_eq!(units.len(), 6);
        for name in ["poweroff.power", "reboot.power", "halt.power", "kexec.power", "suspend.power", "hibernate.power"] {
            assert!(units.contains_key(name), "missing {name}");
        }
    }

    #[test]
    fn powerctl_auto_requires_pid_one() {
        assert!(PowerCtl::Auto.enabled(true));
        assert!(!PowerCtl::Auto.enabled(false));
    }

    #[test]
    fn powerctl_always_enabled() {
        assert!(PowerCtl::Always.enabled(true));
        assert!(PowerCtl::Always.enabled(false));
    }

    #[test]
    fn powerctl_never_disabled() {
        assert!(!PowerCtl::Never.enabled(true));
        assert!(!PowerCtl::Never.enabled(false));
    }

    #[test]
    fn powerctl_from_str_round_trips() {
        for variant in [PowerCtl::Auto, PowerCtl::Always, PowerCtl::Never] {
            let s = variant.to_string();
            assert_eq!(s.parse::<PowerCtl>().unwrap(), variant);
        }
    }

    #[test]
    fn powerctl_from_str_rejects_unknown() {
        assert!("bogus".parse::<PowerCtl>().is_err());
    }
}