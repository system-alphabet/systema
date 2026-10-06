//! Supervised process registry for SysAInit.
//!
//! SysAInit does exactly one thing: spawn System A and the System Workers,
//! and supervise them until they exit.  This module owns the *set* of
//! processes to spawn: the default collection, `--skip-workers` trimming,
//! platform gating, and the one-shot finder chain.

/// Supervision kind for a child process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessKind {
    /// Long-running process: its unexpected exit terminates SysAInit.
    LongRunning,
    /// One-shot process: exiting on its own is expected and does not
    /// terminate SysAInit.
    OneShot,
}

/// A supervised child process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerSpec {
    /// Canonical short name used in `--skip-workers` (e.g. `"syss"`).
    pub name: &'static str,
    /// Executable name to spawn; when `linux_binary` is present it replaces
    /// `binary` on Linux platforms.
    pub binary: &'static str,
    /// Linux-specific executable name that replaces `binary` on Linux.
    pub linux_binary: Option<&'static str>,
    /// Worker ID registered with System A (`None` for System A itself).
    pub worker_id: Option<&'static str>,
    /// Supervision kind.
    pub kind: ProcessKind,
    /// Only spawned on Linux (dropped on other platforms).
    pub linux_only: bool,
    /// Extra command-line arguments.
    pub args: &'static [&'static str],
}

impl WorkerSpec {
    fn new(
        name: &'static str,
        binary: &'static str,
        worker_id: Option<&'static str>,
        kind: ProcessKind,
        linux_only: bool,
    ) -> Self {
        Self {
            name,
            binary,
            worker_id,
            kind,
            linux_only,
            linux_binary: None,
            args: &[],
        }
    }

    fn matches_name(&self, s: &str) -> bool {
        s == self.name || s == self.binary || self.linux_binary == Some(s)
    }
}

/// The default set of long-running processes.
///
/// System M ships a dedicated `.linux` flavor and is dropped off Linux.
/// System P has been removed: System Init manages the `power` unit type
/// directly (in-process `reboot(2)`) and no `power` worker is supervised.
fn default_workers() -> Vec<WorkerSpec> {
    vec![
        WorkerSpec::new(
            "sysa",
            "systema-sysa",
            None,
            ProcessKind::LongRunning,
            false,
        ),
        WorkerSpec::new(
            "syss",
            "systema-syss",
            Some("system-s-1"),
            ProcessKind::LongRunning,
            false,
        ),
        WorkerSpec::new(
            "syse",
            "systema-syse",
            Some("system-e-1"),
            ProcessKind::LongRunning,
            false,
        ),
        WorkerSpec::new(
            "syst",
            "systema-syst",
            Some("system-t-1"),
            ProcessKind::LongRunning,
            false,
        ),
        WorkerSpec::new(
            "sysc",
            "systema-sysc",
            Some("system-c-1"),
            ProcessKind::LongRunning,
            false,
        ),
        WorkerSpec::new(
            "sysk",
            "systema-sysk",
            Some("system-k-1"),
            ProcessKind::LongRunning,
            false,
        ),
        WorkerSpec::new(
            "sysn",
            "systema-sysn",
            Some("system-n-1"),
            ProcessKind::LongRunning,
            false,
        ),
        WorkerSpec::new(
            "sysd",
            "systema-sysd",
            Some("system-d-1"),
            ProcessKind::LongRunning,
            false,
        ),
        WorkerSpec::new(
            "sysr",
            "systema-sysr",
            Some("system-r-1"),
            ProcessKind::LongRunning,
            false,
        ),
        WorkerSpec::new(
            "sysm",
            "systema-sysm.linux",
            Some("system-m-1"),
            ProcessKind::LongRunning,
            true,
        ),
        WorkerSpec::new(
            "sysw",
            "systema-sysw.systemd",
            Some("system-w-1"),
            ProcessKind::LongRunning,
            true,
        ),
    ]
}

/// Platform abstraction so tests can simulate non-Linux hosts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Linux,
    Other,
}

impl Platform {
    pub fn current() -> Self {
        if cfg!(any(target_os = "linux", target_os = "android")) {
            Platform::Linux
        } else {
            Platform::Other
        }
    }
}

/// Build the supervised process set.
///
/// `skip` accepts canonical short names (`"sysd"`) or full binary names
/// (`"systema-sysd"`, `"systema-sysm.linux"`); unknown names are an error.
/// Skipping `"sysf"` removes the whole finder chain.  Platform-gated
/// workers are dropped on non-Linux.
pub fn build_worker_set(skip: &[String]) -> anyhow::Result<Vec<WorkerSpec>> {
    build_worker_set_for(Platform::current(), skip)
}

pub fn build_worker_set_for(
    platform: Platform,
    skip: &[String],
) -> anyhow::Result<Vec<WorkerSpec>> {
    let all: Vec<WorkerSpec> = default_workers();

    let mut retained: Vec<WorkerSpec> = Vec::with_capacity(all.len());
    for spec in all {
        if spec.linux_only && platform != Platform::Linux {
            continue;
        }
        if skip.iter().any(|s| spec.matches_name(s)) {
            continue;
        }
        // Pick the executable for this platform so downstream resolution
        // and logging always see the concrete binary.
        let mut spec = spec;
        if platform == Platform::Linux {
            if let Some(linux_binary) = spec.linux_binary {
                spec.binary = linux_binary;
            }
        }
        spec.linux_binary = None;
        retained.push(spec);
    }

    if let Some(unknown) = skip
        .iter()
        .find(|s| !default_workers().iter().any(|spec| spec.matches_name(s)))
    {
        anyhow::bail!(sysa::l10n::fmt(
            sysa::l10n::t_("unknown worker name '{unknown}'"),
            &[("unknown", &unknown.to_string())]
        ));
    }

    Ok(retained)
}

/// A resolved (spawnable) supervised process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProcess {
    pub spec: WorkerSpec,
    pub path: std::path::PathBuf,
}

#[cfg(unix)]
fn executable_exists(path: &std::path::Path) -> bool {
    nix::unistd::access(path, nix::unistd::AccessFlags::X_OK).is_ok()
}

#[cfg(not(unix))]
fn executable_exists(path: &std::path::Path) -> bool {
    path.is_file()
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
/// the directory of the SysAInit executable itself, then the canonical
/// systema install directories (`sysa::paths::systema_bin_search_paths`),
/// then `PATH`.
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

    fn skip(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn names(set: &[WorkerSpec]) -> Vec<&str> {
        set.iter().map(|s| s.name).collect()
    }

    #[test]
    fn platform_selects_binaries() {
        let linux = build_worker_set_for(Platform::Linux, &[]).unwrap();
        assert_eq!(
            names(&linux),
            vec!["sysa", "syss", "syse", "syst", "sysc", "sysk", "sysn", "sysd", "sysr", "sysm", "sysw"]
        );
        assert_eq!(linux.len(), 11);
        assert!(
            linux.iter().all(|s| s.kind == ProcessKind::LongRunning)
        );
        fn binary_of<'a>(set: &'a [WorkerSpec], name: &'a str) -> &'a str {
            set.iter().find(|s| s.name == name).unwrap().binary
        }
        // System M / System W exist only on Linux.
        assert_eq!(binary_of(&linux, "sysm"), "systema-sysm.linux");
        assert_eq!(binary_of(&linux, "sysw"), "systema-sysw.systemd");
        // System R carries the plain name on every platform — unlike M and W
        // it has no per-platform suffix, so this is the name that must exist
        // as a build artifact.
        assert_eq!(binary_of(&linux, "sysr"), "systema-sysr");

        let other = build_worker_set_for(Platform::Other, &[]).unwrap();
        assert_eq!(
            names(&other),
            vec!["sysa", "syss", "syse", "syst", "sysc", "sysk", "sysn", "sysd", "sysr"]
        );
        assert_eq!(binary_of(&other, "sysr"), "systema-sysr");
    }

    #[test]
    fn skip_accepts_short_names() {
        let set = build_worker_set_for(Platform::Linux, &skip(&["sysd", "sysc"])).unwrap();
        assert_eq!(
            names(&set),
            vec!["sysa", "syss", "syse", "syst", "sysk", "sysn", "sysr", "sysm", "sysw"]
        );
    }

    #[test]
    fn skip_accepts_full_binary_names() {
        let set = build_worker_set_for(
            Platform::Linux,
            &skip(&["systema-sysd", "systema-sysm.linux"]),
        )
        .unwrap();
        assert_eq!(
            names(&set),
            vec!["sysa", "syss", "syse", "syst", "sysc", "sysk", "sysn", "sysr", "sysw"]
        );
    }

    #[test]
    fn skipping_sysa_removes_the_core() {
        let set = build_worker_set_for(Platform::Linux, &skip(&["sysa"])).unwrap();
        assert!(!names(&set).contains(&"sysa"));
    }

    #[test]
    fn sysf_not_in_worker_set() {
        let set = build_worker_set_for(Platform::Linux, &[]).unwrap();
        assert!(!names(&set).contains(&"sysf"));
    }

    #[test]
    fn unknown_skip_name_is_an_error() {
        let err = build_worker_set_for(Platform::Linux, &skip(&["nope"])).unwrap_err();
        assert!(err.to_string().contains("nope"));
    }

    fn shim_dir(tag: &str) -> std::path::PathBuf {
        use std::fs;
        let dir = std::env::temp_dir().join(format!("sysa-sysi-unit-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn bin_dir_is_authoritative() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        let dir = shim_dir("bin-dir");
        fs::write(dir.join("systema-sysd"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(dir.join("systema-sysd"), fs::Permissions::from_mode(0o755)).unwrap();

        let hit = resolve_binary("systema-sysd", Some(&dir)).unwrap();
        assert_eq!(hit, dir.join("systema-sysd"));

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
        let dir = shim_dir("resolve-set");
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        fs::write(dir.join("systema-sysa"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(dir.join("systema-sysa"), fs::Permissions::from_mode(0o755)).unwrap();

        let set = build_worker_set_for(Platform::Linux, &[]).unwrap();
        let (resolved, missing) = resolve_set(&set, Some(&dir));
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].spec.name, "sysa");
        assert_eq!(missing.len(), 10);
        assert!(missing.iter().any(|m| m.name == "syss"));
        assert!(missing.iter().any(|m| m.name == "sysw"));
        let _ = fs::remove_dir_all(&dir);
    }

    /// Directory paths listed in the root workspace `members = [ ... ]`.
    fn workspace_members(root: &std::path::Path) -> std::collections::HashSet<String> {
        let text = std::fs::read_to_string(root.join("Cargo.toml")).expect("root Cargo.toml");
        let mut members = std::collections::HashSet::new();
        let mut in_list = false;
        for line in text.lines() {
            let line = line.trim();
            if in_list {
                if line.starts_with(']') {
                    in_list = false;
                } else if let Some(rest) = line.strip_prefix('"') {
                    if let Some(end) = rest.find('"') {
                        members.insert(rest[..end].to_string());
                    }
                }
            } else if line.starts_with("members") && line.contains('[') {
                in_list = !line.contains(']');
            }
        }
        members
    }

    /// Directories under `dir` that are crates with a binary of their own.
    fn collect_bin_crates(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || name == "target" {
                continue;
            }
            if path.join("Cargo.toml").is_file() && path.join("src/main.rs").is_file() {
                out.push(
                    path.strip_prefix(root)
                        .expect("crate path under workspace root")
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
            // A crate may contain further crates (`systema-sysf/` does), so
            // recurse either way.
            collect_bin_crates(&path, root, out);
        }
    }

    /// A crate with a `src/main.rs` that is absent from the workspace
    /// `members` is never built: no artifact, and nothing anywhere complains.
    ///
    /// This happened to System R.  `56c2bc3` ("Delete System P, use System
    /// Init to manage power") dropped `crates/backend/systema-sysr/systema-sysr`
    /// from `members` on the line adjacent to the System P entry it meant to
    /// delete, while leaving the crate's five source files in the tree.  Every
    /// test stayed green — `platform_selects_binaries` only inspects the spec
    /// table, and the integration tests synthesise their own shims from it —
    /// so nothing noticed that `target/debug/systema-sysr` had stopped
    /// existing, which a strict boot reports as a missing executable and
    /// aborts on.
    #[test]
    fn every_bin_crate_is_a_workspace_member() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../..")
            .canonicalize()
            .expect("workspace root");

        let members = workspace_members(&root);
        assert!(
            !members.is_empty(),
            "parsed no entries from the root Cargo.toml"
        );

        let mut bins = Vec::new();
        collect_bin_crates(&root.join("crates"), &root, &mut bins);
        assert!(bins.len() >= 10, "found only {} bin crate(s)", bins.len());

        let orphans: Vec<&String> = bins.iter().filter(|d| !members.contains(*d)).collect();
        assert!(
            orphans.is_empty(),
            "bin crate(s) on disk that cargo never builds because they are missing \
             from the workspace `members`: {orphans:?}"
        );
    }
}
