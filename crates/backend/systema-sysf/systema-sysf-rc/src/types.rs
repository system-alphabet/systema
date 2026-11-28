//! Data model for SysV init (`rc`) scripts.

/// Source-format tag reported in every produced [`UnitIR`](systema_sysf::ir::UnitIR).
pub const SOURCE_FORMAT: &str = "sysv";

/// One discovered init script: its LSB header and the runlevel links found
/// on disk.
#[derive(Debug, Clone, Default)]
pub struct InitScript {
    /// Canonical name stem, i.e. the file name without a `.sh` suffix
    /// (`sshd`).
    pub name: String,
    /// Path of the script file.
    pub path: String,
    /// Whether the script carries a `### BEGIN INIT INFO` block.
    pub has_lsb: bool,
    /// `Provides:` words, raw (`$network`, `ssh`, ...).
    pub provides: Vec<String>,
    /// `Required-Start:` facilities and services.
    pub required_start: Vec<String>,
    /// `Required-Stop:` facilities and services.
    pub required_stop: Vec<String>,
    /// `Should-Start:` facilities and services.
    pub should_start: Vec<String>,
    /// `Should-Stop:` facilities and services.
    pub should_stop: Vec<String>,
    /// `X-Start-Before:` unit names.
    pub x_start_before: Vec<String>,
    /// `X-Start-After:` unit names.
    pub x_start_after: Vec<String>,
    /// `Default-Start:` runlevels (`2 3 4 5`).
    pub default_start: Vec<String>,
    /// `Default-Stop:` runlevels (`0 1 6`).
    pub default_stop: Vec<String>,
    /// Chosen description: `Short-Description:` > Red Hat `# description:`
    /// > `Description:`.
    pub description: String,
    /// Red Hat `# pidfile:` header value; empty when absent.
    pub pid_file: String,
    /// Whether the script's usage text advertises a `reload` action.
    pub supports_reload: bool,
    /// Highest `Snn` priority seen in the start-up runlevel directories.
    pub start_priority: Option<u32>,
    /// Whether an `S` link enables the script for a start-up runlevel.
    pub enabled: bool,
    /// Start-up runlevels naming this script in their `rcN.d` directory.
    pub link_runlevels: Vec<char>,
}

impl InitScript {
    pub fn new(name: impl Into<String>, path: impl Into<String>) -> Self {
        InitScript {
            name: name.into(),
            path: path.into(),
            ..Default::default()
        }
    }

    /// The canonical unit name produced for this script (`sshd.service`).
    pub fn unit_name(&self) -> String {
        format!("{}.service", self.name)
    }
}

/// The systemd target equivalent of a SysV runlevel.
///
/// 0 powers off, 1 (and `S`/`s`, single-user mode) drops to rescue, 2-4 are
/// the multi-user runlevels, 5 adds the graphical session, 6 reboots.
pub fn runlevel_target(runlevel: char) -> Option<&'static str> {
    match runlevel {
        '0' => Some("poweroff.target"),
        '1' | 'S' | 's' => Some("rescue.target"),
        '2' | '3' | '4' => Some("multi-user.target"),
        '5' => Some("graphical.target"),
        '6' => Some("reboot.target"),
        _ => None,
    }
}

/// Whether `runlevel` is a boot-time runlevel: membership in one of these
/// is enablement, and its links order the script before the runlevel target.
/// The shutdown runlevels (0 and 6) are handled by the default shutdown
/// dependencies every service gets, exactly like systemd's SysV generator.
pub fn is_startup_runlevel(runlevel: char) -> bool {
    matches!(runlevel, '1' | '2' | '3' | '4' | '5' | 'S' | 's')
}

/// The runlevel named by an `rcN.d` directory (`rc3.d` → `3`).
pub fn rc_dir_runlevel(dir_name: &str) -> Option<char> {
    let stem = dir_name.strip_prefix("rc")?.strip_suffix(".d")?;
    let mut chars = stem.chars();
    let runlevel = chars.next()?;
    chars.next().is_none().then_some(runlevel)
}
