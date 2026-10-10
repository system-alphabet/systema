//! Direct cgroup v2 filesystem operations for scopes.
//!
//! System R owns the cgroup hierarchy (creation, limits, PID attach); System
//! E only *monitors* the scope's cgroup (`cgroup.events`) and *kills* its
//! processes on stop (`cgroup.procs` / `cgroup.kill`) — read/kill access
//! that does not conflict with System R's writer role.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use systema_sysr_common::unit_cgroup_path;

/// The cgroup filesystem path of a scope unit's own cgroup.
///
/// `slice` is the parent slice (empty = `system.slice`), mirroring
/// systemd's default placement for scopes.
pub fn scope_cgroup_path(slice: &str, unit_name: &str) -> String {
    let parent = if slice.is_empty() {
        "system.slice"
    } else {
        slice
    };
    unit_cgroup_path(parent, unit_name)
}

/// Read the PIDs currently in the scope's cgroup (`cgroup.procs`).
pub fn read_cgroup_procs(cgroup_path: &str) -> Result<Vec<u32>> {
    let content = fs::read_to_string(PathBuf::from(cgroup_path).join("cgroup.procs"))
        .with_context(|| {
            sysa::l10n::fmt(
                sysa::l10n::t_("cannot read {cgroup_path} cgroup.procs"),
                &[("cgroup_path", &cgroup_path.to_string())],
            )
        })?;
    Ok(content
        .lines()
        .filter_map(|l| l.trim().parse::<u32>().ok())
        .collect())
}

/// Whether the scope's cgroup is empty, from `cgroup.events` (`populated 0`).
pub fn is_cgroup_empty(cgroup_path: &str) -> bool {
    fs::read_to_string(PathBuf::from(cgroup_path).join("cgroup.events"))
        .map(|content| content.lines().any(|l| l.starts_with("populated") && l.ends_with('0')))
        .unwrap_or(true)
}

/// Whether the scope's cgroup directory exists yet (System R creates it
/// when the unit becomes active; before that we must not read it).
pub fn cgroup_exists(cgroup_path: &str) -> bool {
    PathBuf::from(cgroup_path).join("cgroup.events").exists()
}

/// Signal every process in the scope's cgroup.
pub fn signal_cgroup(cgroup_path: &str, signal: nix::sys::signal::Signal) -> Result<()> {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;

    for pid in read_cgroup_procs(cgroup_path)? {
        let _ = kill(Pid::from_raw(pid as i32), signal);
    }
    Ok(())
}

/// Kill every process in the scope's cgroup subtree via the cgroup v2
/// `cgroup.kill` file (equivalent to SIGKILL on all member processes).
pub fn kill_cgroup(cgroup_path: &str) -> Result<()> {
    fs::write(PathBuf::from(cgroup_path).join("cgroup.kill"), "1").with_context(|| {
        sysa::l10n::fmt(
            sysa::l10n::t_("cannot write {cgroup_path} cgroup.kill"),
            &[("cgroup_path", &cgroup_path.to_string())],
        )
    })
}

/// Parse a `KillSignal=` value into a `nix` signal.  Accepts `"SIGTERM"`,
/// `"TERM"`, bare numeric values; defaults to SIGTERM on empty/unparseable
/// input (systemd also falls back to the default kill signal).
pub fn parse_signal(value: &str) -> nix::sys::signal::Signal {
    use nix::sys::signal::Signal;
    use std::str::FromStr;

    let s = value.trim().to_ascii_uppercase();
    if s.is_empty() {
        return Signal::SIGTERM;
    }
    if let Some(stripped) = s.strip_prefix("SIG") {
        if let Ok(sig) = Signal::from_str(stripped) {
            return sig;
        }
    }
    if let Ok(sig) = Signal::from_str(&s) {
        return sig;
    }
    if let Ok(num) = s.parse::<i32>() {
        if let Ok(sig) = Signal::try_from(num) {
            return sig;
        }
    }
    Signal::SIGTERM
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::sys::signal::Signal;

    #[test]
    fn scope_paths() {
        assert_eq!(
            scope_cgroup_path("system.slice", "session-1.scope"),
            "/sys/fs/cgroup/system.slice/session-1.scope"
        );
        assert_eq!(
            scope_cgroup_path("", "session-1.scope"),
            "/sys/fs/cgroup/system.slice/session-1.scope"
        );
        assert_eq!(
            scope_cgroup_path("user-1000.slice", "app-1.scope"),
            "/sys/fs/cgroup/user.slice/1000.slice/app-1.scope"
        );
    }

    #[test]
    fn signal_parsing() {
        assert_eq!(parse_signal(""), Signal::SIGTERM);
        assert_eq!(parse_signal("SIGTERM"), Signal::SIGTERM);
        assert_eq!(parse_signal("TERM"), Signal::SIGTERM);
        assert_eq!(parse_signal("SIGKILL"), Signal::SIGKILL);
        assert_eq!(parse_signal("9"), Signal::SIGKILL);
        assert_eq!(parse_signal("SIGHUP"), Signal::SIGHUP);
        assert_eq!(parse_signal("not-a-signal"), Signal::SIGTERM);
        assert_eq!(parse_signal("SIGTERM"), Signal::SIGTERM);
    }
}
