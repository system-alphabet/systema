//! Process management for System S.
//!
//! Provides `start_service` and `stop_service` functions that launch and
//! terminate processes, respectively. Uses `tokio::process` (which wraps
//! fork/exec on Unix) so we remain single-threaded-async.
//!
//! # ExecStart parsing (systemd-compatible)
//!
//! Systemd's `ExecStart=` directive supports several single-character
//! prefixes, `%`-specifiers, `$VAR` / `${VAR}` environment expansion,
//! and a `|` prefix that routes the command through `sh -c`.  Our
//! implementation mirrors systemd's behaviour:
//!
//! | Prefix | systemd flag               | Meaning                          |
//! |--------|----------------------------|----------------------------------|
//! | `-`    | `IGNORE_FAILURE`           | Ignore non-zero exit             |
//! | `@`    | (separate argv[0])         | Use next token as argv[0]        |
//! | `+`    | `FULLY_PRIVILEGED`         | Run as root (no User=/Group=)    |
//! | `:`    | `NO_ENV_EXPAND`            | Disable `$VAR` expansion         |
//! | `!`    | `NO_SETUID`                | No credential changes            |
//! | `!!`   | `NO_SETUID` (seccomp only) | Like `!` but only for seccomp    |
//! | `|`    | `VIA_SHELL`                | Run via `sh -c`                  |

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::OnceLock;

#[cfg(unix)]
use std::os::unix::io::{FromRawFd, IntoRawFd, OwnedFd, RawFd};

#[cfg(unix)]
use std::ffi::CString;

use anyhow::{bail, Context, Result};
use tokio::process::{Child, Command};
use tracing::{debug, info, warn};

#[cfg(unix)]
use nix::libc;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use sysa::proto::UnitConfig;

use crate::state::{ServiceRegistry, ServiceState};

/// Environment variables injected into every spawned process so that
/// `systemctl`/`systemd` invocations inside a unit talk to the system bus
/// instead of falling back to offline / `--root` operation.
///
/// `SYSTEMCTL_FORCE_BUS=1` is exactly what real systemd passes to its child
/// processes (it forces `systemctl` to use the bus even when it looks
/// offline); `SYSTEMD_OFFLINE=0` disables systemd's offline (`--root`)
/// detection. Both are applied *after* the unit's `Environment=` settings so
/// a unit cannot override them.
const FORCED_SERVICE_ENV: &[(&str, &str)] =
    &[("SYSTEMCTL_FORCE_BUS", "1"), ("SYSTEMD_OFFLINE", "0")];

/// Directory where per-service stdout/stderr logs are captured.
const SERVICE_LOG_DIR: &str = "/var/log/services";

/// Lazily create the service log directory (`/var/log/services/`) with
/// world-readable permissions (0755).  Returns `Ok(())` on success or if the
/// directory already exists; logs a warning and returns `Ok(())` on failure
/// so that a log-directory issue never prevents a service from starting.
fn ensure_service_log_dir() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        if let Err(e) = std::fs::create_dir_all(SERVICE_LOG_DIR) {
            warn!("Failed to create {}: {}", SERVICE_LOG_DIR, e);
        } else {
            // Ensure the directory is world-readable and traversable.
            let _ =
                std::fs::set_permissions(SERVICE_LOG_DIR, std::fs::Permissions::from_mode(0o755));
        }
    });
}

/// Open a log file at `/var/log/services/<name>.log` in append mode with
/// 0644 permissions (world-readable).  Returns `None` on failure so that
/// log-file issues never prevent a service from starting.
fn open_service_log(name: &str) -> Option<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    let path = format!("{}/{}.log", SERVICE_LOG_DIR, name);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o644)
        .open(&path)
        .ok()?;
    // Ensure existing files are also world-readable (the mode option only
    // applies on creation).
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644));
    Some(file)
}

/// Launch the service described by `config`.
/// Returns the PID and the Child handle of the spawned main process.
/// The caller must keep the Child handle to later collect the exit status.
///
/// If `invocation_id` is `Some`, the `INVOCATION_ID` environment variable is
/// set in the spawned process's environment (systemd-compatible behaviour).
///
/// If `listen_fds` is non-empty, the fds are handed to the child as
/// systemd-style socket-activation fds: they are moved to 3..3+N,
/// `LISTEN_FDS=N` / `LISTEN_PID=<child pid>` are set in the child, and the
/// original descriptors are closed in the parent after `spawn()`.
#[cfg(unix)]
pub async fn start_service(
    registry: ServiceRegistry,
    config: &UnitConfig,
    invocation_id: Option<String>,
    listen_fds: Vec<RawFd>,
) -> Result<(u32, Child)> {
    let unit_name = config.unit_name.clone();
    let svc = config.service.as_ref().ok_or_else(|| {
        anyhow::anyhow!(sysa::l10n::fmt(
            sysa::l10n::t_("No [Service] config for {unit_name}."),
            &[("unit_name", &unit_name)]
        ))
    })?;

    if svc.exec_start.is_empty() {
        bail!(sysa::l10n::fmt(
            sysa::l10n::t_("ExecStart is empty for {unit_name}."),
            &[("unit_name", &unit_name)]
        ));
    }

    // Parse the ExecStart command line (prefixes, word splitting, % specifiers).
    let parsed = parse_exec_start(&svc.exec_start, &unit_name)?;
    info!(
        "Starting {}: {} {:?}",
        unit_name, parsed.program, parsed.args
    );

    // Whether the service gets its credentials switched (User=/Group=) and
    // its PAM session opened.  The `+` (FULLY_PRIVILEGED) and `!`/`!!`
    // (NO_SETUID) ExecStart prefixes disable both, exactly like systemd's
    // `needs_setuid` in exec-invoke.c.
    let wants_credentials = !parsed.flags.privileged && !parsed.flags.no_new_privileges;

    // Resolve User=/Group= credentials first (numeric UIDs are supported,
    // like systemd's get_user_creds()).  The canonical username from
    // /etc/passwd is what PAM gets (systemd passes pw_name, not the raw
    // "User=" value — pam_systemd rejects purely numeric user names).
    let creds = if wants_credentials {
        let user = svc.user.clone();
        let group = svc.group.clone();
        tokio::task::spawn_blocking(move || resolve_credentials(&user, &group))
            .await
            .context(sysa::l10n::t_("Credential lookup task failed"))?
            .with_context(|| {
                sysa::l10n::fmt(
                    sysa::l10n::t_("Failed to resolve credentials for {unit_name}."),
                    &[("unit_name", &unit_name)],
                )
            })?
    } else {
        None
    };

    // Open the PAM session (PAMName= + User= required, mirroring systemd's
    // exec-invoke.c `setup_pam()` trigger).  pam_systemd.so & co run here
    // and export the runtime environment (XDG_RUNTIME_DIR, ...) which is
    // merged into the child's environment below.
    let mut pam_env: Vec<(String, String)> = Vec::new();
    let mut pam_session: Option<crate::pam::PamSession> = None;
    if wants_credentials && !svc.pam_name.is_empty() && !svc.user.is_empty() {
        let pam_name = svc.pam_name.clone();
        // The canonical username (pw_name) resolved above; fall back to the
        // raw User= value if resolution somehow produced no name.
        let username = creds
            .as_ref()
            .and_then(|c| c.username.clone())
            .unwrap_or_else(|| svc.user.clone());
        let tty = if svc.tty_path.is_empty() {
            None
        } else {
            Some(svc.tty_path.clone())
        };
        let pam_name_task = pam_name.clone();
        let username_task = username.clone();
        match tokio::task::spawn_blocking(move || {
            crate::pam::pam_setup(&pam_name_task, &username_task, tty.as_deref())
        })
        .await
        {
            Ok(Ok((env, session))) => {
                pam_env = env;
                pam_session = Some(session);
                info!(
                    "Opened PAM session '{}' for user '{}' ({})",
                    pam_name, username, unit_name
                );
            }
            Ok(Err(e)) => {
                warn!(
                    "PAM setup for {} ({}) failed: {}; continuing without PAM environment",
                    unit_name, pam_name, e
                );
            }
            Err(e) => {
                warn!("PAM setup task for {} failed: {}", unit_name, e);
            }
        }
    }

    // XDG_RUNTIME_DIR fallback: pam_systemd normally provides this via the
    // PAM environment, and systemd --user refuses to run without it
    // (main.c "Trying to run as user instance, but $XDG_RUNTIME_DIR is not
    // set.").  When the PAM setup was skipped or failed (e.g. a missing
    // /etc/pam.d/systemd-user), fall back to the user's runtime directory
    // if it exists, mirroring what pam_systemd would have set.
    if let Some((key, value)) =
        xdg_runtime_dir_fallback(&svc.environment, &pam_env, creds.as_ref().map(|c| c.uid))
    {
        pam_env.push((key, value.clone()));
        info!("Set XDG_RUNTIME_DIR={value} for {unit_name} (PAM did not provide it)");
    }

    // Build environment lookup table (process env + unit Environment= +
    // PAM env + forced env).
    let env_table = build_env_table(&svc.environment, &pam_env);

    // Expand $VAR / ${VAR} in every argument, handling standalone splitting.
    let final_args = if parsed.flags.no_env_expand {
        parsed.args.clone()
    } else {
        expand_argv(&parsed.args, &env_table)
    };

    // Update state to Starting.
    {
        let mut reg = registry.lock();
        let inst = reg.entry(unit_name.clone()).or_default();
        inst.state = ServiceState::Starting;
        inst.invocation_id = invocation_id.clone();
    }

    // Build the Command plus the exact argv that will be handed to exec (the
    // final command array with every transformation applied: env expansion,
    // via_shell join), so the executed command can be verified against the
    // unit file's ExecStart.
    let (mut cmd, argv) = if parsed.flags.via_shell {
        // | prefix: route through sh -c
        let joined = build_shell_command_line(&parsed.program, &final_args);
        let mut c = Command::new(sysa::paths::instance().systema_shell_path);
        c.arg("-c");
        c.arg(&joined);
        (
            c,
            vec![
                sysa::paths::instance().systema_shell_path.to_string(),
                "-c".to_string(),
                joined,
            ],
        )
    } else {
        let mut c = Command::new(&parsed.program);
        c.args(&final_args);
        (
            c,
            std::iter::once(parsed.program.clone())
                .chain(final_args.iter().cloned())
                .collect(),
        )
    };
    debug!("ExecStart {} argv={argv:?}", unit_name);

    if !svc.working_directory.is_empty() {
        cmd.current_dir(&svc.working_directory);
    }

    // Environment variables: collected here and applied inside the pre_exec
    // hook via setenv(3).  We must NOT use Command::env(): once any env is
    // set, std::process builds the child environment array in the parent and
    // execs with execvpe(), which discards every setenv() performed by
    // pre_exec (LISTEN_PID/LISTEN_FDS included).
    let mut child_envs: Vec<(CString, CString)> = Vec::new();
    for env_str in &svc.environment {
        if let Some((key, val)) = env_str.split_once('=') {
            if let (Ok(k), Ok(v)) = (CString::new(key), CString::new(val)) {
                child_envs.push((k, v));
            }
        }
    }
    // PAM environment (applied after Environment= so it cannot be
    // overridden by the unit, mirroring systemd's strv_env_merge order).
    for (key, val) in &pam_env {
        if let (Ok(k), Ok(v)) = (CString::new(key.as_str()), CString::new(val.as_str())) {
            child_envs.push((k, v));
        }
    }
    // Set INVOCATION_ID if provided (systemd compatibility).
    if let Some(ref inv_id) = invocation_id {
        if let (Ok(k), Ok(v)) = (CString::new("INVOCATION_ID"), CString::new(inv_id.as_str())) {
            child_envs.push((k, v));
        }
    }
    // Force systemctl/systemd in the child to use the live bus (applied last
    // so it cannot be overridden by the unit's Environment= settings).
    for (key, val) in FORCED_SERVICE_ENV {
        if let (Ok(k), Ok(v)) = (CString::new(*key), CString::new(*val)) {
            child_envs.push((k, v));
        }
    }
    // Type=notify / notify-reload: point sd_notify(3) at the shared
    // datagram socket, like systemd's exec-invoke.c:2218 (applied after
    // everything else so the unit cannot override it).
    if matches!(svc.service_type.as_str(), "notify" | "notify-reload") {
        if let (Ok(k), Ok(v)) = (
            CString::new("NOTIFY_SOCKET"),
            CString::new(crate::notify::notify_socket_path()),
        ) {
            child_envs.push((k, v));
        }
    }

    // TTY stdio: attach the configured TTY as the controlling terminal and
    // redirect the requested standard streams to it.  The pre_exec action is
    // returned so it can be merged with the env/socket-activation one below
    // (std::process only allows a single pre_exec hook).
    let (tty_fd, tty_pre_exec) = apply_tty(&mut cmd, svc, &unit_name)?;

    // Per-service log capture: redirect stdout/stderr to
    // /var/log/services/<unit_name>.log unless TTY already owns them.
    // Two separate file handles are needed because Stdio::from() consumes
    // the File; append mode makes concurrent writes safe.
    if tty_fd.is_none() {
        ensure_service_log_dir();
        if let Some(f) = open_service_log(&unit_name) {
            if let Ok(f2) = f.try_clone() {
                cmd.stdout(Stdio::from(f));
                cmd.stderr(Stdio::from(f2));
            } else {
                cmd.stdout(Stdio::from(f));
            }
        }
    }

    // Env injection + socket activation: hand listener fds to the child as
    // 3..3+N and set LISTEN_FDS / LISTEN_PID (checked by sd_listen_fds()).
    // User=/Group= credentials are applied last, in the child.
    let sa_pre_exec = build_pre_exec(child_envs, &listen_fds, creds);
    let merged = match (tty_pre_exec, sa_pre_exec) {
        (Some(mut a), Some(mut b)) => Some(Box::new(move || {
            a()?;
            b()
        }) as PreExecFn),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };
    if let Some(mut pre) = merged {
        unsafe {
            cmd.pre_exec(move || pre());
        }
    }

    // Spawn the child process. We deliberately do NOT wait here — the child
    // is monitored asynchronously via `monitor_child`.
    let spawn_result = cmd.spawn();
    close_tty_fd(tty_fd);
    for fd in &listen_fds {
        unsafe {
            libc::close(*fd);
        }
    }
    let child = spawn_result.with_context(|| {
        sysa::l10n::fmt(
            sysa::l10n::t_("Failed to spawn {program}."),
            &[("program", &parsed.program)],
        )
    })?;

    let pid = child.id().ok_or_else(|| {
        anyhow::anyhow!(sysa::l10n::fmt(
            sysa::l10n::t_("Failed to get PID for {unit_name}."),
            &[("unit_name", &unit_name)]
        ))
    })?;

    info!("Service {} started, PID={}", unit_name, pid);

    // Keep the PAM session open until the service's main process exits,
    // then tear it down (the "(sd-pam)" helper equivalent).
    if let Some(session) = pam_session {
        let unit_name_for_cleanup = unit_name.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                // The monitor reaps the child; once reaped (or a zombie),
                // the process is gone and the PAM session can be closed.
                if !is_alive(pid) || pid_is_zombie(pid) {
                    break;
                }
            }
            let _ = tokio::task::spawn_blocking(move || session.close()).await;
            debug!(
                "Closed PAM session for {} (PID {})",
                unit_name_for_cleanup, pid
            );
        });
    }

    // Update state.  A `Type=oneshot` service must NOT be reported as
    // "running"/active right after spawn: unlike systemd it would only
    // become UNIT_ACTIVE after ExecStart has exited (SERVICE_EXITED with
    // RemainAfterExit=true).  systemd keeps oneshot units in
    // SERVICE_START/SERVICE_START_POST (i.e. UNIT_ACTIVATING) while the
    // control process is still running (src/core/service.c:73-103).  The
    // oneshot start path therefore stays in `Starting` here and lets
    // `await_oneshot_exit` (in controller.rs) move it to Running/Dead/Failed
    // once ExecStart actually exits.  Other service types are "Running" as
    // soon as the main process is spawned.
    let is_oneshot = svc.service_type.as_str() == "oneshot";
    {
        let mut reg = registry.lock();
        if let Some(inst) = reg.get_mut(&unit_name) {
            inst.state = if is_oneshot {
                ServiceState::Starting
            } else {
                ServiceState::Running
            };
            inst.main_pid = Some(pid);
        }
    }

    Ok((pid, child))
}

#[cfg(not(unix))]
pub async fn start_service(
    _registry: ServiceRegistry,
    _config: &UnitConfig,
    _invocation_id: Option<String>,
    _listen_fds: Vec<i32>,
) -> Result<(u32, Child)> {
    bail!(sysa::l10n::t_(
        "Starting services is not supported on this platform."
    ));
}

/// Stop a running service by sending SIGTERM (then SIGKILL after timeout).
pub async fn stop_service(
    registry: ServiceRegistry,
    unit_name: &str,
    timeout_secs: u32,
) -> Result<()> {
    let pid = {
        let mut reg = registry.lock();
        let inst = reg.get_mut(unit_name);
        match inst {
            None => {
                debug!("stop_service: {} not in registry", unit_name);
                return Ok(());
            }
            Some(inst) => {
                if inst.state == ServiceState::Dead || inst.state == ServiceState::Failed {
                    debug!("stop_service: {} already stopped", unit_name);
                    return Ok(());
                }
                inst.state = ServiceState::Stopping;
                inst.main_pid
            }
        }
    };

    if let Some(pid) = pid {
        #[cfg(unix)]
        {
            use nix::sys::signal::{self, Signal};
            use nix::unistd::Pid;
            let _ = signal::kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
            info!("Sent SIGTERM to PID {} ({})", pid, unit_name);

            let deadline = std::time::Instant::now()
                + std::time::Duration::from_secs(timeout_secs.max(1) as u64);
            loop {
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                if !is_alive(pid) {
                    info!("Service {} (PID {}) exited after SIGTERM", unit_name, pid);
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    warn!(
                        "Service {} (PID {}) did not exit in {}s; sending SIGKILL",
                        unit_name, pid, timeout_secs
                    );
                    let _ = signal::kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
                    break;
                }
            }
        }
        #[cfg(not(unix))]
        {
            warn!("Signal delivery not supported on this platform");
        }
    }

    {
        let mut reg = registry.lock();
        if let Some(inst) = reg.get_mut(unit_name) {
            inst.state = ServiceState::Dead;
            inst.main_pid = None;
            inst.invocation_id = None;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Platform-specific helpers
// ---------------------------------------------------------------------------

#[cfg(unix)]
pub fn is_alive(pid: u32) -> bool {
    use nix::sys::signal;
    use nix::unistd::Pid;
    signal::kill(Pid::from_raw(pid as i32), None).is_ok()
}

#[cfg(not(unix))]
pub fn is_alive(_pid: u32) -> bool {
    false
}

// ---------------------------------------------------------------------------
// Pre-exec hooks (TTY + socket activation)
// ---------------------------------------------------------------------------

/// Type-erased `pre_exec` closure (std::process::CommandExt allows only one).
#[cfg(unix)]
type PreExecFn = Box<dyn FnMut() -> std::io::Result<()> + Send + Sync>;

/// Credential switching data for the child pre_exec hook
/// (User=/Group=, applied like systemd's `apply_credentials()`).
#[cfg(unix)]
struct Creds {
    uid: libc::uid_t,
    gid: libc::gid_t,
    /// Canonical username (pw_name) for `initgroups()`; `None` when only
    /// `Group=` was set.
    username: Option<String>,
}

/// Resolve `User=`/`Group=` into uid/gid.  Numeric UIDs are supported
/// (systemd behaviour: `User=1000` resolves via `getpwuid`).  Returns
/// `Ok(None)` when neither is set.
#[cfg(unix)]
fn resolve_credentials(user: &str, group: &str) -> Result<Option<Creds>> {
    use nix::unistd::{Gid, Group, Uid, User};

    let username = if user.is_empty() {
        None
    } else {
        Some(user.to_string())
    };
    let group = if group.is_empty() {
        None
    } else {
        Some(group.to_string())
    };
    if username.is_none() && group.is_none() {
        return Ok(None);
    }

    let mut gid: Option<Gid> = None;
    if let Some(g) = &group {
        let gr = Group::from_name(g)
            .with_context(|| {
                sysa::l10n::fmt(
                    sysa::l10n::t_("lookup of group '{g}' failed"),
                    &[("g", &g.to_string())],
                )
            })?
            .ok_or_else(|| {
                anyhow::anyhow!(sysa::l10n::fmt(
                    sysa::l10n::t_("group '{g}' not found"),
                    &[("g", &g.to_string())]
                ))
            })?;
        gid = Some(gr.gid);
    }

    let mut uid: Option<Uid> = None;
    let mut resolved_user: Option<String> = None;
    if let Some(u) = &username {
        let found = match User::from_name(u).with_context(|| {
            sysa::l10n::fmt(
                sysa::l10n::t_("lookup of user '{u}' failed"),
                &[("u", &u.to_string())],
            )
        })? {
            Some(usr) => Some(usr),
            // Numeric UID (systemd resolves `User=1000` via getpwuid).
            None => u.parse::<u32>().ok().and_then(|n| {
                User::from_uid(Uid::from_raw(n))
                    .with_context(|| {
                        sysa::l10n::fmt(
                            sysa::l10n::t_("lookup of uid '{n}' failed"),
                            &[("n", &n.to_string())],
                        )
                    })
                    .ok()
                    .flatten()
            }),
        };
        let usr = found.ok_or_else(|| {
            anyhow::anyhow!(sysa::l10n::fmt(
                sysa::l10n::t_("user '{u}' not found"),
                &[("u", &u.to_string())]
            ))
        })?;
        uid = Some(usr.uid);
        resolved_user = Some(usr.name);
        if gid.is_none() {
            gid = Some(usr.gid);
        }
    }

    Ok(Some(Creds {
        uid: uid.unwrap_or(Uid::from_raw(0)).as_raw(),
        gid: gid.unwrap_or(Gid::from_raw(0)).as_raw(),
        username: resolved_user,
    }))
}

/// Whether `pid` is a zombie (exited but not yet reaped).  Linux-specific;
/// used to detect process death without waiting.
#[cfg(unix)]
pub fn pid_is_zombie(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| s.split_whitespace().nth(2).map(|st| st == "Z"))
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn pid_is_zombie(_pid: u32) -> bool {
    false
}

/// Build the pre_exec closure that applies the child environment with
/// setenv(3), hands `listen_fds` to the child as systemd-style
/// socket-activation fds: each fd is dup2'ed to 3..3+N with CLOEXEC
/// cleared, the originals are CLOEXEC-flagged (they must not leak past
/// exec), sets `LISTEN_PID` / `LISTEN_FDS` (the pid is only known in the
/// child; `sd_listen_fds()` verifies `LISTEN_PID` against `getpid()`), and
/// finally switches credentials (`initgroups`/`setgid`/`setuid`) when
/// `creds` is set — mirroring systemd's `apply_credentials()`.
///
/// The returned hook is always Some: the forced environment must reach the
/// child even when there are no listener fds.  This only works when the
/// Command carries no explicit env (get_envs() == None), because std then
/// execs with execvp() using the child's environ — the one setenv() writes
/// to — instead of a pre-built execvpe() array.
#[cfg(unix)]
fn build_pre_exec(
    child_envs: Vec<(CString, CString)>,
    listen_fds: &[RawFd],
    creds: Option<Creds>,
) -> Option<PreExecFn> {
    let fds = listen_fds.to_vec();
    Some(Box::new(move || {
        for (key, val) in &child_envs {
            unsafe {
                if libc::setenv(key.as_ptr(), val.as_ptr(), 1) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
        }
        for (i, &fd) in fds.iter().enumerate() {
            let target = 3 + i as RawFd;
            if fd != target {
                unsafe {
                    if libc::dup2(fd, target) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                }
            }
            unsafe {
                let flags = libc::fcntl(target, libc::F_GETFD, 0);
                if flags >= 0 {
                    libc::fcntl(target, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
                }
            }
        }
        if !fds.is_empty() {
            let pid_key = CString::new("LISTEN_PID").map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    sysa::l10n::t_("NUL in LISTEN_PID"),
                )
            })?;
            let pid = unsafe { libc::getpid() };
            let pid_val = CString::new(pid.to_string()).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    sysa::l10n::t_("NUL in pid string"),
                )
            })?;
            unsafe {
                libc::setenv(pid_key.as_ptr(), pid_val.as_ptr(), 1);
            }
            let n_fds = CString::new(fds.len().to_string()).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    sysa::l10n::t_("NUL in LISTEN_FDS"),
                )
            })?;
            unsafe {
                libc::setenv(c"LISTEN_FDS".as_ptr(), n_fds.as_ptr(), 1);
            }
        }
        if let Some(creds) = &creds {
            unsafe {
                match &creds.username {
                    // Load the user's supplementary groups (systemd's
                    // initgroups()), or drop all supplementary groups when
                    // only Group= was set.
                    Some(username) => {
                        let uname = CString::new(username.as_str()).map_err(|_| {
                            std::io::Error::new(
                                std::io::ErrorKind::InvalidInput,
                                sysa::l10n::t_("NUL in username"),
                            )
                        })?;
                        if libc::initgroups(uname.as_ptr(), creds.gid) < 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                    }
                    None => {
                        if libc::setgroups(0, std::ptr::null()) < 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                    }
                }
                if libc::setgid(creds.gid) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::setuid(creds.uid) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
        }
        Ok(())
    }))
}

#[cfg(not(unix))]
type PreExecFn = ();

// ---------------------------------------------------------------------------
// TTY stdio (systemd-compatible StandardInput/Output/Error=tty)
// ---------------------------------------------------------------------------

/// Whether a `Standard*=` value requests TTY I/O.  systemd supports the
/// values `tty`, `tty-force`, `tty-fail` and `tty-sockets`; all of them
/// attach the TTY to the process.
fn is_tty_mode(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower == "tty" || lower.starts_with("tty-")
}

fn is_tty_force(value: &str) -> bool {
    value.eq_ignore_ascii_case("tty-force")
}

/// Duplicate `fd` into a fresh, owned descriptor suitable for `Stdio`.
#[cfg(unix)]
fn dup_fd(fd: RawFd) -> Result<OwnedFd> {
    let new_fd = unsafe { libc::dup(fd) };
    if new_fd < 0 {
        bail!(sysa::l10n::fmt(
            sysa::l10n::t_("Failed to duplicate TTY fd: {e}."),
            &[("e", &std::io::Error::last_os_error().to_string())]
        ));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(new_fd) })
}

/// Close the parent-side TTY descriptor after `spawn()` (the child already
/// duplicated what it needs via stdio / `pre_exec`).
#[cfg(unix)]
fn close_tty_fd(fd: Option<RawFd>) {
    if let Some(fd) = fd {
        unsafe {
            libc::close(fd);
        }
    }
}

#[cfg(not(unix))]
fn close_tty_fd(_fd: Option<RawFd>) {}

/// Attach the unit's TTY to the child process.
///
/// When any of `StandardInput=/StandardOutput=/StandardError=` selects the
/// `tty` mode, the device at `TTYPath=` (default `/dev/console`) is opened,
/// the requested standard streams are redirected to it, and the child is
/// made a session leader with the TTY as its controlling terminal (so the
/// TTY delivers terminal signals and job control to the service).
///
/// Returns the raw fd of the opened TTY (the caller must keep it open until
/// after `spawn()`, then close it with [`close_tty_fd`]) and the pre_exec
/// closure to run in the child (returned — not registered — because
/// `std::process` only allows a single pre_exec hook that must be merged with
/// the socket-activation one).
///
/// If the TTY device cannot be opened (e.g. insufficient permissions), a
/// warning is logged and `Ok((None, None))` is returned so the service still
/// starts with its ordinary stdio.
#[cfg(unix)]
fn apply_tty(
    cmd: &mut Command,
    svc: &sysa::proto::ServiceConfig,
    unit_name: &str,
) -> Result<(Option<RawFd>, Option<PreExecFn>)> {
    let tty_in = is_tty_mode(&svc.standard_input);
    let tty_out = is_tty_mode(&svc.standard_output);
    let tty_err = is_tty_mode(&svc.standard_error);
    if !(tty_in || tty_out || tty_err) {
        return Ok((None, None));
    }

    let path = if svc.tty_path.is_empty() {
        "/dev/console".to_string()
    } else {
        svc.tty_path.clone()
    };

    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) => {
            warn!(
                "Cannot open TTY {} for {} ({}): ignoring TTYPath",
                path, unit_name, e
            );
            return Ok((None, None));
        }
    };
    let base = file.into_raw_fd();

    // Redirect each requested stream to a duplicate of the TTY fd. On error,
    // close `base` before returning so we do not leak the descriptor.
    for (want, index) in [(tty_in, 0usize), (tty_out, 1), (tty_err, 2)] {
        if !want {
            continue;
        }
        let owned = match dup_fd(base) {
            Ok(fd) => fd,
            Err(e) => {
                unsafe {
                    libc::close(base);
                }
                return Err(e);
            }
        };
        let stdio = Stdio::from(owned);
        match index {
            0 => cmd.stdin(stdio),
            1 => cmd.stdout(stdio),
            _ => cmd.stderr(stdio),
        };
    }
    // In the child: become a session leader and acquire the TTY as the
    // controlling terminal. A failed TIOCSCTTY is non-fatal (matching
    // systemd's `StandardInput=tty`): the redirected streams still work
    // without a controlling terminal.
    let force = is_tty_force(&svc.standard_input)
        || is_tty_force(&svc.standard_output)
        || is_tty_force(&svc.standard_error);
    let pre: PreExecFn = Box::new(move || {
        unsafe {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let arg = if force { 1 } else { 0 };
            libc::ioctl(base, libc::TIOCSCTTY, arg);
            libc::close(base);
        }
        Ok(())
    });
    Ok((Some(base), Some(pre)))
}

#[cfg(not(unix))]
fn apply_tty(
    _cmd: &mut Command,
    _svc: &sysa::proto::ServiceConfig,
    _unit_name: &str,
) -> Result<(Option<i32>, Option<PreExecFn>)> {
    Ok((None, None))
}

// ---------------------------------------------------------------------------
// ExecStart parsing (systemd-compatible)
// ---------------------------------------------------------------------------

/// Flags collected from systemd-style prefix characters in `ExecStart=`.
#[derive(Debug, Clone, Default)]
struct ExecFlags {
    ignore_failure: bool,
    privileged: bool,
    custom_argv0: bool,
    no_env_expand: bool,
    no_new_privileges: bool,
    via_shell: bool,
}

/// Result of parsing an `ExecStart=` line.
#[derive(Debug, Clone)]
struct ParsedExec {
    program: String,
    args: Vec<String>,
    flags: ExecFlags,
}

/// Parse a raw `ExecStart=` string into program + args, after stripping
/// prefix characters, splitting tokens (systemd-compatible word splitting),
/// and expanding `%`-specifiers.
fn parse_exec_start(raw: &str, unit_name: &str) -> Result<ParsedExec> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!(sysa::l10n::t_("Empty ExecStart command."));
    }

    let (flags, rest) = strip_prefixes(raw);

    // Split the remainder into words (respects quotes, C-escapes).
    let words = split_words(rest);
    if words.is_empty() {
        bail!(sysa::l10n::t_(
            "Empty ExecStart command after prefix stripping."
        ));
    }

    // Expand % specifiers in each word.
    let words: Vec<String> = words
        .into_iter()
        .map(|w| expand_specifiers(&w, unit_name))
        .collect();

    if flags.custom_argv0 {
        // @ prefix: first word is argv[0], second word is the program.
        if words.len() < 2 {
            bail!(sysa::l10n::t_(
                "@ prefix requires at least two tokens (argv0 program)."
            ));
        }
        // We don't have a way to set argv[0] natively in tokio::process::Command,
        // so we just use the program as-is and note the custom argv0 in the log.
        debug!(
            "@ prefix: argv[0] would be '{}', using program '{}'",
            words[0], words[1]
        );
        Ok(ParsedExec {
            program: words[1].clone(),
            args: words[2..].to_vec(),
            flags,
        })
    } else {
        Ok(ParsedExec {
            program: words[0].clone(),
            args: words[1..].to_vec(),
            flags,
        })
    }
}

/// Strip systemd prefix characters from the beginning of a command string.
/// Returns (flags, remainder).
fn strip_prefixes(s: &str) -> (ExecFlags, &str) {
    let mut flags = ExecFlags::default();
    let mut cursor = s;

    loop {
        match cursor.as_bytes().first() {
            Some(b'-') => {
                flags.ignore_failure = true;
                cursor = &cursor[1..];
            }
            Some(b'+') => {
                flags.privileged = true;
                cursor = &cursor[1..];
            }
            Some(b'@') => {
                // The @ prefix means first token is used as argv[0].
                // We track it, but actual argv[0] manipulation is not
                // supported by tokio::process::Command on all platforms.
                flags.custom_argv0 = true;
                cursor = &cursor[1..];
            }
            Some(b':') => {
                flags.no_env_expand = true;
                cursor = &cursor[1..];
            }
            Some(b'!') => {
                flags.no_new_privileges = true;
                cursor = &cursor[1..];
                if cursor.as_bytes().first() == Some(&b'!') {
                    cursor = &cursor[1..];
                }
            }
            Some(b'|') => {
                flags.via_shell = true;
                cursor = &cursor[1..];
            }
            _ => break,
        }
    }

    (flags, cursor)
}

// ---------------------------------------------------------------------------
// Word splitting (systemd extract_first_word compatible)
// ---------------------------------------------------------------------------

/// Split a command-line string into words, respecting single/double quotes
/// and C-style escape sequences (\n, \t, \r, \\, \xNN, \uNNNN, \UNNNNNNNN).
///
/// This mirrors systemd's `extract_first_word` with the flags
/// `EXTRACT_UNQUOTE | EXTRACT_CUNESCAPE`.
fn split_words(s: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;

    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let ch = chars[i];

        if in_single {
            if ch == '\'' {
                in_single = false;
            } else {
                current.push(ch);
            }
            i += 1;
            continue;
        }

        if in_double {
            match ch {
                '\\' => {
                    if i + 1 < chars.len() {
                        i += 1;
                        let next = chars[i];
                        if matches!(next, '"' | '\\' | '$' | '`') {
                            current.push(next);
                        } else {
                            current.push('\\');
                            current.push(next);
                        }
                    } else {
                        current.push('\\');
                    }
                }
                '"' => {
                    in_double = false;
                }
                _ => {
                    current.push(ch);
                }
            }
            i += 1;
            continue;
        }

        match ch {
            ' ' | '\t' => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
                i += 1;
            }
            '\'' => {
                in_single = true;
                i += 1;
            }
            '"' => {
                in_double = true;
                i += 1;
            }
            '\\' => {
                if i + 1 < chars.len() {
                    i += 1;
                    let next = chars[i];
                    match next {
                        'a' => current.push('\u{0007}'),
                        'b' => current.push('\u{0008}'),
                        'f' => current.push('\u{000C}'),
                        'n' => current.push('\n'),
                        'r' => current.push('\r'),
                        't' => current.push('\t'),
                        'v' => current.push('\u{000B}'),
                        '\\' => current.push('\\'),
                        '\'' => current.push('\''),
                        '"' => current.push('"'),
                        'x' | 'X' => {
                            let (consumed, byte) = parse_hex_escape(&chars, i + 1);
                            if let Some(b) = byte {
                                current.push(b as char);
                                i += consumed;
                            } else {
                                current.push(next);
                            }
                        }
                        'u' => {
                            let (consumed, c) = parse_unicode_escape(&chars, i + 1, 4);
                            if let Some(c) = c {
                                current.push(c);
                                i += consumed;
                            } else {
                                current.push(next);
                            }
                        }
                        'U' => {
                            let (consumed, c) = parse_unicode_escape(&chars, i + 1, 8);
                            if let Some(c) = c {
                                current.push(c);
                                i += consumed;
                            } else {
                                current.push(next);
                            }
                        }
                        other => {
                            current.push(other);
                        }
                    }
                } else {
                    current.push('\\');
                }
                i += 1;
            }
            other => {
                current.push(other);
                i += 1;
            }
        }
    }

    if !current.is_empty() {
        words.push(current);
    }

    words
}

/// Parse \xNN hex escape starting at index `start` in `chars`.
fn parse_hex_escape(chars: &[char], start: usize) -> (usize, Option<u8>) {
    if start + 1 >= chars.len() {
        return (0, None);
    }
    let hex: String = chars[start..].iter().take(2).collect();
    if hex.len() < 2 {
        return (0, None);
    }
    u8::from_str_radix(&hex, 16)
        .ok()
        .map(|b| (2, Some(b)))
        .unwrap_or((0, None))
}

/// Parse \uNNNN or \UNNNNNNNN unicode escape.
fn parse_unicode_escape(chars: &[char], start: usize, digits: usize) -> (usize, Option<char>) {
    if start + digits > chars.len() {
        return (0, None);
    }
    let hex: String = chars[start..start + digits].iter().collect();
    match u32::from_str_radix(&hex, 16).ok() {
        Some(code) => char::from_u32(code)
            .map(|c| (digits, Some(c)))
            .unwrap_or((0, None)),
        None => (0, None),
    }
}

// ---------------------------------------------------------------------------
// % specifier expansion
// ---------------------------------------------------------------------------

/// Expand systemd `%`-specifiers in `s` using the given unit `name`.
fn expand_specifiers(s: &str, name: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }

    let unit_name = name;
    let unit_no_ext = name.rsplit_once('.').map(|(p, _)| p).unwrap_or(name);
    let (prefix, instance) = if let Some(at_pos) = unit_no_ext.find('@') {
        let p = &unit_no_ext[..at_pos];
        let i = &unit_no_ext[at_pos + 1..];
        (p, i)
    } else {
        (unit_no_ext, "")
    };

    let hostname = || {
        std::process::Command::new("hostname")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };
    let machine_id = || {
        std::fs::read_to_string(sysa::paths::instance().systemd_machine_id_file)
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };
    let kernel_release = || {
        std::process::Command::new("uname")
            .arg("-r")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };

    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            None => out.push('%'),
            Some('n') => out.push_str(unit_name),
            Some('N') => out.push_str(unit_no_ext),
            Some('p') => out.push_str(prefix),
            Some('i') => out.push_str(instance),
            Some('u') => {
                let user = std::env::var("USER")
                    .or_else(|_| std::env::var("LOGNAME"))
                    .unwrap_or_default();
                out.push_str(&user);
            }
            Some('U') => {
                let uid = std::process::Command::new("id")
                    .arg("-u")
                    .output()
                    .ok()
                    .and_then(|o| String::from_utf8(o.stdout).ok())
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default();
                out.push_str(&uid);
            }
            Some('g') => {
                let group = std::env::var("GROUP")
                    .or_else(|_| {
                        std::process::Command::new("id")
                            .arg("-gn")
                            .output()
                            .ok()
                            .and_then(|o| String::from_utf8(o.stdout).ok())
                            .map(|s| s.trim().to_string())
                            .ok_or(std::env::VarError::NotPresent)
                    })
                    .unwrap_or_default();
                out.push_str(&group);
            }
            Some('G') => {
                let gid = std::process::Command::new("id")
                    .arg("-g")
                    .output()
                    .ok()
                    .and_then(|o| String::from_utf8(o.stdout).ok())
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default();
                out.push_str(&gid);
            }
            Some('H') => out.push_str(&hostname()),
            Some('m') => out.push_str(&machine_id()),
            Some('v') => out.push_str(&kernel_release()),
            Some('%') => out.push('%'),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// $VAR / ${VAR} environment variable expansion
// ---------------------------------------------------------------------------

/// Build a lookup table from the unit's `Environment=` settings and the PAM
/// environment (order: process env, unit env, PAM env, forced env — later
/// entries win).
fn build_env_table(unit_env: &[String], pam_env: &[(String, String)]) -> HashMap<String, String> {
    let mut table: HashMap<String, String> = HashMap::new();

    for (key, val) in std::env::vars() {
        table.insert(key, val);
    }

    for entry in unit_env {
        if let Some((key, val)) = entry.split_once('=') {
            table.insert(key.to_string(), val.to_string());
        }
    }

    for (key, val) in pam_env {
        table.insert(key.clone(), val.clone());
    }

    for (key, val) in FORCED_SERVICE_ENV {
        table.insert(key.to_string(), val.to_string());
    }

    table
}

/// Whether `XDG_RUNTIME_DIR` should fall back to `/run/user/<uid>`.
///
/// `pam_systemd` normally exports this via the PAM environment and
/// systemd --user refuses to run without it ("Trying to run as user
/// instance, but $XDG_RUNTIME_DIR is not set.").  When the PAM setup was
/// skipped or failed (e.g. a missing `/etc/pam.d/systemd-user`), fall
/// back to the user's runtime directory if it exists, mirroring what
/// pam_systemd would have set.  A unit-provided `Environment=` value takes
/// precedence over the fallback.
fn xdg_runtime_dir_fallback(
    unit_env: &[String],
    pam_env: &[(String, String)],
    uid: Option<libc::uid_t>,
) -> Option<(String, String)> {
    if pam_env.iter().any(|(k, _)| k == "XDG_RUNTIME_DIR")
        || unit_env.iter().any(|e| e.starts_with("XDG_RUNTIME_DIR="))
    {
        return None;
    }
    let uid = uid?;
    let dir = format!("/run/user/{uid}");
    if std::path::Path::new(&dir).is_dir() {
        Some(("XDG_RUNTIME_DIR".to_string(), dir))
    } else {
        None
    }
}

/// Expand `$VAR` / `${VAR}` in an argv, handling:
///
/// - Standalone `$VAR` (exact word = `$NAME`): value is split by whitespace
///   into multiple argv entries.
/// - `${VAR}` / `$VAR` inline: expanded in-place.
/// - `${VAR:-default}` / `${VAR:+alternate}`: default/alternate value.
fn expand_argv(args: &[String], env: &HashMap<String, String>) -> Vec<String> {
    let mut result = Vec::with_capacity(args.len());

    for arg in args {
        if is_standalone_var_ref(arg) {
            let name = &arg[1..];
            match env.get(name) {
                Some(value) if !value.is_empty() => {
                    for val in value.split_whitespace() {
                        result.push(val.to_string());
                    }
                }
                _ => {
                    // Unset or empty → skip (systemd behaviour).
                }
            }
        } else {
            result.push(expand_env_in_word(arg, env));
        }
    }

    result
}

/// Check if a word is exactly `$VARNAME` (no braces, no surrounding text).
fn is_standalone_var_ref(word: &str) -> bool {
    let bytes = word.as_bytes();
    if bytes.len() < 2 || bytes[0] != b'$' {
        return false;
    }
    if bytes[1] == b'{' {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|&b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Expand `${VAR}`, `${VAR:-default}`, `${VAR:+alternate}`, `$VAR`, `$$` within
/// a single word.  This mirrors systemd's `replace_env_full`.
fn expand_env_in_word(word: &str, env: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(word.len());
    let mut chars = word.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch != '$' {
            out.push(ch);
            continue;
        }

        if chars.peek() == Some(&'$') {
            chars.next();
            out.push('$');
            continue;
        }

        if chars.peek() == Some(&'{') {
            chars.next();
            expand_braced_expr(&mut out, &mut chars, env);
            continue;
        }

        // $VAR — simple variable name.
        let mut name = String::new();
        while let Some(&c) = chars.peek() {
            if c.is_alphanumeric() || c == '_' {
                name.push(c);
                chars.next();
            } else {
                break;
            }
        }

        if name.is_empty() {
            out.push('$');
            continue;
        }

        out.push_str(&env.get(&name).cloned().unwrap_or_default());
    }

    out
}

/// Expand `${...}` expression: `${VAR}`, `${VAR:-default}`, `${VAR:+alternate}`.
fn expand_braced_expr(
    out: &mut String,
    chars: &mut std::iter::Peekable<std::str::Chars>,
    env: &HashMap<String, String>,
) {
    let mut name = String::new();
    let mut substitution: Option<(char, String)> = None;

    enum Phase {
        Name,
        Default,
    }
    let mut phase = Phase::Name;
    let mut op = ' '; // '-' or '+'

    loop {
        match chars.next() {
            None => {
                out.push_str("${");
                out.push_str(&name);
                if let Some((_, ref val)) = substitution {
                    out.push(':');
                    out.push_str(val);
                }
                break;
            }
            Some('}') => {
                let resolved = match phase {
                    Phase::Name => env.get(&name).cloned().unwrap_or_default(),
                    Phase::Default => {
                        if op == '-' {
                            env.get(&name)
                                .map(|v| {
                                    if v.is_empty() {
                                        substitution
                                            .as_ref()
                                            .map(|(_, d)| d.clone())
                                            .unwrap_or_default()
                                    } else {
                                        v.clone()
                                    }
                                })
                                .unwrap_or_else(|| {
                                    substitution
                                        .as_ref()
                                        .map(|(_, d)| d.clone())
                                        .unwrap_or_default()
                                })
                        } else {
                            // '+'
                            if env.get(&name).is_some_and(|v| !v.is_empty()) {
                                substitution
                                    .as_ref()
                                    .map(|(_, a)| a.clone())
                                    .unwrap_or_default()
                            } else {
                                String::new()
                            }
                        }
                    }
                };
                out.push_str(&resolved);
                break;
            }
            Some(':') if matches!(phase, Phase::Name) => {
                if let Some(&c @ ('-' | '+')) = chars.peek() {
                    op = c;
                    phase = Phase::Default;
                    chars.next();
                } else {
                    name.push(':');
                }
            }
            Some(ch) if matches!(phase, Phase::Name) => {
                name.push(ch);
            }
            Some(ch) if matches!(phase, Phase::Default) => match substitution {
                Some((_opchar, ref mut val)) => {
                    val.push(ch);
                }
                None => {
                    let mut val = String::new();
                    val.push(ch);
                    substitution = Some((op, val));
                }
            },
            _ => unreachable!(),
        }
    }
}

/// Build the command line string for `sh -c` invocation (| prefix).
fn build_shell_command_line(program: &str, args: &[String]) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(1 + args.len());
    parts.push(quote_for_shell(program));
    for arg in args {
        parts.push(quote_for_shell(arg));
    }
    parts.join(" ")
}

/// Quote a string for shell consumption (single-quote wrapping).
fn quote_for_shell(s: &str) -> String {
    if s.contains('\'') {
        let mut out = String::new();
        out.push('\'');
        for ch in s.chars() {
            if ch == '\'' {
                out.push_str("'\\''");
            } else {
                out.push(ch);
            }
        }
        out.push('\'');
        out
    } else {
        format!("'{}'", s)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- Word splitting ---

    #[test]
    fn split_simple_words() {
        assert_eq!(split_words("foo bar baz"), vec!["foo", "bar", "baz"]);
    }

    #[test]
    fn split_single_quoted() {
        assert_eq!(split_words("foo 'bar baz'"), vec!["foo", "bar baz"]);
    }

    #[test]
    fn split_double_quoted() {
        assert_eq!(split_words("foo \"bar baz\""), vec!["foo", "bar baz"]);
    }

    #[test]
    fn split_mixed_quotes() {
        assert_eq!(
            split_words("foo 'bar \"baz\"' \"qux 'quux'\""),
            vec!["foo", "bar \"baz\"", "qux 'quux'"]
        );
    }

    #[test]
    fn split_backslash_escape() {
        assert_eq!(split_words(r"foo\ bar"), vec!["foo bar"]);
    }

    #[test]
    fn split_c_escape_newline() {
        assert_eq!(split_words(r"foo\nbar"), vec!["foo\nbar"]);
    }

    #[test]
    fn split_c_escape_tab() {
        assert_eq!(split_words(r"foo\tbar"), vec!["foo\tbar"]);
    }

    #[test]
    fn split_c_escape_hex() {
        assert_eq!(split_words(r"foo\x20bar"), vec!["foo bar"]);
    }

    #[test]
    fn split_c_escape_unicode() {
        assert_eq!(split_words(r"foo\u0020bar"), vec!["foo bar"]);
    }

    #[test]
    fn double_quote_backslash_escapes() {
        assert_eq!(split_words(r#""foo\"bar""#), vec!["foo\"bar"]);
        assert_eq!(split_words(r#""foo\\bar""#), vec!["foo\\bar"]);
    }

    #[test]
    fn double_quote_backslash_n_is_literal() {
        assert_eq!(split_words(r#""foo\nbar""#), vec![r"foo\nbar"]);
    }

    #[test]
    fn split_tabs_as_separators() {
        assert_eq!(split_words("foo\tbar"), vec!["foo", "bar"]);
    }

    // --- Prefix stripping ---

    #[test]
    fn strip_ignore_failure() {
        let (f, rest) = strip_prefixes("-/usr/bin/foo");
        assert!(f.ignore_failure);
        assert_eq!(rest, "/usr/bin/foo");
    }

    #[test]
    fn strip_privileged() {
        let (f, rest) = strip_prefixes("+/usr/bin/foo");
        assert!(f.privileged);
        assert_eq!(rest, "/usr/bin/foo");
    }

    #[test]
    fn strip_via_shell() {
        let (f, rest) = strip_prefixes("|/usr/bin/foo arg");
        assert!(f.via_shell);
        assert_eq!(rest, "/usr/bin/foo arg");
    }

    #[test]
    fn strip_no_env_expand() {
        let (f, rest) = strip_prefixes(":/usr/bin/foo");
        assert!(f.no_env_expand);
        assert_eq!(rest, "/usr/bin/foo");
    }

    #[test]
    fn strip_combined() {
        let (f, rest) = strip_prefixes("-+/usr/bin/foo");
        assert!(f.ignore_failure);
        assert!(f.privileged);
        assert_eq!(rest, "/usr/bin/foo");
    }

    #[test]
    fn strip_double_bang() {
        let (f, rest) = strip_prefixes("!!/usr/bin/foo");
        assert!(f.no_new_privileges);
        assert_eq!(rest, "/usr/bin/foo");
    }

    #[test]
    fn strip_at_prefix() {
        let (f, rest) = strip_prefixes("@/usr/lib/foo/foo myapp");
        assert!(f.custom_argv0);
        assert_eq!(rest, "/usr/lib/foo/foo myapp");
    }

    #[test]
    fn strip_all_prefixes() {
        let (f, rest) = strip_prefixes("-+@:/usr/bin/foo");
        assert!(f.ignore_failure);
        assert!(f.privileged);
        assert!(f.custom_argv0);
        assert!(f.no_env_expand);
        assert_eq!(rest, "/usr/bin/foo");
    }

    // --- % specifier expansion ---

    #[test]
    fn expand_percent_n() {
        assert_eq!(expand_specifiers("%n", "sshd.service"), "sshd.service");
    }

    #[test]
    fn expand_percent_capital_n() {
        assert_eq!(expand_specifiers("%N", "sshd.service"), "sshd");
    }

    #[test]
    fn expand_percent_p() {
        assert_eq!(expand_specifiers("%p", "sshd.service"), "sshd");
    }

    #[test]
    fn expand_percent_i() {
        assert_eq!(expand_specifiers("%i", "getty@tty1.service"), "tty1");
    }

    #[test]
    fn expand_percent_percent() {
        assert_eq!(expand_specifiers("%%", "foo.service"), "%");
    }

    #[test]
    fn expand_percent_noop() {
        assert_eq!(
            expand_specifiers("hello world", "foo.service"),
            "hello world"
        );
    }

    #[test]
    fn expand_percent_unknown_is_literal() {
        assert_eq!(expand_specifiers("%z", "foo.service"), "%z");
    }

    // --- $VAR expansion ---

    #[test]
    fn expand_simple_var() {
        let mut env = HashMap::new();
        env.insert("FOO".to_string(), "bar".to_string());
        assert_eq!(expand_env_in_word("$FOO", &env), "bar");
    }

    #[test]
    fn expand_braced_var() {
        let mut env = HashMap::new();
        env.insert("FOO".to_string(), "bar".to_string());
        assert_eq!(expand_env_in_word("${FOO}", &env), "bar");
    }

    #[test]
    fn expand_dollar_dollar() {
        let env = HashMap::new();
        assert_eq!(expand_env_in_word("$$", &env), "$");
    }

    #[test]
    fn expand_default_value_unset() {
        let env: HashMap<String, String> = HashMap::new();
        assert_eq!(expand_env_in_word("${UNDEF:-default}", &env), "default");
    }

    #[test]
    fn expand_default_value_set() {
        let mut env = HashMap::new();
        env.insert("DEFINED".to_string(), "value".to_string());
        assert_eq!(expand_env_in_word("${DEFINED:-default}", &env), "value");
    }

    #[test]
    fn expand_alternate_value_set() {
        let mut env = HashMap::new();
        env.insert("FOO".to_string(), "bar".to_string());
        assert_eq!(expand_env_in_word("${FOO:+alt}", &env), "alt");
    }

    #[test]
    fn expand_alternate_value_unset() {
        let env: HashMap<String, String> = HashMap::new();
        assert_eq!(expand_env_in_word("${UNDEF:+alt}", &env), "");
    }

    #[test]
    fn expand_inline_var() {
        let mut env = HashMap::new();
        env.insert("PORT".to_string(), "2222".to_string());
        assert_eq!(expand_env_in_word("-p${PORT}", &env), "-p2222");
    }

    #[test]
    fn expand_unset_var_is_empty() {
        let env: HashMap<String, String> = HashMap::new();
        assert_eq!(expand_env_in_word("$UNDEF", &env), "");
    }

    #[test]
    fn expand_standalone_splitting() {
        let mut env = HashMap::new();
        env.insert("OPTS".to_string(), "-o Port=2222 -v".to_string());
        let args = vec!["$OPTS".to_string()];
        let expanded = expand_argv(&args, &env);
        assert_eq!(expanded, vec!["-o", "Port=2222", "-v"]);
    }

    #[test]
    fn expand_standalone_unset_skips() {
        let env: HashMap<String, String> = HashMap::new();
        let args = vec!["$UNDEF".to_string()];
        let expanded = expand_argv(&args, &env);
        let expected: Vec<String> = vec![];
        assert_eq!(expanded, expected);
    }

    #[test]
    fn expand_inline_does_not_split() {
        let mut env = HashMap::new();
        env.insert("PORT".to_string(), "2222 3333".to_string());
        // Inline ${VAR} is expanded in-place, not split.
        let args = vec!["-p${PORT}".to_string()];
        let expanded = expand_argv(&args, &env);
        assert_eq!(expanded, vec!["-p2222 3333"]);
    }

    // --- Full parse_exec_start ---

    #[test]
    fn parse_simple_command() {
        let p = parse_exec_start("/usr/bin/foo bar baz", "test.service").unwrap();
        assert_eq!(p.program, "/usr/bin/foo");
        assert_eq!(p.args, vec!["bar", "baz"]);
        assert!(!p.flags.ignore_failure);
        assert!(!p.flags.via_shell);
    }

    #[test]
    fn parse_with_prefixes() {
        let p = parse_exec_start("-+/usr/bin/foo arg", "test.service").unwrap();
        assert_eq!(p.program, "/usr/bin/foo");
        assert_eq!(p.args, vec!["arg"]);
        assert!(p.flags.ignore_failure);
        assert!(p.flags.privileged);
    }

    #[test]
    fn parse_via_shell() {
        let p = parse_exec_start("|/usr/bin/foo bar", "test.service").unwrap();
        assert_eq!(p.program, "/usr/bin/foo");
        assert!(p.flags.via_shell);
    }

    #[test]
    fn parse_no_env_expand() {
        let p = parse_exec_start(":/usr/bin/foo $VAR", "test.service").unwrap();
        assert!(p.flags.no_env_expand);
    }

    // --- Shell quoting ---

    #[test]
    fn shell_quote_simple() {
        assert_eq!(quote_for_shell("hello"), "'hello'");
    }

    #[test]
    fn shell_quote_with_single_quote() {
        assert_eq!(quote_for_shell("it's"), "'it'\\''s'");
    }

    // --- is_standalone_var_ref ---

    #[test]
    fn standalone_var_ref_simple() {
        assert!(is_standalone_var_ref("$FOO"));
        assert!(is_standalone_var_ref("$FOO_BAR"));
        assert!(is_standalone_var_ref("$FOO1"));
    }

    #[test]
    fn standalone_var_ref_not() {
        assert!(!is_standalone_var_ref("${FOO}"));
        assert!(!is_standalone_var_ref("$"));
        assert!(!is_standalone_var_ref("$"));
        assert!(!is_standalone_var_ref("x$FOO"));
    }

    // --- Empty string ---

    #[test]
    fn parse_empty_fails() {
        assert!(parse_exec_start("", "test.service").is_err());
    }

    // --- TTY stdio mode detection ---

    #[cfg(unix)]
    #[test]
    fn tty_mode_detection() {
        assert!(is_tty_mode("tty"));
        assert!(is_tty_mode("tty-force"));
        assert!(is_tty_mode("tty-fail"));
        assert!(is_tty_mode("tty-sockets"));
        assert!(is_tty_mode("TTY"));
        assert!(!is_tty_mode("journal"));
        assert!(!is_tty_mode("inherit"));
        assert!(!is_tty_mode(""));
        assert!(is_tty_force("tty-force"));
        assert!(is_tty_force("TTY-FORCE"));
        assert!(!is_tty_force("tty"));
        assert!(!is_tty_force(""));
    }

    // --- Forced service environment ---

    #[test]
    fn forced_env_has_bus_variables() {
        let vars: HashMap<&str, &str> = FORCED_SERVICE_ENV.iter().copied().collect();
        assert_eq!(vars.get("SYSTEMCTL_FORCE_BUS"), Some(&"1"));
        assert_eq!(vars.get("SYSTEMD_OFFLINE"), Some(&"0"));
    }

    #[test]
    fn forced_env_overrides_unit_environment_in_table() {
        let unit_env = vec![
            "SYSTEMCTL_FORCE_BUS=0".to_string(),
            "SYSTEMD_OFFLINE=1".to_string(),
        ];
        let table = build_env_table(&unit_env, &[]);
        assert_eq!(
            table.get("SYSTEMCTL_FORCE_BUS").map(String::as_str),
            Some("1")
        );
        assert_eq!(table.get("SYSTEMD_OFFLINE").map(String::as_str), Some("0"));
    }

    #[test]
    fn pam_env_override_unit_environment_in_table() {
        let unit_env = vec!["XDG_RUNTIME_DIR=/tmp/unit".to_string()];
        let pam_env = vec![("XDG_RUNTIME_DIR".to_string(), "/run/user/1000".to_string())];
        let table = build_env_table(&unit_env, &pam_env);
        assert_eq!(
            table.get("XDG_RUNTIME_DIR").map(String::as_str),
            Some("/run/user/1000")
        );
    }

    #[test]
    fn forced_env_overrides_pam_environment_in_table() {
        let pam_env = vec![
            ("SYSTEMCTL_FORCE_BUS".to_string(), "0".to_string()),
            ("SYSTEMD_OFFLINE".to_string(), "1".to_string()),
        ];
        let table = build_env_table(&[], &pam_env);
        assert_eq!(
            table.get("SYSTEMCTL_FORCE_BUS").map(String::as_str),
            Some("1")
        );
        assert_eq!(table.get("SYSTEMD_OFFLINE").map(String::as_str), Some("0"));
    }

    // --- Credential resolution ---

    #[test]
    #[cfg(unix)]
    fn resolve_no_credentials_is_none() {
        let c = resolve_credentials("", "").unwrap();
        assert!(c.is_none());
    }

    #[test]
    #[cfg(unix)]
    fn resolve_named_user() {
        let c = resolve_credentials("root", "").unwrap().unwrap();
        assert_eq!(c.uid, 0);
        assert_eq!(c.gid, 0);
        assert_eq!(c.username.as_deref(), Some("root"));
    }

    #[test]
    #[cfg(unix)]
    fn resolve_numeric_uid_gets_canonical_name() {
        // `User=1000`-style numeric UIDs resolve via getpwuid and must yield
        // the canonical pw_name (systemd get_user_creds() behaviour) — that
        // name is what PAM receives (pam_systemd rejects numeric usernames).
        let me = nix::unistd::Uid::current();
        let pw = nix::unistd::User::from_uid(me).unwrap().unwrap();
        let c = resolve_credentials(&me.as_raw().to_string(), "")
            .unwrap()
            .unwrap();
        assert_eq!(c.uid, pw.uid.as_raw());
        assert_eq!(c.gid, pw.gid.as_raw());
        assert_eq!(c.username.as_deref(), Some(pw.name.as_str()));
    }

    #[test]
    #[cfg(unix)]
    fn resolve_group_only() {
        let c = resolve_credentials("", "root").unwrap().unwrap();
        assert_eq!(c.gid, 0);
        assert_eq!(c.uid, 0);
        assert_eq!(c.username, None);
    }

    #[test]
    #[cfg(unix)]
    fn resolve_missing_user_fails() {
        assert!(resolve_credentials("systema-no-such-user-xyz", "").is_err());
    }

    // --- XDG_RUNTIME_DIR fallback ---

    #[test]
    fn xdg_runtime_dir_fallback_missing_dir_is_none() {
        let r = xdg_runtime_dir_fallback(&[], &[], Some(999_999));
        assert_eq!(r, None);
    }

    #[test]
    fn xdg_runtime_dir_fallback_respects_unit_env() {
        let r = xdg_runtime_dir_fallback(&["XDG_RUNTIME_DIR=/custom".to_string()], &[], Some(0));
        assert_eq!(r, None);
    }

    #[test]
    fn xdg_runtime_dir_fallback_respects_pam_env() {
        let r = xdg_runtime_dir_fallback(
            &[],
            &[("XDG_RUNTIME_DIR".to_string(), "/run/user/0".to_string())],
            Some(0),
        );
        assert_eq!(r, None);
    }

    #[test]
    fn xdg_runtime_dir_fallback_uses_existing_dir() {
        let uid = nix::unistd::Uid::current();
        let dir = format!("/run/user/{}", uid.as_raw());
        if !std::path::Path::new(&dir).is_dir() {
            // Environment without a runtime dir (e.g. CI container); the
            // fallback must then be None, not an invented path.
            assert_eq!(xdg_runtime_dir_fallback(&[], &[], Some(uid.as_raw())), None);
            return;
        }
        assert_eq!(
            xdg_runtime_dir_fallback(&[], &[], Some(uid.as_raw())),
            Some(("XDG_RUNTIME_DIR".to_string(), dir))
        );
    }
}
