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

/// Whether [`FORCED_SERVICE_ENV`] applies to a service running as `user`
/// (empty `User=`, `root` or uid `0` all mean the system domain's root).
///
/// User-land services must not get the forced system-bus environment —
/// their `systemctl` talks to a user manager, not the system bus (see the
/// call site in the spawn path for the user@.service failure mode).
fn forced_bus_env_applies(user: &str) -> bool {
    user.is_empty() || user == "root" || user == "0"
}

/// Directory where per-service stdout/stderr logs are captured.
///
/// `/var/log/services` in the system domain.  The per-user instance
/// (`systema-sysu`) points `SYSTEMA_SERVICE_LOG_DIR` at a user-writable
/// directory, since an unprivileged System S cannot create anything under
/// `/var/log`.  Resolved once, lazily, so the value a unit's `Environment=`
/// tries to set is not consulted (the manager's environment is).
fn service_log_dir() -> &'static str {
    static DIR: OnceLock<String> = OnceLock::new();
    DIR.get_or_init(|| {
        std::env::var("SYSTEMA_SERVICE_LOG_DIR")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "/var/log/services".to_string())
    })
}

/// Lazily create the service log directory (`/var/log/services/` by
/// default) with world-readable permissions (0755).  Returns `Ok(())` on
/// success or if the directory already exists; logs a warning and returns
/// `Ok(())` on failure so that a log-directory issue never prevents a
/// service from starting.
fn ensure_service_log_dir() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        let dir = service_log_dir();
        if let Err(e) = std::fs::create_dir_all(dir) {
            warn!("Failed to create {}: {}", dir, e);
        } else {
            // Ensure the directory is world-readable and traversable.
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755));
        }
    });
}

/// Open a log file at `<service-log-dir>/<name>.log` in append mode with
/// 0644 permissions (world-readable).  Returns `None` on failure so that
/// log-file issues never prevent a service from starting.
fn open_service_log(name: &str) -> Option<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    let path = format!("{}/{}.log", service_log_dir(), name);
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

    // Open the PAM session in a dedicated forked holder child (the
    // "(syss-pam)" helper; PAMName= + User= required, mirroring systemd's
    // exec-invoke.c `setup_pam()` trigger).  The child runs the stack in
    // the only execution context the kernel grants loginuid writes (see
    // pam.rs), parks holding the session for the service's lifetime and
    // ships `pam_getenvlist()` back over a pipe; it tears the session down
    // in its own context when this service's main process exits.
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
        let pam_started = std::time::Instant::now();
        match tokio::task::spawn_blocking(move || {
            crate::pam::pam_session_open(&pam_name_task, &username_task, tty.as_deref())
        })
        .await
        {
            Ok(Ok((env, session))) => {
                pam_env = env;
                pam_session = Some(session);
                info!(
                    "Opened PAM session '{}' for user '{}' ({}): elapsed={}ms",
                    pam_name,
                    username,
                    unit_name,
                    pam_started.elapsed().as_millis()
                );
            }
            Ok(Err(e)) => {
                warn!(
                    "PAM setup for {} ({}) failed after {}ms: {}; continuing without PAM environment",
                    unit_name,
                    pam_name,
                    pam_started.elapsed().as_millis(),
                    e
                );
            }
            Err(e) => {
                warn!(
                    "PAM setup task for {} failed after {}ms: {}",
                    unit_name,
                    pam_started.elapsed().as_millis(),
                    e
                );
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

    // Build environment lookup table (process env + passwd identity env +
    // unit Environment= + PAM env + forced env).
    let identity_env = identity_env(creds.as_ref());
    let directory_env = prepare_service_directories(svc, creds.as_ref())?;
    let env_table = build_env_table(&svc.environment, &pam_env, &identity_env, &directory_env);

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
    // Passwd identity env ($USER/$LOGNAME/$HOME/$SHELL, systemd.exec(5))
    // goes in FIRST: later entries win, so the unit's own Environment= (and
    // the PAM environment) can still override them.
    for (key, val) in &identity_env {
        if let (Ok(k), Ok(v)) = (CString::new(key.as_str()), CString::new(val.as_str())) {
            child_envs.push((k, v));
        }
    }
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
    // Directory-directive env ($STATE_DIRECTORY & co): manager-synthesized
    // exec-run metadata, applied like INVOCATION_ID below so a stale unit
    // Environment= assignment cannot mask the real directories.
    for (key, val) in &directory_env {
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
    //
    // Root services only: a user-land service's `systemctl` must reach its
    // own user manager via the private socket / user bus.  Forcing the
    // system bus there stalls the caller — inside the user manager this
    // poisons the `ExecStartPost=systemctl --user set-environment` of
    // Debian's dbus.socket/ssh-agent.socket, whose stuck start jobs then
    // block basic.target and keep `systemd --user` from ever reporting
    // READY=1 (every start job times out).
    if forced_bus_env_applies(&svc.user) {
        for (key, val) in FORCED_SERVICE_ENV {
            if let (Ok(k), Ok(v)) = (CString::new(*key), CString::new(*val)) {
                child_envs.push((k, v));
            }
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
    // A non-empty UnitConfig.cgroup_path (Delegate= prepared by System R)
    // moves the child into its delegated cgroup while still root, and
    // User=/Group= credentials are applied last, in the child.
    let sa_pre_exec = build_pre_exec(child_envs, &listen_fds, creds, config.cgroup_path.clone());
    // Whether the TTY hook below acquires a new session for the child.  That
    // is mutually exclusive with `process_group(0)`: `setsid()` fails with
    // EPERM when the caller is already a process group leader, so the two
    // would fight over the same kernel slot and a TTY unit could not start.
    let tty_owns_the_group = tty_pre_exec.is_some();
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

    // Own process group: a stop tears the whole tree down (systemd's default
    // KillMode=control-group), and the process-group fallback kills with
    // killpg(), which requires the main process to be its own group leader —
    // process_group(0) is setpgid(0,0) in the child.  The TTY branch instead
    // calls setsid() in its pre_exec hook, which yields `pgid == pid` as a
    // side effect (and would fail with EPERM if this ran first), so exactly
    // one of the two paths claims the group and the stop-time fallback may
    // always killpg(main_pid).
    if !tty_owns_the_group {
        cmd.process_group(0);
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

    // Keep the PAM holder child parked until the service's main process
    // exits; then deliver EOF via the lifecycle pipe, so the parked child
    // tears the PAM session down in its own (opening) context, mirroring
    // systemd's "(sd-pam)" teardown sequence.
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

/// The main process's process group — but only when the main process is
/// still its own group leader (`pid == pgid`; the spawn path guarantees it
/// via `process_group(0)` / the TTY branch's `setsid()`).  A stale main pid
/// whose leader slot was recycled must never drag an unrelated group into
/// the kill.
#[cfg(unix)]
fn group_leader_of(pid: u32) -> Option<nix::unistd::Pid> {
    use nix::unistd::Pid;
    let pgid = nix::unistd::getpgid(Some(Pid::from_raw(pid as i32)));
    match pgid {
        Ok(pgid) if pgid == Pid::from_raw(pid as i32) => Some(pgid),
        _ => None,
    }
}

/// Whether the unit's cgroup subtree looks empty.  Prefer the `populated`
/// flag (it also covers child cgroups; `cgroup.procs` at the root lists
/// direct members only) and fall back to the direct process list on older
/// kernels.  No path at all — or an unreadable cgroup — means "nothing
/// there to wait for"; the main pid check guards that side.
fn cgroup_tree_clear(path: Option<&str>) -> bool {
    match path {
        None => true,
        Some(p) => match crate::cgroup::populated(p) {
            Some(v) => !v,
            None => crate::cgroup::read_pids(p).is_empty(),
        },
    }
}

/// Stop a running service: SIGTERM the whole unit cgroup when its path is
/// known (catching children that escaped the main process group), degrade
/// to the classic pid/process-group teardown otherwise, and SIGKILL the
/// rest after the stop timeout.
///
/// The cgroup path is remembered per unit (registry), so only the first
/// stop pays the lookup round trip, and even that one never waits longer
/// than the querier's budget before falling back.
pub async fn stop_service(
    registry: ServiceRegistry,
    unit_name: &str,
    timeout_secs: u32,
    querier: Option<&crate::cgroup::Querier>,
) -> Result<()> {
    // Phase 0 — peek at the instance without disturbing it: `Stopping` is
    // only set after the (budget-bounded) cgroup lookup, so a state reader
    // never sees "stopping" while the process is still fully alive.  A
    // cached path short-circuits the lookup below.
    let (pid, cached_cgroup) = {
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
                (inst.main_pid, inst.cgroup_path.clone())
            }
        }
    };

    // Phase 1 — learn the unit's cgroup for this stop.  `None` after a
    // failed/absent lookup: killing degrades to the process group below.
    let cgroup_path = match querier {
        Some(q) => q.resolve(unit_name, cached_cgroup).await,
        None => None,
    };
    // Remember the path for later stops (it is stable for the unit).
    if cgroup_path.is_some() {
        let mut reg = registry.lock();
        if let Some(inst) = reg.get_mut(unit_name) {
            inst.cgroup_path = cgroup_path.clone();
        }
    }

    // Phase 2 — mark the unit as stopping only now.
    {
        let mut reg = registry.lock();
        if let Some(inst) = reg.get_mut(unit_name) {
            inst.state = ServiceState::Stopping;
        }
    }

    #[cfg(not(unix))]
    {
        let _ = &cgroup_path;
        warn!("Signal delivery not supported on this platform");
    }

    #[cfg(unix)]
    {
        use nix::sys::signal::{self, Signal};
        use nix::unistd::Pid;

        // Phase 3 — SIGTERM the whole tree.  The main process group covers
        // the ordinary children; the cgroup's process list adds members
        // that escaped the group (deduped against the main pid, which
        // `killpg`/`kill` already reached).
        let cgroup_pids: Vec<u32> = cgroup_path
            .as_deref()
            .map(crate::cgroup::read_pids)
            .unwrap_or_default();
        if let Some(pid) = pid {
            match group_leader_of(pid) {
                Some(pgid) => {
                    let _ = signal::killpg(pgid, Signal::SIGTERM);
                }
                None => {
                    let _ = signal::kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
                }
            }
            info!("Sent SIGTERM to PID {} ({})", pid, unit_name);
        }
        let extra_cgroup_pids: Vec<u32> = cgroup_pids
            .iter()
            .copied()
            .filter(|p| Some(*p) != pid)
            .collect();
        for p in &extra_cgroup_pids {
            let _ = signal::kill(Pid::from_raw(*p as i32), Signal::SIGTERM);
        }
        if !extra_cgroup_pids.is_empty() {
            debug!(
                "Sent SIGTERM to {} cgroup process(es) of {} on top of the process group",
                extra_cgroup_pids.len(),
                unit_name
            );
        }

        // Phase 4 — wait for the main process to die AND the unit's cgroup
        // subtree to empty out (the whole point of cgroup-based stops: a
        // service is only "stopped" when nothing of its tree survives).
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs.max(1) as u64);
        loop {
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            let main_gone = pid.map_or(true, |p| !is_alive(p));
            if main_gone && cgroup_tree_clear(cgroup_path.as_deref()) {
                info!("Service {} exited after SIGTERM", unit_name);
                break;
            }
            if std::time::Instant::now() >= deadline {
                warn!(
                    "Service {} (PID {}) did not exit in {}s; SIGKILLing the unit tree",
                    unit_name,
                    pid.map(|p| p.to_string())
                        .unwrap_or_else(|| "-".to_string()),
                    timeout_secs
                );

                // Phase 5 — SIGKILL: the cgroup-wide kill first (atomic,
                // covers every descendant cgroup), then the pid-based
                // backstop for kernels without the `cgroup.kill`
                // interface (or paths that vanished mid-stop).
                if let Some(p) = cgroup_path.as_deref() {
                    if crate::cgroup::kill(p) {
                        debug!(
                            "Killed the whole unit cgroup of {} via cgroup.kill",
                            unit_name
                        );
                    }
                }
                for p in cgroup_path
                    .as_deref()
                    .map(crate::cgroup::read_pids)
                    .unwrap_or_default()
                {
                    let _ = signal::kill(Pid::from_raw(p as i32), Signal::SIGKILL);
                }
                if let Some(pid) = pid {
                    match group_leader_of(pid) {
                        Some(pgid) => {
                            let _ = signal::killpg(pgid, Signal::SIGKILL);
                        }
                        None => {
                            let _ = signal::kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
                        }
                    }
                }
                break;
            }
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
    /// passwd home directory; `None` when only `Group=` was set (no user
    /// lookup happened).
    home: Option<String>,
    /// passwd login shell.
    shell: Option<String>,
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
        if gid.is_none() {
            gid = Some(usr.gid);
        }
        return Ok(Some(Creds {
            uid: uid.unwrap_or(Uid::from_raw(0)).as_raw(),
            gid: gid.unwrap_or(Gid::from_raw(0)).as_raw(),
            username: Some(usr.name.clone()),
            home: Some(usr.dir.to_string_lossy().into_owned()),
            shell: Some(usr.shell.to_string_lossy().into_owned()),
        }));
    }

    Ok(Some(Creds {
        uid: uid.unwrap_or(Uid::from_raw(0)).as_raw(),
        gid: gid.unwrap_or(Gid::from_raw(0)).as_raw(),
        username: None,
        home: None,
        shell: None,
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
/// child; `sd_listen_fds()` verifies `LISTEN_PID` against `getpid()`),
/// moves the process into `cgroup_path` (Delegate= placement; empty =
/// untouched) while it is still root, and finally switches credentials
/// (`initgroups`/`setgid`/`setuid`) when `creds` is set — mirroring
/// systemd's `apply_credentials()`.
///
/// The placement must run before the credential switch: the source
/// cgroup's `cgroup.procs` is root-owned, so a post-setuid self-move would
/// be EACCES even though the target cgroup is already chowned to the unit's
/// user.
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
    cgroup_path: String,
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
        // Delegate= placement: move this process into the unit's prepared,
        // delegated cgroup while still root — before the credential switch
        // below (writing the source cgroup's root-owned cgroup.procs as the
        // final uid would be EACCES).  An empty path means the unit does not
        // delegate (or no cgroup backend answered): the process stays in its
        // current cgroup, exactly as before delegation existed.
        if !cgroup_path.is_empty() {
            let procs = format!("{cgroup_path}/cgroup.procs");
            let c_procs = CString::new(procs.as_str()).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    sysa::l10n::t_("NUL in cgroup path"),
                )
            })?;
            let pid = unsafe { libc::getpid() }.to_string();
            unsafe {
                let fd = libc::open(c_procs.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
                if fd < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let rc = libc::write(fd, pid.as_ptr().cast(), pid.len());
                let write_err = std::io::Error::last_os_error();
                libc::close(fd);
                if rc != pid.len() as isize {
                    return Err(if rc < 0 {
                        write_err
                    } else {
                        std::io::Error::new(
                            std::io::ErrorKind::WriteZero,
                            sysa::l10n::t_("short write to cgroup.procs"),
                        )
                    });
                }
            }
        }
        if let Some(creds) = &creds {
            // An unprivileged System S (the per-user instance) cannot call
            // initgroups(2)/setgroups(2) — they need CAP_SETGID — and
            // setuid(2) to its own uid is a no-op anyway, so `User=` naming
            // the current user would fail with EPERM on a credential switch
            // that changes nothing.  Skip the whole switch exactly when it
            // is that no-op (target uid/gid == the process's own, process
            // not privileged); a root System S (the system instance) always
            // takes the full systemd-compatible path below.
            let euid = unsafe { libc::geteuid() };
            let egid = unsafe { libc::getegid() };
            let no_op = euid != 0 && creds.uid == euid && creds.gid == egid;
            if !no_op {
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
    let machine_id = || sysa::firstboot::read_machine_id().unwrap_or_default();
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
fn build_env_table(
    unit_env: &[String],
    pam_env: &[(String, String)],
    identity_env: &[(String, String)],
    directory_env: &[(String, String)],
) -> HashMap<String, String> {
    let mut table: HashMap<String, String> = HashMap::new();

    for (key, val) in std::env::vars() {
        table.insert(key, val);
    }

    for (key, val) in identity_env {
        table.insert(key.clone(), val.clone());
    }

    for entry in unit_env {
        if let Some((key, val)) = entry.split_once('=') {
            table.insert(key.to_string(), val.to_string());
        }
    }

    for (key, val) in pam_env {
        table.insert(key.clone(), val.clone());
    }

    for (key, val) in directory_env {
        table.insert(key.clone(), val.clone());
    }

    for (key, val) in FORCED_SERVICE_ENV {
        table.insert(key.to_string(), val.to_string());
    }

    table
}

/// `$USER`/`$LOGNAME`/`$HOME`/`$SHELL` for the spawned child, sourced from the
/// passwd entry of the service user — systemd.exec(5) has the manager set
/// these four for every service, from `User=`'s passwd record or root's when
/// the unit specifies no user (PAM-managed units get them from the pam
/// environment instead, which is merged after them).  `$HOME` unset is not a
/// tolerable fallback: dde-dconfig-daemon interpreted it as `//.cache/…`, and
/// spdlog abort()ed the daemon on the resulting Permission denied — taking
/// the whole accounts stack (and with it the greeter) down with it.
fn identity_env(creds: Option<&Creds>) -> Vec<(String, String)> {
    let creds = match creds {
        Some(c) => c,
        // No `User=`/`Group=` anywhere: the service runs as root, whose
        // passwd entry is /root:/bin/sh on essentially every image.
        None => {
            return vec![
                ("USER".to_string(), "root".to_string()),
                ("LOGNAME".to_string(), "root".to_string()),
                ("HOME".to_string(), "/root".to_string()),
                ("SHELL".to_string(), "/bin/sh".to_string()),
            ];
        }
    };
    let user = creds.username.clone().unwrap_or_else(|| "root".to_string());
    let home = creds.home.clone().unwrap_or_else(|| "/root".to_string());
    let shell = creds.shell.clone().unwrap_or_else(|| "/bin/sh".to_string());
    vec![
        ("USER".to_string(), user.clone()),
        ("LOGNAME".to_string(), user),
        ("HOME".to_string(), home),
        ("SHELL".to_string(), shell),
    ]
}

/// Per-family bases for the systemd.exec(5) directory directives.
///
/// The system domain uses the absolute bases; a user-domain manager remaps
/// them into the calling user's XDG hierarchy (see systemd.exec(5)'s
/// user-manager table).
#[cfg(unix)]
#[derive(Debug, Clone)]
struct DirectoryBases {
    state: std::path::PathBuf,
    logs: std::path::PathBuf,
    runtime: std::path::PathBuf,
    configuration: std::path::PathBuf,
}

#[cfg(unix)]
fn directory_bases() -> DirectoryBases {
    // The bases follow the manager's own identity (System S), not the
    // service's: the system-domain manager is root and uses the absolute
    // systemd paths, while a user-domain manager remaps its per-service
    // directories into the user's XDG hierarchy (RuntimeDirectory →
    // $XDG_RUNTIME_DIR, StateDirectory → $XDG_STATE_HOME, CacheDirectory →
    // $XDG_CACHE_HOME, ConfigurationDirectory → $XDG_CONFIG_HOME — for the
    // LogsDirectory the cache hierarchy is the deepin-observed convention).
    if nix::unistd::Uid::current().as_raw() == 0 {
        return DirectoryBases {
            state: "/var/lib".into(),
            logs: "/var/log".into(),
            runtime: "/run".into(),
            configuration: "/etc".into(),
        };
    }
    let env_or = |key: &str, fallback: String| -> std::path::PathBuf {
        std::env::var(key)
            .ok()
            .filter(|v| !v.is_empty())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(fallback))
    };
    let home = env_or("HOME", "/root".to_string());
    DirectoryBases {
        state: env_or("XDG_STATE_HOME", format!("{}/.local/state", home.display())),
        logs: env_or("XDG_CACHE_HOME", format!("{}/.cache", home.display())),
        runtime: env_or(
            "XDG_RUNTIME_DIR",
            format!("/run/user/{}", nix::unistd::Uid::current().as_raw()),
        ),
        configuration: env_or("XDG_CONFIG_HOME", format!("{}/.config", home.display())),
    }
}

/// Create one directory family's paths, apply the mode, hand ownership to
/// the service user and export the corresponding `$*_DIRECTORY` env var
/// (systemd.exec(5)).  Nested fragments get their intermediate levels with
/// the default umask permissions; mode and ownership land on the leaf.
#[cfg(unix)]
fn prepare_directory_family(
    env_key: &str,
    fragments: &[String],
    mode: u32,
    base: &std::path::Path,
    owner: Option<(nix::unistd::Uid, nix::unistd::Gid)>,
) -> Result<Vec<(String, String)>> {
    use std::os::unix::fs::PermissionsExt;

    let mut paths: Vec<String> = Vec::new();
    for fragment in fragments {
        let path = base.join(fragment.trim_start_matches('/'));
        std::fs::create_dir_all(&path)
            .with_context(|| format!("creating {} directory {}", env_key, path.display()))?;
        if mode != 0 {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
                .with_context(|| format!("setting mode on {}", path.display()))?;
        }
        if let Some((uid, gid)) = owner {
            nix::unistd::chown(&path, Some(uid), Some(gid))
                .map_err(|e| anyhow::anyhow!("chown {} to service user: {e}", path.display()))?;
        }
        paths.push(path.to_string_lossy().into_owned());
    }
    Ok(if paths.is_empty() {
        Vec::new()
    } else {
        vec![(env_key.to_string(), paths.join(":"))]
    })
}

/// Run every directory family for one service.  Returns the env pairs to
/// hand to the process (empty when the unit declares none), failing the
/// start when a directory cannot be created — systemd likewise fails the
/// unit (e.g. `Failed to create /var/lib/…`).
#[cfg(unix)]
fn prepare_service_directories(
    svc: &sysa::proto::ServiceConfig,
    creds: Option<&Creds>,
) -> Result<Vec<(String, String)>> {
    let bases = directory_bases();
    // Only the root manager can (and systemd-ish does) enforce ownership;
    // a user-domain manager is already running as the owning user.
    let owner = if nix::unistd::Uid::current().as_raw() == 0 {
        Some((
            nix::unistd::Uid::from_raw(creds.map(|c| c.uid).unwrap_or(0)),
            nix::unistd::Gid::from_raw(creds.map(|c| c.gid).unwrap_or(0)),
        ))
    } else {
        None
    };
    let mut env = Vec::new();
    for (key, fragments, mode, base) in [
        (
            "STATE_DIRECTORY",
            &svc.state_directory,
            svc.state_directory_mode,
            bases.state.as_path(),
        ),
        (
            "LOGS_DIRECTORY",
            &svc.logs_directory,
            svc.logs_directory_mode,
            bases.logs.as_path(),
        ),
        (
            "RUNTIME_DIRECTORY",
            &svc.runtime_directory,
            svc.runtime_directory_mode,
            bases.runtime.as_path(),
        ),
        (
            "CONFIGURATION_DIRECTORY",
            &svc.configuration_directory,
            svc.configuration_directory_mode,
            bases.configuration.as_path(),
        ),
    ] {
        env.extend(prepare_directory_family(key, fragments, mode, base, owner)?);
    }
    Ok(env)
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
    fn forced_env_applies_only_to_root_services() {
        assert!(forced_bus_env_applies(""));
        assert!(forced_bus_env_applies("root"));
        assert!(forced_bus_env_applies("0"));
        assert!(!forced_bus_env_applies("user"));
        assert!(!forced_bus_env_applies("1000"));
    }

    #[test]
    fn forced_env_overrides_unit_environment_in_table() {
        let unit_env = vec![
            "SYSTEMCTL_FORCE_BUS=0".to_string(),
            "SYSTEMD_OFFLINE=1".to_string(),
        ];
        let table = build_env_table(&unit_env, &[], &[], &[]);
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
        let table = build_env_table(&unit_env, &pam_env, &[], &[]);
        assert_eq!(
            table.get("XDG_RUNTIME_DIR").map(String::as_str),
            Some("/run/user/1000")
        );
    }

    #[test]
    fn directory_env_overrides_unit_environment_in_table() {
        // The manager synthesizes the directory variables from the unit's
        // directory directives; a unit that (pointlessly) carries its own
        // STATE_DIRECTORY= must not mask the real one.
        let directory_env = [("STATE_DIRECTORY".to_string(), "/var/lib/foo".to_string())];
        let unit_env = vec!["STATE_DIRECTORY=/opt/lie".to_string()];
        let table = build_env_table(&unit_env, &[], &[], &directory_env);
        assert_eq!(
            table.get("STATE_DIRECTORY").map(String::as_str),
            Some("/var/lib/foo")
        );
    }

    #[test]
    fn forced_env_overrides_pam_environment_in_table() {
        let pam_env = vec![
            ("SYSTEMCTL_FORCE_BUS".to_string(), "0".to_string()),
            ("SYSTEMD_OFFLINE".to_string(), "1".to_string()),
        ];
        let table = build_env_table(&[], &pam_env, &[], &[]);
        assert_eq!(
            table.get("SYSTEMCTL_FORCE_BUS").map(String::as_str),
            Some("1")
        );
        assert_eq!(table.get("SYSTEMD_OFFLINE").map(String::as_str), Some("0"));
    }

    #[test]
    fn identity_env_defaults_to_root_passwd_entry_for_userless_units() {
        // systemd.exec(5): with no User=/Group= the four identity variables
        // still exist, sourced from root's passwd record.  An empty $HOME is
        // what made dde-dconfig-daemon resolve its cache file as
        // //.cache/deepin/dde-dconfig-daemon/dde-dconfig-daemon.log.
        let table = build_env_table(&[], &[], &identity_env(None), &[]);
        assert_eq!(table.get("USER").map(String::as_str), Some("root"));
        assert_eq!(table.get("LOGNAME").map(String::as_str), Some("root"));
        assert_eq!(table.get("HOME").map(String::as_str), Some("/root"));
        assert_eq!(table.get("SHELL").map(String::as_str), Some("/bin/sh"));
    }

    #[test]
    fn identity_env_sources_home_and_shell_from_service_user() {
        let creds = Creds {
            uid: 1000,
            gid: 1000,
            username: Some("chenpi11".to_string()),
            home: Some("/home/chenpi11".to_string()),
            shell: Some("/usr/bin/zsh".to_string()),
        };
        let identity = identity_env(Some(&creds));
        let table = build_env_table(&[], &[], &identity, &[]);
        assert_eq!(table.get("USER").map(String::as_str), Some("chenpi11"));
        assert_eq!(table.get("LOGNAME").map(String::as_str), Some("chenpi11"));
        assert_eq!(
            table.get("HOME").map(String::as_str),
            Some("/home/chenpi11")
        );
        assert_eq!(table.get("SHELL").map(String::as_str), Some("/usr/bin/zsh"));
    }

    #[test]
    fn unit_environment_overrides_identity_env_in_table() {
        let creds = Creds {
            uid: 1000,
            gid: 1000,
            username: Some("chenpi11".to_string()),
            home: Some("/home/chenpi11".to_string()),
            shell: Some("/usr/bin/zsh".to_string()),
        };
        let identity = identity_env(Some(&creds));
        let unit_env = vec!["HOME=/opt/unit-home".to_string()];
        let table = build_env_table(&unit_env, &[], &identity, &[]);
        assert_eq!(
            table.get("HOME").map(String::as_str),
            Some("/opt/unit-home")
        );
    }

    #[test]
    #[cfg(unix)]
    fn directory_family_creates_env_colon_joined() {
        // systemd.exec(5): each fragment below the family base is created
        // (nested levels included), gets the directive mode on its leaf and
        // the family env var joins the full paths colon-separated.
        use std::os::unix::fs::PermissionsExt;

        let base = std::env::temp_dir().join(format!(
            "systema-syss-dirs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();

        let env = prepare_directory_family(
            "STATE_DIRECTORY",
            &["foo".to_string(), "sub/bar".to_string()],
            0o700,
            &base,
            None,
        )
        .unwrap();

        assert_eq!(
            env,
            vec![(
                "STATE_DIRECTORY".to_string(),
                format!(
                    "{}:{}",
                    base.join("foo").display(),
                    base.join("sub/bar").display()
                )
            )]
        );
        // The mode lands on each leaf (the nested intermediate level keeps
        // umask defaults, like systemd's intermediate handling).
        let mode = std::fs::metadata(base.join("foo"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
        let mode = std::fs::metadata(base.join("sub/bar"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn directory_family_without_mode_gets_default_and_env() {
        // mode 0 = unset: the directory is still created and exported; the
        // open perms follow the umask (not asserted here — umask-dependent).
        let base = std::env::temp_dir().join(format!(
            "systema-syss-dirs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();

        let env =
            prepare_directory_family("LOGS_DIRECTORY", &["deepin".to_string()], 0, &base, None)
                .unwrap();
        assert_eq!(
            env,
            vec![(
                "LOGS_DIRECTORY".to_string(),
                base.join("deepin").to_string_lossy().into_owned()
            )]
        );
        assert!(base.join("deepin").is_dir());

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn directory_family_failure_fails_the_start() {
        // A base that is a file (not a directory) must fail creation —
        // systemd rejects the unit for a failing directory directive too.
        let root = std::env::temp_dir().join(format!(
            "systema-syss-dirs-file-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos()
        ));
        std::fs::write(&root, b"not a dir").unwrap();

        let result =
            prepare_directory_family("STATE_DIRECTORY", &["foo".to_string()], 0, &root, None);
        assert!(result.is_err());

        std::fs::remove_file(&root).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn prepare_service_directories_skips_empty_families() {
        // No directory directives listed → no env produced and nothing
        // created anywhere.
        let svc = sysa::proto::ServiceConfig::default();
        let env = prepare_service_directories(&svc, None).unwrap_or_else(|e| {
            panic!("empty family must not fail: {e}");
        });
        assert!(env.is_empty());
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

// ---------------------------------------------------------------------------
// Tests: which processes a stop reaches
// ---------------------------------------------------------------------------

/// The stop path's cgroup preference is only observable on real processes
/// (the question is *which* pids receive a signal), so these tests fork
/// `sleep` children of the test process and fake the cgroup interface files
/// in a temporary directory: no root, no cgroupfs, no System R.
#[cfg(all(test, unix))]
mod stop_tests {
    use super::*;
    use crate::cgroup::Querier;
    use crate::state::{new_registry, ServiceRegistry, ServiceState};
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;
    use prost::Message;
    use std::collections::HashMap;
    use std::os::unix::process::CommandExt;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use sysa::proto::{Envelope, UnitCgroupResult};
    use sysa::worker_ipc::EventPublisher;

    /// Fake cgroup directory for one test (tests run concurrently, so the
    /// tag plus the pid keeps them apart).
    fn fake_cgroup_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("syss-stop-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Spawn a long-running `sleep` child of the test process in a process
    /// group of its own.  With `own_session` it also gets a session of its
    /// own (`setsid`), i.e. it is no longer reachable through the main
    /// process's group — the escaped child a cgroup-wide stop must catch.
    fn spawn_sleeper(own_session: bool) -> u32 {
        let mut cmd = std::process::Command::new("sleep");
        cmd.arg("60").stdout(std::process::Stdio::null());
        if own_session {
            // SAFETY: `setsid` is async-signal-safe and cannot fail for a
            // child that is not yet a process group leader.
            unsafe {
                cmd.pre_exec(|| {
                    nix::libc::setsid();
                    Ok(())
                });
            }
        } else {
            cmd.process_group(0);
        }
        cmd.spawn().expect("failed to spawn the test sleeper").id()
    }

    /// A detached sleeper that ignores SIGTERM (`trap ''` survives `exec`),
    /// so only the SIGKILL half of the teardown can collect it.
    fn spawn_sigterm_ignorer() -> u32 {
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.arg("-c")
            .arg("trap '' TERM; exec sleep 60")
            .stdout(std::process::Stdio::null());
        // SAFETY: see `spawn_sleeper`.
        unsafe {
            cmd.pre_exec(|| {
                nix::libc::setsid();
                Ok(())
            });
        }
        cmd.spawn()
            .expect("failed to spawn the SIGTERM-ignoring child")
            .id()
    }

    /// Reap the test's own children as they exit — the job
    /// `monitor_service` and the orphan reaper do in production.  Without
    /// it a dead child stays a zombie, `kill(pid, 0)` keeps answering
    /// success, and the stop would sit out its whole timeout.
    fn spawn_reaper(pids: Vec<u32>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut left = pids;
            for _ in 0..600 {
                left.retain(|pid| {
                    let mut status = 0;
                    // SAFETY: `pid` is our own child; WNOHANG never blocks.
                    unsafe {
                        nix::libc::waitpid(*pid as i32, &mut status, nix::libc::WNOHANG);
                    }
                    is_alive(*pid)
                });
                if left.is_empty() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
    }

    /// Wait for `pid` to be gone (the reaper above collects the zombie).
    async fn assert_gone(pid: u32, what: &str) {
        for _ in 0..300 {
            if !is_alive(pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("{what} (PID {pid}) should have been terminated");
    }

    /// Leave nothing running behind a test that expects a survivor.
    async fn kill_now(pid: u32) {
        let _ = kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
        assert_gone(pid, "the cleanup target").await;
    }

    /// A registry holding one running service with `pid` as its main pid.
    fn running_registry(unit: &str, pid: u32) -> ServiceRegistry {
        let registry = new_registry();
        {
            let mut reg = registry.lock();
            let inst = reg.entry(unit.to_string()).or_default();
            inst.state = ServiceState::Running;
            inst.main_pid = Some(pid);
        }
        registry
    }

    /// A querier wired to a fake System A that answers every `unit.cgroup`
    /// question with `result`, echoing the request_id the way the relay
    /// does.
    fn answering_querier(result: UnitCgroupResult) -> Querier {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<bytes::Bytes>();
        let pending: Arc<Mutex<HashMap<u64, tokio::sync::oneshot::Sender<UnitCgroupResult>>>> =
            Arc::default();
        let publisher = EventPublisher::new(tx, "system-s-1", Arc::default(), pending.clone());
        tokio::spawn(async move {
            while let Some(bytes) = rx.recv().await {
                let Ok(env) = Envelope::decode(bytes.as_ref()) else {
                    continue;
                };
                assert_eq!(env.method, "unit.cgroup");
                if let Some(tx) = pending.lock().unwrap().remove(&env.request_id) {
                    let _ = tx.send(result.clone());
                }
            }
        });
        Querier::new(publisher, Arc::new(AtomicBool::new(false)))
    }

    /// A querier whose System A accepts the question and never answers: the
    /// returned receiver keeps the channel open so the lookup really runs
    /// into its budget instead of failing on a closed pipe.
    fn silent_querier() -> (Querier, tokio::sync::mpsc::UnboundedReceiver<bytes::Bytes>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<bytes::Bytes>();
        let querier = Querier::new(
            EventPublisher::new(tx, "system-s-1", Arc::default(), Arc::default()),
            Arc::new(AtomicBool::new(false)),
        );
        (querier, rx)
    }

    /// A known cgroup path makes the stop reach the whole subtree: a child
    /// that escaped the main process group is signalled too, and the path is
    /// remembered for the next stop.
    #[tokio::test]
    async fn stop_signals_the_whole_unit_cgroup() {
        let dir = fake_cgroup_dir("tree");
        // The fake cgroup reports itself empty, so the stop returns as soon
        // as the main process is gone instead of polling out its timeout;
        // the escapee still has to receive the SIGTERM fanout.
        std::fs::write(dir.join("cgroup.events"), "populated 0\n").unwrap();
        let main = spawn_sleeper(false);
        let escapee = spawn_sleeper(true);
        std::fs::write(dir.join("cgroup.procs"), format!("{main}\n{escapee}\n")).unwrap();
        let reaper = spawn_reaper(vec![main, escapee]);

        let registry = running_registry("app.service", main);
        let querier = answering_querier(UnitCgroupResult {
            available: true,
            found: true,
            cgroup_path: dir.to_string_lossy().into_owned(),
            error: String::new(),
        });

        stop_service(registry.clone(), "app.service", 5, Some(&querier))
            .await
            .expect("a stop with a known cgroup must succeed");

        assert_gone(main, "the main process").await;
        assert_gone(escapee, "the child outside the main process group").await;
        {
            let reg = registry.lock();
            let inst = reg.get("app.service").unwrap();
            // A stop that saw the tree die marks the unit dead itself.
            assert_eq!(inst.state, ServiceState::Dead);
            assert_eq!(inst.main_pid, None);
            assert_eq!(
                inst.cgroup_path.as_deref(),
                Some(dir.to_str().unwrap()),
                "the path must be remembered for later stops"
            );
        }

        reaper.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The SIGTERM round can be ignored: after the stop timeout the cgroup
    /// (and with it a member that ignores SIGTERM) is SIGKILLed, and the
    /// kernel's `cgroup.kill` interface is tried first.
    #[tokio::test]
    async fn stop_sigkills_a_cgroup_member_that_ignores_sigterm() {
        let dir = fake_cgroup_dir("kill");
        // No `cgroup.events`: the emptiness probe falls back to polling
        // `cgroup.procs`, which still lists the stubborn member — exactly
        // the situation the SIGKILL round exists for.
        let main = spawn_sleeper(false);
        let stubborn = spawn_sigterm_ignorer();
        std::fs::write(dir.join("cgroup.procs"), format!("{main}\n{stubborn}\n")).unwrap();
        let reaper = spawn_reaper(vec![main, stubborn]);

        let registry = running_registry("app.service", main);
        let querier = answering_querier(UnitCgroupResult {
            available: true,
            found: true,
            cgroup_path: dir.to_string_lossy().into_owned(),
            error: String::new(),
        });

        stop_service(registry.clone(), "app.service", 1, Some(&querier))
            .await
            .expect("the stop must still complete after the SIGKILL round");

        assert_gone(main, "the main process").await;
        assert_gone(stubborn, "the SIGTERM-ignoring cgroup member").await;
        assert!(
            dir.join("cgroup.kill").exists(),
            "the cgroup-wide kill interface must be tried first"
        );

        reaper.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// No cgroup backend on the machine (`available = false`): the stop
    /// falls back to the main process group, so the escaped child survives —
    /// the documented degradation, never a delay.
    #[tokio::test]
    async fn stop_without_a_cgroup_backend_kills_only_the_process_group() {
        let main = spawn_sleeper(false);
        let escapee = spawn_sleeper(true);
        let reaper = spawn_reaper(vec![main, escapee]);
        let registry = running_registry("app.service", main);
        let querier = answering_querier(UnitCgroupResult {
            available: false,
            found: false,
            cgroup_path: String::new(),
            error: "no cgroup backend".to_string(),
        });

        stop_service(registry.clone(), "app.service", 5, Some(&querier))
            .await
            .expect("the stop must succeed on the fallback path");

        assert_gone(main, "the main process").await;
        assert!(
            is_alive(escapee),
            "without a cgroup the escaped child is out of reach"
        );
        assert!(
            registry
                .lock()
                .get("app.service")
                .unwrap()
                .cgroup_path
                .is_none(),
            "an unavailable verdict must not be remembered as a path"
        );

        kill_now(escapee).await;
        reaper.abort();
    }

    /// A System A that never answers must not stretch the stop: the lookup
    /// budget expires and the process-group path runs.
    #[tokio::test]
    async fn stop_degrades_when_the_lookup_never_answers() {
        let main = spawn_sleeper(false);
        let escapee = spawn_sleeper(true);
        let reaper = spawn_reaper(vec![main, escapee]);
        let registry = running_registry("app.service", main);
        let (querier, _rx) = silent_querier();

        let started = std::time::Instant::now();
        stop_service(registry.clone(), "app.service", 5, Some(&querier))
            .await
            .expect("a silent lookup must not fail the stop");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the lookup budget, not the stop timeout, must bound the query"
        );

        assert_gone(main, "the main process").await;
        assert!(is_alive(escapee), "the escaped child survives the fallback");

        kill_now(escapee).await;
        reaper.abort();
    }
}
