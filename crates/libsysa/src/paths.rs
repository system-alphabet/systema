mod builtin {
    include!(concat!(env!("OUT_DIR"), "/paths.rs"));
}

use std::sync::OnceLock;
use tracing::debug;

pub struct Paths {
    /// Allocator workload-plane socket: workers register here and the
    /// System Finder (sysd/sysm/sysr/sysf) registers/commits staging sets.
    pub ipc_socket_path: &'static str,
    pub systema_fdpass_sock: &'static str,
    /// Control-port bus socket (one-to-many).  System Wrapper bridge flavors
    /// and control-plane tooling connect here; System A serves `manager.*`
    /// RPCs, pushes unit/job events on this socket, and answers one-shot
    /// `admin.*` requests (`admin.staging`, `admin.unitstate`).
    pub control_socket_path: &'static str,
    pub systema_shell_path: &'static str,
    pub systema_socket_handler_path: &'static str,
    pub systemd_machine_id_file: &'static str,
    pub systemd_lib_unit_dir: &'static str,
    pub systemd_first_boot_file: &'static str,
    pub locale_dir: &'static str,
    /// Base directory for runtime state (e.g. `/run`).  System A creates
    /// `systemd/system` under this path on startup so that units can
    /// reference `/run/systemd/system`.
    pub runstatedir: &'static str,
    /// Base directory for log files (e.g. `/var/log`).  SysAInit writes its
    /// own log and the per-worker logs (`systema-sysa.log`, ...) here.
    pub log_dir: &'static str,
    pub unit_search_paths: Vec<String>,
    pub generator_search_paths: Vec<String>,
    /// Search paths for System F finder executables.  The `systema-sysf`
    /// executable locates format-specific finder binaries (e.g.
    /// `systema-sysf.systemd`) in these directories.
    pub finder_search_paths: Vec<String>,
    pub systema_bin_search_paths: Vec<String>,
    /// Directory holding the notify listener sockets (SysAInit, boot
    /// animation, ...).  System A broadcasts every boot/unit event to all
    /// sockets found in this directory.
    pub notify_dir: String,
    /// Unix socket path for the SysA → SysF reload notification protocol.
    /// System F listens here; System A connects to trigger a re-scan.
    pub reload_socket: &'static str,
}

fn resolve(env: &str, default: &'static str) -> &'static str {
    match std::env::var(env) {
        Ok(v) => Box::leak(v.into_boxed_str()),
        Err(_) => default,
    }
}

fn resolve_list(env: &str, defaults: &[&str]) -> Vec<String> {
    match std::env::var(env) {
        Ok(v) => v.split(':').map(|s| s.trim().to_string()).collect(),
        Err(_) => defaults.iter().map(|s| s.to_string()).collect(),
    }
}

fn compute_paths() -> Paths {
    Paths {
        ipc_socket_path: resolve("SYSTEMA_IPC_SOCKET", builtin::IPC_SOCKET_PATH),
        systema_fdpass_sock: resolve("SYSTEMA_FDPASS_SOCK", builtin::SYSTEMA_FDPASS_SOCK),
        control_socket_path: resolve("SYSTEMA_CONTROL_SOCKET", builtin::CONTROL_SOCKET_PATH),
        systema_shell_path: resolve("SYSTEMA_SHELL_PATH", builtin::SYSTEMA_SHELL_PATH),
        systema_socket_handler_path: resolve(
            "SYSTEMA_SOCKET_HANDLER_PATH",
            builtin::SYSTEMA_SOCKET_HANDLER_PATH,
        ),
        systemd_machine_id_file: resolve(
            "SYSTEMD_MACHINE_ID_FILE",
            builtin::SYSTEMD_MACHINE_ID_FILE,
        ),
        systemd_lib_unit_dir: resolve("SYSTEMD_LIB_UNIT_DIR", builtin::SYSTEMD_LIB_UNIT_DIR),
        systemd_first_boot_file: resolve(
            "SYSTEMD_FIRST_BOOT_FILE",
            builtin::SYSTEMD_FIRST_BOOT_FILE,
        ),
        locale_dir: resolve("SYSTEMA_LOCALE_DIR", builtin::LOCALE_DIR),
        runstatedir: resolve("SYSTEMA_RUNSTATEDIR", builtin::RUNSTATEDIR),
        log_dir: resolve("SYSTEMA_LOG_DIR", builtin::LOG_DIR),
        unit_search_paths: resolve_list("SYSTEMA_UNIT_PATH", builtin::UNIT_SEARCH_PATHS),
        generator_search_paths: resolve_list(
            "SYSTEMA_GENERATOR_PATH",
            builtin::GENERATOR_SEARCH_PATHS,
        ),
        finder_search_paths: resolve_list("SYSTEMA_FINDER_PATH", builtin::FINDER_SEARCH_PATHS),
        systema_bin_search_paths: resolve_list(
            "SYSTEMA_BIN_PATH",
            builtin::SYSTEMA_BIN_SEARCH_PATHS,
        ),
        notify_dir: resolve("SYSTEMA_NOTIFY_DIR", builtin::NOTIFY_DIR).to_string(),
        reload_socket: resolve("SYSTEMA_RELOAD_SOCKET", builtin::RELOAD_SOCKET),
    }
}

static INSTANCE: OnceLock<Paths> = OnceLock::new();

pub fn init() {
    INSTANCE.get_or_init(compute_paths);
}

pub fn instance() -> &'static Paths {
    INSTANCE.get_or_init(compute_paths)
}

/// The compile-time path defaults baked in by build.rs, as
/// (`SYSTEMA_*` / `SYSTEMD_*` environment variable, default) pairs —
/// the record printed by `--full-version`.
pub fn builtin_defaults() -> Vec<(&'static str, String)> {
    let list = |items: &[&str]| items.join(":");
    vec![
        (
            "SYSTEMD_LIB_UNIT_DIR",
            builtin::SYSTEMD_LIB_UNIT_DIR.to_string(),
        ),
        (
            "SYSTEMD_FIRST_BOOT_FILE",
            builtin::SYSTEMD_FIRST_BOOT_FILE.to_string(),
        ),
        (
            "SYSTEMD_MACHINE_ID_FILE",
            builtin::SYSTEMD_MACHINE_ID_FILE.to_string(),
        ),
        ("SYSTEMA_IPC_SOCKET", builtin::IPC_SOCKET_PATH.to_string()),
        (
            "SYSTEMA_FDPASS_SOCK",
            builtin::SYSTEMA_FDPASS_SOCK.to_string(),
        ),
        (
            "SYSTEMA_CONTROL_SOCKET",
            builtin::CONTROL_SOCKET_PATH.to_string(),
        ),
        ("SYSTEMA_RUNSTATEDIR", builtin::RUNSTATEDIR.to_string()),
        ("SYSTEMA_NOTIFY_DIR", builtin::NOTIFY_DIR.to_string()),
        (
            "SYSTEMA_SHELL_PATH",
            builtin::SYSTEMA_SHELL_PATH.to_string(),
        ),
        (
            "SYSTEMA_SOCKET_HANDLER_PATH",
            builtin::SYSTEMA_SOCKET_HANDLER_PATH.to_string(),
        ),
        ("SYSTEMA_LOCALE_DIR", builtin::LOCALE_DIR.to_string()),
        ("SYSTEMA_LOG_DIR", builtin::LOG_DIR.to_string()),
        ("SYSTEMA_RELOAD_SOCKET", builtin::RELOAD_SOCKET.to_string()),
        ("SYSTEMA_UNIT_PATH", list(builtin::UNIT_SEARCH_PATHS)),
        (
            "SYSTEMA_GENERATOR_PATH",
            list(builtin::GENERATOR_SEARCH_PATHS),
        ),
        ("SYSTEMA_FINDER_PATH", list(builtin::FINDER_SEARCH_PATHS)),
        ("SYSTEMA_BIN_PATH", list(builtin::SYSTEMA_BIN_SEARCH_PATHS)),
    ]
}

/// Log every effective path value at debug level (`-D` /
/// `--log-level debug`).  System A runs in volatile environments:
/// `SYSTEMA_*` / `SYSTEMD_*` env vars and CLI flags (e.g.
/// `systema-sysi --log-dir`) may change paths between runs, so each
/// startup records what this run actually resolved.
///
/// `log_dir` is the log directory this process really uses — usually
/// [`Paths::log_dir`], or a CLI override of it.
pub fn debug_dump(log_dir: &str) {
    let p = instance();
    let list = |items: &[String]| items.join(":");
    // Same names and order as `builtin_defaults()` so a run's log can be
    // compared against `--full-version` line by line.
    let values = [
        ("SYSTEMD_LIB_UNIT_DIR", p.systemd_lib_unit_dir.to_string()),
        (
            "SYSTEMD_FIRST_BOOT_FILE",
            p.systemd_first_boot_file.to_string(),
        ),
        (
            "SYSTEMD_MACHINE_ID_FILE",
            p.systemd_machine_id_file.to_string(),
        ),
        ("SYSTEMA_IPC_SOCKET", p.ipc_socket_path.to_string()),
        ("SYSTEMA_FDPASS_SOCK", p.systema_fdpass_sock.to_string()),
        ("SYSTEMA_CONTROL_SOCKET", p.control_socket_path.to_string()),
        ("SYSTEMA_RUNSTATEDIR", p.runstatedir.to_string()),
        ("SYSTEMA_NOTIFY_DIR", p.notify_dir.clone()),
        ("SYSTEMA_SHELL_PATH", p.systema_shell_path.to_string()),
        (
            "SYSTEMA_SOCKET_HANDLER_PATH",
            p.systema_socket_handler_path.to_string(),
        ),
        ("SYSTEMA_LOCALE_DIR", p.locale_dir.to_string()),
        ("SYSTEMA_LOG_DIR", log_dir.to_string()),
        ("SYSTEMA_RELOAD_SOCKET", p.reload_socket.to_string()),
        ("SYSTEMA_UNIT_PATH", list(&p.unit_search_paths)),
        ("SYSTEMA_GENERATOR_PATH", list(&p.generator_search_paths)),
        ("SYSTEMA_FINDER_PATH", list(&p.finder_search_paths)),
        ("SYSTEMA_BIN_PATH", list(&p.systema_bin_search_paths)),
    ];
    debug!(
        "resolved paths (SYSTEMA_*/SYSTEMD_* env and CLI flags override the compiled-in defaults):"
    );
    let width = values.iter().map(|(name, _)| name.len()).max().unwrap_or(0);
    for (name, value) in values {
        debug!("  {name:<width$} {value}");
    }
}
