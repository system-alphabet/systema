//! The per-user environment matrix of `systema-sysu`.
//!
//! Every `SYSTEMA_*` socket default is a compile-time constant under
//! `/run/systema`, and the compiled-in unit search paths all point at
//! *system* unit directories.  An unprivileged per-user instance must
//! therefore override them explicitly: it can neither create anything in
//! `/run/systema` nor be allowed to load the system's units.  Everything
//! else the instance needs — binaries, locale data, shell, machine-id —
//! is shared read-only state that keeps its compiled-in default
//! (`doc/user-session.md` §3.1).
//!
//! The matrix is computed at **runtime** from `XDG_RUNTIME_DIR`,
//! `XDG_STATE_HOME` and `HOME`, and installed into the launcher's own
//! environment before [`sysa::paths::init`] — which is what the spawned
//! workers inherit.  `build.rs` compile-time defaults are untouched, and
//! the matrix stays pure `SYSTEMA_*`: the D-Bus address is not an
//! environment override but a `--bus-address` argument passed at spawn
//! ([`session_bus_address`], plan 备注 11).

#[cfg(test)]
use std::sync::OnceLock;

use sysa::paths;

/// Strip surrounding whitespace and any trailing `/` from a path value.
fn trim_path(value: &str) -> String {
    value.trim().trim_end_matches('/').to_string()
}

/// The instance's log directory: `${XDG_STATE_HOME:-$HOME/.local/state}/systema/log`.
///
/// The final `/tmp/systema-<uid>-state` branch only matters for a session
/// without `HOME`; a login session always has one of the two XDG inputs
/// (plan §3.1 defines exactly those two cases).
pub fn log_dir() -> String {
    let state_home = match std::env::var("XDG_STATE_HOME") {
        Ok(v) if !v.trim().is_empty() => trim_path(&v),
        _ => match std::env::var("HOME") {
            Ok(v) if !v.trim().is_empty() => format!("{}/.local/state", trim_path(&v)),
            _ => format!("/tmp/systema-{}-state", unsafe { libc::geteuid() }),
        },
    };
    format!("{state_home}/systema/log")
}

/// The user-domain unit search path — a **full replacement**, never an
/// extension of the compiled-in default: inheriting it would make the
/// per-user instance scan `/etc/systemd/system` and friends (plan §3.1).
fn unit_path() -> String {
    let mut dirs: Vec<String> = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        let home = trim_path(&home);
        if !home.is_empty() {
            dirs.push(format!("{home}/.config/systema"));
        }
    }
    // System-wide *user* unit directories only.  There is deliberately no
    // `/run/systema`-style entry: the runtime dir already holds the
    // instance's sockets and must not be scanned for unit files.
    dirs.push("/etc/systemd/user".to_string());
    dirs.push("/usr/lib/systemd/user".to_string());
    dirs.join(":")
}

/// The D-Bus address this instance's bus consumers must use — the
/// *session* bus, never the system bus (`doc/user-session.md` 备注 11).
///
/// The launcher passes it to its exactly two D-Bus consumers as
/// `--bus-address` at spawn: `systema-sysw.systemd` (serves
/// `org.freedesktop.systemd1` for the session's `systemctl --user`) and
/// syss's `Type=dbus` `BusName=` watch.  Both default to the **system**
/// bus (zbus's `Address::system()`) when the flag is absent — the system
/// domain never passes it — and for a user instance the session bus is
/// where they belong (the semantics `systemd --user` implements): reaching
/// `/run/dbus/system_bus_socket` would cross into the system domain.
///
/// Precedence mirrors `sd_bus_open_user()`:
/// 1. `DBUS_SESSION_BUS_ADDRESS` when set — whatever address the session's
///    own `systemctl --user` connects to is exactly what sysw must serve;
/// 2. `$XDG_RUNTIME_DIR/bus` — the standard session-bus socket;
/// 3. `{user_runtime_dir}/bus` — a path where no bus can ever listen (a
///    session this degraded has no session bus), which keeps a provable
///    dead end instead of falling back to the real system bus.
pub fn session_bus_address() -> String {
    if let Ok(addr) = std::env::var("DBUS_SESSION_BUS_ADDRESS") {
        if !addr.trim().is_empty() {
            return addr;
        }
    }
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        let dir = dir.trim().trim_end_matches('/');
        if !dir.is_empty() {
            return format!("unix:path={dir}/bus");
        }
    }
    format!("unix:path={}/bus", paths::user_runtime_dir())
}

/// The full environment matrix of the per-user instance as
/// (name, value) pairs (`doc/user-session.md` §3.1) — pure `SYSTEMA_*`;
/// the D-Bus address rides on the `--bus-address` argument instead
/// ([`session_bus_address`], 备注 11).
pub fn matrix() -> Vec<(&'static str, String)> {
    let runtime = paths::user_runtime_dir();
    let log_dir = log_dir();
    vec![
        ("SYSTEMA_RUNSTATEDIR", runtime.clone()),
        ("SYSTEMA_IPC_SOCKET", format!("{runtime}/allocator.sock")),
        ("SYSTEMA_CONTROL_SOCKET", paths::user_control_socket()),
        ("SYSTEMA_FDPASS_SOCK", format!("{runtime}/fdpass.sock")),
        // NOT `{runtime}/notify`: syss's service sd_notify socket *is*
        // `{runstatedir}/notify` (notify_socket_path()), and a path cannot
        // be both this listener directory and that socket file.  The extra
        // `/systema/` segment mirrors the system-domain construction
        // (build.rs: `{runstatedir}/systema/notify`) that keeps the two
        // apart there too.
        ("SYSTEMA_NOTIFY_DIR", format!("{runtime}/systema/notify")),
        ("SYSTEMA_RELOAD_SOCKET", format!("{runtime}/sysf.sock")),
        ("SYSTEMA_LOG_DIR", log_dir.clone()),
        ("SYSTEMA_SERVICE_LOG_DIR", format!("{log_dir}/services")),
        ("SYSTEMA_UNIT_PATH", unit_path()),
        (
            "SYSTEMA_GENERATOR_PATH",
            format!("{runtime}/systemd/generator:{runtime}/systemd/generator.late"),
        ),
    ]
}

/// Install the matrix into this process, so every worker spawned
/// afterwards inherits it.
///
/// Overrides **unconditionally**: a stray `SYSTEMA_*` export left over
/// from a system shell must not route the per-user instance into
/// `/run/systema` (or scan system unit directories).
pub fn install() {
    for (key, value) in matrix() {
        std::env::set_var(key, value);
    }
}

/// Serializes the tests of this crate that mutate or read `XDG_*` / `HOME`:
/// the process-global environment is shared by all running tests.
#[cfg(test)]
pub(crate) fn env_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Set (or, with `None`, unset) `vars` for the duration of `f`,
    /// restoring the caller's values afterwards.
    fn with_env(vars: &[(&str, Option<&str>)], f: impl FnOnce()) {
        let _guard = env_lock().lock().unwrap();
        let saved: Vec<(&str, Option<std::ffi::OsString>)> = vars
            .iter()
            .map(|(k, _)| (*k, std::env::var_os(k)))
            .collect();
        for (k, v) in vars {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        f();
        for (k, v) in saved {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }

    fn get<'a>(matrix: &'a [(&'static str, String)], key: &str) -> &'a str {
        matrix
            .iter()
            .find(|(k, _)| *k == key)
            .unwrap_or_else(|| panic!("matrix has no {key}"))
            .1
            .as_str()
    }

    #[test]
    fn every_socket_lives_under_the_user_runtime_dir() {
        with_env(
            &[
                ("XDG_RUNTIME_DIR", Some("/run/user/1000")),
                ("HOME", Some("/home/u")),
                ("XDG_STATE_HOME", None),
            ],
            || {
                let m = matrix();
                let runtime = "/run/user/1000/systema";
                assert_eq!(get(&m, "SYSTEMA_RUNSTATEDIR"), runtime);
                assert_eq!(
                    get(&m, "SYSTEMA_IPC_SOCKET"),
                    format!("{runtime}/allocator.sock")
                );
                // The CLI's `--user` target and the launcher's socket must
                // be one and the same path.
                assert_eq!(
                    get(&m, "SYSTEMA_CONTROL_SOCKET"),
                    paths::user_control_socket()
                );
                assert_eq!(
                    get(&m, "SYSTEMA_FDPASS_SOCK"),
                    format!("{runtime}/fdpass.sock")
                );
                assert_eq!(
                    get(&m, "SYSTEMA_NOTIFY_DIR"),
                    format!("{runtime}/systema/notify")
                );
                assert_eq!(
                    get(&m, "SYSTEMA_RELOAD_SOCKET"),
                    format!("{runtime}/sysf.sock")
                );
                assert_eq!(
                    get(&m, "SYSTEMA_GENERATOR_PATH"),
                    format!("{runtime}/systemd/generator:{runtime}/systemd/generator.late")
                );
            },
        );
    }

    #[test]
    fn dbus_bus_address_follows_the_session_bus() {
        // 1. An explicit session-bus address wins verbatim — it is the
        //    very address the session's `systemctl --user` will use.
        with_env(
            &[
                (
                    "DBUS_SESSION_BUS_ADDRESS",
                    Some("unix:abstract=/tmp/dbus-smoke"),
                ),
                ("XDG_RUNTIME_DIR", Some("/run/user/1000")),
            ],
            || {
                assert_eq!(session_bus_address(), "unix:abstract=/tmp/dbus-smoke");
            },
        );
        // 2. No session-bus export: the standard `$XDG_RUNTIME_DIR/bus`.
        with_env(
            &[
                ("DBUS_SESSION_BUS_ADDRESS", None),
                ("XDG_RUNTIME_DIR", Some("/run/user/1000")),
            ],
            || {
                assert_eq!(session_bus_address(), "unix:path=/run/user/1000/bus");
            },
        );
        // 3. A degraded session (no bus export, no XDG): a dead-end path
        //    under this instance's own runtime dir — never the real system
        //    bus socket.
        with_env(
            &[
                ("DBUS_SESSION_BUS_ADDRESS", None),
                ("XDG_RUNTIME_DIR", None),
            ],
            || {
                let addr = session_bus_address();
                assert!(addr.starts_with("unix:path=/tmp/systema-"), "got {addr}");
                assert!(addr.ends_with("/bus"), "got {addr}");
            },
        );
        // An empty export is treated as unset.
        with_env(
            &[
                ("DBUS_SESSION_BUS_ADDRESS", Some("  ")),
                ("XDG_RUNTIME_DIR", Some("/run/user/1000")),
            ],
            || {
                assert_eq!(session_bus_address(), "unix:path=/run/user/1000/bus");
            },
        );
    }

    #[test]
    fn unit_path_replaces_the_system_unit_dirs() {
        with_env(&[("HOME", Some("/home/u"))], || {
            let m = matrix();
            assert_eq!(
                get(&m, "SYSTEMA_UNIT_PATH"),
                "/home/u/.config/systema:/etc/systemd/user:/usr/lib/systemd/user"
            );
        });
    }

    #[test]
    fn runtime_dir_falls_back_without_xdg() {
        with_env(&[("XDG_RUNTIME_DIR", None)], || {
            let runtime = paths::user_runtime_dir();
            assert!(runtime.starts_with("/tmp/systema-"), "got {runtime}");
            assert_eq!(
                paths::user_control_socket(),
                format!("{runtime}/control.socket")
            );
        });
    }

    #[test]
    fn log_dirs_follow_the_xdg_state_home() {
        with_env(
            &[("XDG_STATE_HOME", Some("/state")), ("HOME", None)],
            || {
                let m = matrix();
                assert_eq!(get(&m, "SYSTEMA_LOG_DIR"), "/state/systema/log");
                assert_eq!(
                    get(&m, "SYSTEMA_SERVICE_LOG_DIR"),
                    "/state/systema/log/services"
                );
            },
        );
        with_env(
            &[("XDG_STATE_HOME", None), ("HOME", Some("/home/u"))],
            || {
                assert_eq!(log_dir(), "/home/u/.local/state/systema/log");
            },
        );
    }

    #[test]
    fn install_overrides_inherited_system_exports() {
        let _guard = env_lock().lock().unwrap();
        let keys: Vec<&'static str> = matrix().into_iter().map(|(k, _)| k).collect();
        let saved: Vec<(&'static str, Option<std::ffi::OsString>)> =
            keys.iter().map(|k| (*k, std::env::var_os(k))).collect();
        // A stray export from a *system* shell, pointing at the system domain.
        std::env::set_var("SYSTEMA_CONTROL_SOCKET", "/run/systema/control.socket");
        std::env::set_var("SYSTEMA_UNIT_PATH", "/etc/systemd/system");
        install();
        assert_ne!(
            std::env::var("SYSTEMA_CONTROL_SOCKET").unwrap_or_default(),
            "/run/systema/control.socket"
        );
        assert_ne!(
            std::env::var("SYSTEMA_UNIT_PATH").unwrap_or_default(),
            "/etc/systemd/system"
        );
        for (k, v) in saved {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }
}
