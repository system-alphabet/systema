//! Supervised process registry for SysAInit.
//!
//! SysAInit does exactly one thing: spawn System A and the System Workers,
//! and supervise them until they exit.  This module owns the *set* of
//! processes to spawn: the built-in module table, `init.conf` overrides,
//! `--skip-workers` trimming, platform defaults, and the one-shot finder
//! chain.

use std::collections::HashMap;

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
    /// Executable name to spawn, as selected by the platform defaults and
    /// `init.conf` overrides.
    pub binary: String,
    /// Worker ID registered with System A (`None` for System A itself).
    pub worker_id: Option<&'static str>,
    /// Supervision kind.
    pub kind: ProcessKind,
    /// Extra command-line arguments.
    pub args: &'static [&'static str],
}

/// One entry of the built-in module table.
///
/// The table defines both the spawn order and the `init.conf` keys.  A
/// module whose binary is empty for a platform is not part of that
/// platform's default set; `init.conf` can still enable it explicitly.
struct ModuleDef {
    /// Canonical short name (`--skip-workers`, per-worker flags).
    name: &'static str,
    /// `init.conf` key overriding this module's binary.
    key: &'static str,
    /// Binary used on Linux.
    linux_binary: &'static str,
    /// Binary used on every other platform.
    unix_binary: &'static str,
    /// Worker ID registered with System A (`None` when the module does not
    /// register as a worker).
    worker_id: Option<&'static str>,
    /// Supervision kind.  The splash console is best effort: its exit must
    /// not abort the boot.
    kind: ProcessKind,
}

impl ModuleDef {
    fn default_binary(&self, platform: Platform) -> Option<&'static str> {
        let binary = match platform {
            Platform::Linux => self.linux_binary,
            Platform::Other => self.unix_binary,
        };
        (!binary.is_empty()).then_some(binary)
    }

    /// Whether `name` identifies this module in `--skip-workers`: its short
    /// name, or either platform's binary name.
    fn matches_name(&self, name: &str) -> bool {
        name == self.name || name == self.linux_binary || name == self.unix_binary
    }
}

/// The built-in module table, in spawn order.
///
/// System M is the only module with a per-platform binary; System W exists
/// only on Linux by default (the D-Bus surface has no other flavor).  The
/// keys live in [`sysa::init_config`] so System A can read the `Finder=`
/// entry from the same file.
const MODULES: &[ModuleDef] = &[
    ModuleDef {
        name: "sysa",
        key: sysa::init_config::KEY_ALLOCATOR,
        linux_binary: "systema-sysa",
        unix_binary: "systema-sysa",
        worker_id: None,
        kind: ProcessKind::LongRunning,
    },
    // Started after System A is ready and before any worker, so the boot log
    // shows what the workers do from the first one on.
    ModuleDef {
        name: "sysh",
        key: sysa::init_config::KEY_SPLASH,
        linux_binary: "systema-sysh.console",
        unix_binary: "systema-sysh.console",
        worker_id: None,
        kind: ProcessKind::OneShot,
    },
    ModuleDef {
        name: "syss",
        key: sysa::init_config::KEY_SERVICE,
        linux_binary: "systema-syss",
        unix_binary: "systema-syss",
        worker_id: Some("system-s-1"),
        kind: ProcessKind::LongRunning,
    },
    ModuleDef {
        name: "syse",
        key: sysa::init_config::KEY_EXTERNAL,
        linux_binary: "systema-syse",
        unix_binary: "systema-syse",
        worker_id: Some("system-e-1"),
        kind: ProcessKind::LongRunning,
    },
    ModuleDef {
        name: "syst",
        key: sysa::init_config::KEY_TARGET,
        linux_binary: "systema-syst",
        unix_binary: "systema-syst",
        worker_id: Some("system-t-1"),
        kind: ProcessKind::LongRunning,
    },
    ModuleDef {
        name: "sysc",
        key: sysa::init_config::KEY_CRON,
        linux_binary: "systema-sysc",
        unix_binary: "systema-sysc",
        worker_id: Some("system-c-1"),
        kind: ProcessKind::LongRunning,
    },
    ModuleDef {
        name: "sysk",
        key: sysa::init_config::KEY_SOCKET,
        linux_binary: "systema-sysk",
        unix_binary: "systema-sysk",
        worker_id: Some("system-k-1"),
        kind: ProcessKind::LongRunning,
    },
    ModuleDef {
        name: "sysn",
        key: sysa::init_config::KEY_PATH,
        linux_binary: "systema-sysn",
        unix_binary: "systema-sysn",
        worker_id: Some("system-n-1"),
        kind: ProcessKind::LongRunning,
    },
    ModuleDef {
        name: "sysd",
        key: sysa::init_config::KEY_DEVICE,
        linux_binary: "systema-sysd",
        unix_binary: "systema-sysd",
        worker_id: Some("system-d-1"),
        kind: ProcessKind::LongRunning,
    },
    ModuleDef {
        name: "sysr",
        key: sysa::init_config::KEY_RESOURCE,
        linux_binary: "systema-sysr",
        unix_binary: "systema-sysr",
        worker_id: Some("system-r-1"),
        kind: ProcessKind::LongRunning,
    },
    ModuleDef {
        name: "sysm",
        key: sysa::init_config::KEY_MOUNT,
        linux_binary: "systema-sysm.linux",
        unix_binary: "systema-sysm.unix",
        worker_id: Some("system-m-1"),
        kind: ProcessKind::LongRunning,
    },
    ModuleDef {
        name: "sysw",
        key: sysa::init_config::KEY_WRAPPER,
        linux_binary: "systema-sysw.systemd",
        unix_binary: "",
        worker_id: Some("system-w-1"),
        kind: ProcessKind::LongRunning,
    },
];

/// Whether `key` names a module System Init itself supervises.
///
/// `Finder` is a valid `init.conf` key but belongs to System A, so it is
/// not accepted by SysAInit's `--worker`.
pub fn is_worker_key(key: &str) -> bool {
    MODULES.iter().any(|module| module.key == key)
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

/// Build the supervised process set from the built-in table and `config`.
///
/// `skip` accepts canonical short names (`"sysd"`) or full binary names
/// (`"systema-sysd"`, `"systema-sysm.linux"`); unknown names are an error.
/// Skipping `"sysf"` removes the whole finder chain.
///
/// An `init.conf` key replaces a module's executable list; an empty list
/// removes the module from the set.  Explicit values are honoured on every
/// platform, so a non-Linux host can opt into a module that has no default
/// there.  A key may name several executables or directories
/// (`Key=a:/dir.d`, or several `Key=`/`Key+=` lines): each becomes its own
/// [`WorkerSpec`] sharing the module's name and worker ID, in configuration
/// order.  Directories expand later, in [`resolve_set`].
pub fn build_worker_set(
    skip: &[String],
    config: &sysa::init_config::InitConfig,
) -> anyhow::Result<Vec<WorkerSpec>> {
    build_worker_set_for(Platform::current(), skip, config)
}

pub fn build_worker_set_for(
    platform: Platform,
    skip: &[String],
    config: &sysa::init_config::InitConfig,
) -> anyhow::Result<Vec<WorkerSpec>> {
    let mut components: HashMap<&'static str, Vec<String>> = MODULES
        .iter()
        .filter_map(|module| {
            module
                .default_binary(platform)
                .map(|binary| (module.name, vec![binary.to_string()]))
        })
        .collect();

    for module in MODULES {
        match config.get_list(module.key) {
            Some([]) => {
                components.remove(module.name);
            }
            Some(list) => {
                components.insert(module.name, list.to_vec());
            }
            None => {}
        }
    }

    if let Some(unknown) = skip.iter().find(|name| {
        !MODULES.iter().any(|module| module.matches_name(name))
            && !MODULES.iter().any(|module| {
                config
                    .get_list(module.key)
                    .is_some_and(|list| list.iter().any(|value| value == *name))
            })
    }) {
        anyhow::bail!(sysa::l10n::fmt(
            sysa::l10n::t_("unknown worker name '{unknown}'"),
            &[("unknown", &unknown.to_string())]
        ));
    }

    let mut retained: Vec<WorkerSpec> = Vec::new();
    for module in MODULES {
        let Some(list) = components.get(module.name) else {
            continue;
        };
        // A module name skips the module as a whole; an individual value
        // only skips that one executable.
        if skip.iter().any(|name| module.matches_name(name)) {
            continue;
        }
        for binary in list {
            if skip.iter().any(|name| name == binary) {
                continue;
            }
            retained.push(WorkerSpec {
                name: module.name,
                binary: binary.clone(),
                worker_id: module.worker_id,
                kind: module.kind,
                args: &[],
            });
        }
    }

    Ok(retained)
}

/// A resolved (spawnable) supervised process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProcess {
    pub spec: WorkerSpec,
    pub path: std::path::PathBuf,
}

/// Whether `path` is a regular file carrying the executable bit.
fn executable_file(path: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Every executable regular file directly inside `dir`, sorted by file
/// name.  Subdirectories are not entered.
fn executables_in(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<std::path::PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| executable_file(path))
        .collect();
    paths.sort();
    paths
}

/// First directory in `dirs` that contains an executable `name`.
fn search_in(dirs: &[std::path::PathBuf], name: &str) -> Option<std::path::PathBuf> {
    for dir in dirs {
        let path = dir.join(name);
        if executable_file(&path) {
            return Some(path);
        }
    }
    None
}

/// Resolve a configured module value to the executable(s) it names.
///
/// - An absolute path naming a regular executable is used as-is.
/// - An absolute path naming a directory expands to every executable
///   regular file directly inside it (non-recursive), sorted by file name.
/// - A plain binary name is searched in `--bin-dir` when given
///   (authoritative), and otherwise in the directory of the SysAInit
///   executable first, then in `PATH`.
///
/// `None` means nothing runnable was found: the path does not exist, the
/// directory holds no executable file, or the name is not in any search
/// directory.  Every candidate must be a regular file carrying the
/// executable bit.
pub fn resolve_binaries(
    binary: &str,
    bin_dir: Option<&std::path::Path>,
) -> Option<Vec<std::path::PathBuf>> {
    let direct = std::path::Path::new(binary);
    if direct.is_absolute() {
        if direct.is_dir() {
            let files = executables_in(direct);
            return (!files.is_empty()).then_some(files);
        }
        return executable_file(direct).then(|| vec![direct.to_path_buf()]);
    }
    if let Some(dir) = bin_dir {
        let path = dir.join(binary);
        return executable_file(&path).then_some(vec![path]);
    }
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            dirs.push(dir.to_path_buf());
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    search_in(&dirs, binary).map(|path| vec![path])
}

/// Resolve every spec in the set, reporting which ones are missing.
///
/// A directory value yields one [`ResolvedProcess`] per executable, in file
/// name order; each entry is named after its executable so the processes
/// get distinct log files.
pub fn resolve_set(
    set: &[WorkerSpec],
    bin_dir: Option<&std::path::Path>,
) -> (Vec<ResolvedProcess>, Vec<WorkerSpec>) {
    let mut resolved = Vec::new();
    let mut missing = Vec::new();
    for spec in set {
        match resolve_binaries(&spec.binary, bin_dir) {
            Some(paths) => {
                for path in paths {
                    resolved.push(ResolvedProcess {
                        spec: named_after_executable(spec, &path),
                        path,
                    });
                }
            }
            None => missing.push(spec.clone()),
        }
    }
    (resolved, missing)
}

/// Name a resolved process after the executable it runs.
///
/// The binary field doubles as the `--log-file` basename, so each entry of
/// an expanded directory gets its own log file instead of sharing one.
fn named_after_executable(spec: &WorkerSpec, path: &std::path::Path) -> WorkerSpec {
    let mut spec = spec.clone();
    if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
        spec.binary = name.to_string();
    }
    spec
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve_binary_call(spec: &WorkerSpec) -> &str {
        &spec.binary
    }

    fn skip(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn config(pairs: &[(&str, &str)]) -> sysa::init_config::InitConfig {
        sysa::init_config::InitConfig::from_pairs(pairs.iter().copied())
    }

    fn names(set: &[WorkerSpec]) -> Vec<&str> {
        set.iter().map(|s| s.name).collect()
    }

    #[test]
    fn platform_selects_binaries() {
        let default = sysa::init_config::InitConfig::default();
        let linux = build_worker_set_for(Platform::Linux, &[], &default).unwrap();
        assert_eq!(
            names(&linux),
            vec![
                "sysa", "sysh", "syss", "syse", "syst", "sysc", "sysk", "sysn", "sysd", "sysr",
                "sysm", "sysw"
            ]
        );
        assert_eq!(linux.len(), 12);
        // The splash console is best effort: its exit must not abort boot.
        let splash = linux.iter().find(|s| s.name == "sysh").unwrap();
        assert_eq!(splash.kind, ProcessKind::OneShot);
        assert_eq!(splash.binary, "systema-sysh.console");
        assert!(linux
            .iter()
            .filter(|s| s.name != "sysh")
            .all(|s| s.kind == ProcessKind::LongRunning));
        fn binary_of<'a>(set: &'a [WorkerSpec], name: &'a str) -> &'a str {
            resolve_binary_call(set.iter().find(|s| s.name == name).unwrap())
        }
        // System M / System W exist only on Linux.
        assert_eq!(binary_of(&linux, "sysm"), "systema-sysm.linux");
        assert_eq!(binary_of(&linux, "sysw"), "systema-sysw.systemd");
        // System R carries the plain name on every platform — unlike M and W
        // it has no per-platform suffix, so this is the name that must exist
        // as a build artifact.
        assert_eq!(binary_of(&linux, "sysr"), "systema-sysr");

        let other = build_worker_set_for(Platform::Other, &[], &default).unwrap();
        assert_eq!(
            names(&other),
            vec![
                "sysa", "sysh", "syss", "syse", "syst", "sysc", "sysk", "sysn", "sysd", "sysr",
                "sysm"
            ]
        );
        // System M falls back to its Unix flavor; System W has no default
        // off Linux.
        assert_eq!(binary_of(&other, "sysm"), "systema-sysm.unix");
        assert_eq!(binary_of(&other, "sysr"), "systema-sysr");
    }

    #[test]
    fn skip_accepts_short_names() {
        let set = build_worker_set_for(
            Platform::Linux,
            &skip(&["sysd", "sysc"]),
            &sysa::init_config::InitConfig::default(),
        )
        .unwrap();
        assert_eq!(
            names(&set),
            vec!["sysa", "sysh", "syss", "syse", "syst", "sysk", "sysn", "sysr", "sysm", "sysw"]
        );
    }

    #[test]
    fn skip_accepts_full_binary_names() {
        let set = build_worker_set_for(
            Platform::Linux,
            &skip(&["systema-sysd", "systema-sysm.linux"]),
            &sysa::init_config::InitConfig::default(),
        )
        .unwrap();
        assert_eq!(
            names(&set),
            vec!["sysa", "sysh", "syss", "syse", "syst", "sysc", "sysk", "sysn", "sysr", "sysw"]
        );
    }

    #[test]
    fn skipping_sysa_removes_the_core() {
        let set = build_worker_set_for(
            Platform::Linux,
            &skip(&["sysa"]),
            &sysa::init_config::InitConfig::default(),
        )
        .unwrap();
        assert!(!names(&set).contains(&"sysa"));
    }

    #[test]
    fn sysf_not_in_worker_set() {
        let set = build_worker_set_for(
            Platform::Linux,
            &[],
            &sysa::init_config::InitConfig::default(),
        )
        .unwrap();
        assert!(!names(&set).contains(&"sysf"));
    }

    #[test]
    fn unknown_skip_name_is_an_error() {
        let err = build_worker_set_for(
            Platform::Linux,
            &skip(&["nope"]),
            &sysa::init_config::InitConfig::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("nope"));
    }

    #[test]
    fn init_conf_overrides_a_binary() {
        let set = build_worker_set_for(
            Platform::Linux,
            &[],
            &config(&[(
                sysa::init_config::KEY_SERVICE,
                "/opt/systema/systema-syss-debug",
            )]),
        )
        .unwrap();
        let service = set.iter().find(|s| s.name == "syss").unwrap();
        assert_eq!(service.binary, "/opt/systema/systema-syss-debug");
    }

    #[test]
    fn init_conf_lists_several_executables_per_module() {
        let set = build_worker_set_for(
            Platform::Linux,
            &[],
            &config(&[(
                sysa::init_config::KEY_SERVICE,
                "systema-syss:/opt/syss.d:/opt/systema-syss-helper",
            )]),
        )
        .unwrap();
        let service: Vec<&str> = set
            .iter()
            .filter(|s| s.name == "syss")
            .map(|s| s.binary.as_str())
            .collect();
        assert_eq!(
            service,
            vec!["systema-syss", "/opt/syss.d", "/opt/systema-syss-helper"]
        );
        // The extra processes keep the module's worker ID, so they stay one
        // logical worker.
        assert!(set
            .iter()
            .filter(|s| s.name == "syss")
            .all(|s| s.worker_id == Some("system-s-1")));
        // The module stays one contiguous block in table order (after sysh).
        let all = names(&set);
        let first = all.iter().position(|n| *n == "syss").unwrap();
        assert_eq!(&all[first..first + 3], &["syss", "syss", "syss"]);
    }

    #[test]
    fn skipping_one_value_leaves_the_rest_of_the_module() {
        let set = build_worker_set_for(
            Platform::Linux,
            &skip(&["/opt/systema-syss-helper"]),
            &config(&[(
                sysa::init_config::KEY_SERVICE,
                "systema-syss:/opt/systema-syss-helper",
            )]),
        )
        .unwrap();
        let service: Vec<&str> = set
            .iter()
            .filter(|s| s.name == "syss")
            .map(|s| s.binary.as_str())
            .collect();
        assert_eq!(service, vec!["systema-syss"]);
    }

    #[test]
    fn skipping_the_module_name_drops_every_value() {
        let set = build_worker_set_for(
            Platform::Linux,
            &skip(&["syss"]),
            &config(&[(
                sysa::init_config::KEY_SERVICE,
                "systema-syss:/opt/systema-syss-helper",
            )]),
        )
        .unwrap();
        assert!(!names(&set).contains(&"syss"));
    }

    #[test]
    fn init_conf_empty_value_disables_a_module() {
        let set = build_worker_set_for(
            Platform::Linux,
            &[],
            &config(&[(sysa::init_config::KEY_DEVICE, "")]),
        )
        .unwrap();
        assert!(!names(&set).contains(&"sysd"));
    }

    #[test]
    fn init_conf_can_disable_the_splash_console() {
        let set = build_worker_set_for(
            Platform::Linux,
            &[],
            &config(&[(sysa::init_config::KEY_SPLASH, "")]),
        )
        .unwrap();
        assert!(!names(&set).contains(&"sysh"));
    }

    #[test]
    fn init_conf_can_enable_a_module_off_linux() {
        let set = build_worker_set_for(
            Platform::Other,
            &[],
            &config(&[(sysa::init_config::KEY_WRAPPER, "systema-sysw.systemd")]),
        )
        .unwrap();
        assert!(names(&set).contains(&"sysw"));
    }

    #[test]
    fn init_conf_binary_can_be_skipped_by_name() {
        let set = build_worker_set_for(
            Platform::Linux,
            &skip(&["systema-syss-debug"]),
            &config(&[(sysa::init_config::KEY_SERVICE, "systema-syss-debug")]),
        )
        .unwrap();
        assert!(!names(&set).contains(&"syss"));
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

        let hit = resolve_binaries("systema-sysd", Some(&dir)).unwrap();
        assert_eq!(hit, vec![dir.join("systema-sysd")]);

        // An explicit bin-dir does not fall back to PATH / exe dir.
        assert!(resolve_binaries("systema-sysa", Some(&dir)).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn absolute_paths_are_used_as_is() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        let dir = shim_dir("absolute");
        let path = dir.join("my-worker");
        fs::write(&path, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();

        // An absolute path bypasses the bin-dir/PATH lookup entirely.
        assert_eq!(
            resolve_binaries(
                path.to_str().unwrap(),
                Some(std::path::Path::new("/nonexistent"))
            ),
            Some(vec![path.clone()])
        );
        assert!(resolve_binaries(dir.join("missing").to_str().unwrap(), None).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn bare_names_are_only_looked_up_in_path() {
        let name = format!("systema-absent-{}", std::process::id());
        assert!(resolve_binaries(&name, None).is_none());
    }

    #[test]
    fn a_directory_value_expands_to_executables_in_name_order() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        let dir = shim_dir("expand");
        let workers = dir.join("workers.d");
        fs::create_dir_all(&workers).unwrap();
        for name in ["charlie", "alpha", "bravo"] {
            let path = workers.join(name);
            fs::write(&path, "#!/bin/sh\n").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        // Neither a file without the executable bit nor a subdirectory is
        // part of the expansion.
        fs::write(workers.join("data.txt"), "not a program").unwrap();
        fs::create_dir_all(workers.join("nested")).unwrap();
        fs::write(workers.join("nested/delta"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(
            workers.join("nested/delta"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();

        let set = vec![WorkerSpec {
            name: "sysd",
            binary: workers.to_string_lossy().into_owned(),
            worker_id: Some("system-d-1"),
            kind: ProcessKind::LongRunning,
            args: &[],
        }];
        let (resolved, missing) = resolve_set(&set, None);
        assert!(missing.is_empty());
        assert_eq!(
            resolved
                .iter()
                .map(|rp| rp.path.clone())
                .collect::<Vec<_>>(),
            vec![
                workers.join("alpha"),
                workers.join("bravo"),
                workers.join("charlie")
            ]
        );
        // Each process is named after its executable, so their `--log-file`
        // basenames differ.
        assert_eq!(
            resolved
                .iter()
                .map(|rp| rp.spec.binary.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "bravo", "charlie"]
        );
        assert!(resolved
            .iter()
            .all(|rp| rp.spec.worker_id == Some("system-d-1")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_directory_is_reported_missing() {
        use std::fs;
        let dir = shim_dir("empty-dir");
        let workers = dir.join("workers.d");
        fs::create_dir_all(&workers).unwrap();

        let set = vec![WorkerSpec {
            name: "sysd",
            binary: workers.to_string_lossy().into_owned(),
            worker_id: Some("system-d-1"),
            kind: ProcessKind::LongRunning,
            args: &[],
        }];
        let (resolved, missing) = resolve_set(&set, None);
        assert!(resolved.is_empty());
        assert_eq!(missing.len(), 1);
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

        let set = build_worker_set_for(
            Platform::Linux,
            &[],
            &sysa::init_config::InitConfig::default(),
        )
        .unwrap();
        let (resolved, missing) = resolve_set(&set, Some(&dir));
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].spec.name, "sysa");
        assert_eq!(missing.len(), 11);
        assert!(missing.iter().any(|m| m.name == "sysh"));
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
