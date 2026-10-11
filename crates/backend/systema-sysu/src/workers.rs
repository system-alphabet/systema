//! The per-user process set for `systema-sysu`.
//!
//! P1 scope (plan §3.2): **service + target units only**, so the launcher
//! supervises System A plus the Service and Target workers — and System W,
//! the D-Bus bridge that gives the session a real `systemctl --user`
//! surface (its bus — and syss's `Type=dbus` watch — is steered to the
//! session bus by the `--bus-address` argument passed at spawn, plan 备注
//! 11).  System R/M/D (system-domain duties) and System C/K/N/E (P2+) are
//! not part of the user domain, and there is no `.power` registration at
//! all — reboot/halt are system-domain actions.
//!
//! Binary resolution follows the SysI recipe: an explicit `--bin-dir`
//! (authoritative), then the directory of the `systema-sysu` executable
//! itself, then the canonical install directories, then `PATH`.

/// A supervised child process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerSpec {
    /// Canonical short name (`"syss"`), used in log lines.
    pub name: &'static str,
    /// Executable name to spawn.
    pub binary: &'static str,
    /// Worker ID registered with System A (`None` for System A itself).
    pub worker_id: Option<&'static str>,
    /// Extra command-line arguments.
    pub args: &'static [&'static str],
}

impl WorkerSpec {
    const fn new(
        name: &'static str,
        binary: &'static str,
        worker_id: Option<&'static str>,
    ) -> Self {
        Self {
            name,
            binary,
            worker_id,
            args: &[],
        }
    }
}

/// The per-user process set, in spawn order: System A first (the launcher
/// waits for its `MANAGER_READY`), then the workers serially, each gated
/// on `WORKER_READY=<id>` (System S, System T, then System W — System A
/// broadcasts W's ready event when the bridge's control `manager.hello`
/// carries `system-w-1`, so D-Bus availability never gates startup).
pub fn default_workers() -> Vec<WorkerSpec> {
    vec![
        WorkerSpec::new("sysa", "systema-sysa", None),
        WorkerSpec::new("syss", "systema-syss", Some("system-s-1")),
        WorkerSpec::new("syst", "systema-syst", Some("system-t-1")),
        // System W serves org.freedesktop.systemd1 for this session's
        // `systemctl --user`, and syss's Type=dbus watch runs there too —
        // both get the session-bus address via `--bus-address` at spawn
        // (supervise.rs, plan 备注 11).
        WorkerSpec::new("sysw", "systema-sysw.systemd", Some("system-w-1")),
    ]
}

/// A resolved (spawnable) supervised process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProcess {
    pub spec: WorkerSpec,
    pub path: std::path::PathBuf,
}

fn executable_exists(path: &std::path::Path) -> bool {
    nix::unistd::access(path, nix::unistd::AccessFlags::X_OK).is_ok()
}

/// First directory in `dirs` that contains an executable `name`.
fn search_in(dirs: &[std::path::PathBuf], name: &str) -> Option<std::path::PathBuf> {
    for dir in dirs {
        let path = dir.join(name);
        if executable_exists(&path) {
            return Some(path);
        }
    }
    None
}

/// Resolve a binary to an executable path.
///
/// Search order: an explicit `--bin-dir` (authoritative when given), then
/// the directory of the `systema-sysu` executable itself, then the
/// canonical systema install directories (`SYSTEMA_BIN_PATH`), then
/// `PATH`.
pub fn resolve_binary(
    binary: &str,
    bin_dir: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    if let Some(dir) = bin_dir {
        let path = dir.join(binary);
        return executable_exists(&path).then_some(path);
    }
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            dirs.push(dir.to_path_buf());
        }
    }
    for dir in &sysa::paths::instance().systema_bin_search_paths {
        dirs.push(std::path::PathBuf::from(dir));
    }
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    search_in(&dirs, binary)
}

/// Resolve every spec in the set, reporting which ones are missing.
pub fn resolve_set(
    set: &[WorkerSpec],
    bin_dir: Option<&std::path::Path>,
) -> (Vec<ResolvedProcess>, Vec<WorkerSpec>) {
    let mut resolved = Vec::new();
    let mut missing = Vec::new();
    for spec in set {
        match resolve_binary(spec.binary, bin_dir) {
            Some(path) => resolved.push(ResolvedProcess {
                spec: spec.clone(),
                path,
            }),
            None => missing.push(spec.clone()),
        }
    }
    (resolved, missing)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(set: &[WorkerSpec]) -> Vec<&str> {
        set.iter().map(|s| s.name).collect()
    }

    #[test]
    fn user_domain_set_is_sysa_syss_syst_sysw() {
        let set = default_workers();
        assert_eq!(names(&set), vec!["sysa", "syss", "syst", "sysw"]);
        // The P1 subset is service + target, plus System W for the
        // session's `systemctl --user` surface: no .power (there is no
        // power worker at all), no other system-domain workers.
        assert_eq!(set[1].worker_id, Some("system-s-1"));
        assert_eq!(set[2].worker_id, Some("system-t-1"));
        assert_eq!(set[3].worker_id, Some("system-w-1"));
        assert_eq!(set[3].binary, "systema-sysw.systemd");
        assert_eq!(set[0].worker_id, None);
    }

    fn shim_dir(tag: &str) -> std::path::PathBuf {
        use std::fs;
        let dir = std::env::temp_dir().join(format!("sysa-sysu-unit-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn bin_dir_is_authoritative() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        let dir = shim_dir("bin-dir");
        fs::write(dir.join("systema-syss"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(dir.join("systema-syss"), fs::Permissions::from_mode(0o755)).unwrap();

        let hit = resolve_binary("systema-syss", Some(&dir)).unwrap();
        assert_eq!(hit, dir.join("systema-syss"));

        // An explicit bin-dir does not fall back to PATH / exe dir.
        assert!(resolve_binary("systema-sysa", Some(&dir)).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_in_finds_executables_only() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        let dir = shim_dir("search-in");
        fs::write(dir.join("runme"), "#!/bin/sh\n").unwrap();
        fs::write(dir.join("not-run"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(dir.join("runme"), fs::Permissions::from_mode(0o755)).unwrap();

        let dirs = vec![dir.clone()];
        assert_eq!(search_in(&dirs, "runme"), Some(dir.join("runme")));
        assert!(search_in(&dirs, "not-run").is_none());
        assert!(search_in(&dirs, "absent").is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_set_partitions_resolved_and_missing() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        let dir = shim_dir("resolve-set");
        fs::write(dir.join("systema-sysa"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(dir.join("systema-sysa"), fs::Permissions::from_mode(0o755)).unwrap();

        let set = default_workers();
        let (resolved, missing) = resolve_set(&set, Some(&dir));
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].spec.name, "sysa");
        assert_eq!(missing.len(), 3);
        assert!(missing.iter().any(|m| m.name == "syss"));
        assert!(missing.iter().any(|m| m.name == "syst"));
        assert!(missing.iter().any(|m| m.name == "sysw"));
        let _ = fs::remove_dir_all(&dir);
    }
}
