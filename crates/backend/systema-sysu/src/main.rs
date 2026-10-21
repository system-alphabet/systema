//! systema-sysu — System U, the per-user instance launcher.
//!
//! The user-session counterpart of `systema-sysi`: after login it starts a
//! per-user service-manager instance (System A + the Service and Target
//! workers + System W's session-bus D-Bus bridge, so the session's
//! `systemctl --user` controls it) on sockets under
//! `${XDG_RUNTIME_DIR}/systema`, waits for each stage on the notify
//! channel, runs the control phase (daemon-reload, then start enabled
//! units + `default.target`), and supervises the group until `--logout`
//! (SIGTERM) or a child's death stops it.
//!
//! Lifecycle hooks (plan §3.4, `doc/user-session.md`):
//!
//! ```text
//! # ~/.profile / ~/.bash_profile
//! systema-sysu --login          # idempotent background start
//! trap 'systema-sysu --logout' EXIT   # best-effort stop
//! ```
//!
//! The instance installs its own environment matrix ([`envs`]) before
//! resolving any path, so it never touches the system domain's
//! `/run/systema` and never scans system unit directories.  Everything
//! else — binaries, locale, shell, machine-id — is shared read-only state
//! with the system instance.

mod envs;
mod supervise;
mod workers;

use anyhow::{bail, Context, Result};
use clap::Parser;
use std::path::PathBuf;
use std::time::Duration;
use tracing::{error, info};

#[derive(Parser)]
#[command(name = "systema-sysu", about = "System U — per-user instance launcher")]
struct Args {
    #[arg(long, short = 'D', help = "Enable debug-level logging")]
    debug: bool,

    #[arg(
        long,
        default_value = "warn",
        help = "Log level (trace, debug, info, warn, error)"
    )]
    log_level: String,

    #[arg(
        long,
        help = "Search for the systema-* binaries only in this directory (default: the executable directory, then PATH)"
    )]
    bin_dir: Option<PathBuf>,

    #[arg(
        long,
        help = "Do not abort when an executable is missing; log an ERROR and skip it"
    )]
    no_strict: bool,

    #[arg(
        long,
        default_value_t = 10,
        help = "Grace period in seconds before SIGKILL during shutdown"
    )]
    shutdown_timeout: u64,

    #[arg(
        long,
        default_value_t = 30,
        help = "Time to wait for System A / a worker to report ready before aborting"
    )]
    ready_timeout: u64,

    #[arg(
        long,
        help = "Path of this process's log file; child processes receive <dir>/<binary>.log (\"-\" writes to stderr only)"
    )]
    log_file: Option<PathBuf>,

    #[arg(
        long,
        help = "Start the per-user instance in the background if it is not running (idempotent; for login hooks)"
    )]
    login: bool,

    #[arg(
        long,
        help = "Stop the running per-user instance and wait for it (for logout hooks)"
    )]
    logout: bool,

    #[arg(
        long,
        hide = true,
        help = "Internal: run detached (systema-sysu --login re-executes itself with this flag)"
    )]
    daemon: bool,

    #[arg(long, short = 'v', help = "Print version information and exit")]
    version: bool,

    #[arg(
        long,
        help = "Print full version: build options and compile-time paths"
    )]
    full_version: bool,
}

/// Build the localized CLI (same pattern as the other System Alphabet
/// binaries: `mut_arg` + `t_()` for every user-visible string).
fn build_args() -> Args {
    use clap::{CommandFactory, FromArgMatches};
    let cmd = Args::command()
        .about(sysa::l10n::t_("System U — per-user instance launcher"))
        .mut_arg("debug", |a| {
            a.help(sysa::l10n::t_("Enable debug-level logging."))
        })
        .mut_arg("log_level", |a| {
            a.help(sysa::l10n::t_(
                "Log level (trace, debug, info, warn, error).",
            ))
        })
        .mut_arg("bin_dir", |a| {
            a.help(sysa::l10n::t_(
                "Search for the systema-* binaries only in this directory.",
            ))
        })
        .mut_arg("no_strict", |a| {
            a.help(sysa::l10n::t_(
                "Do not abort when an executable is missing; log an ERROR and skip it.",
            ))
        })
        .mut_arg("shutdown_timeout", |a| {
            a.help(sysa::l10n::t_(
                "Grace period in seconds before SIGKILL during shutdown.",
            ))
        })
        .mut_arg("ready_timeout", |a| {
            a.help(sysa::l10n::t_(
                "Time to wait for System A / a worker to report ready before aborting.",
            ))
        })
        .mut_arg("log_file", |a| {
            a.help(sysa::l10n::t_(
                "Path of this process's log file; child processes receive <dir>/<binary>.log; \"-\" writes to stderr only.",
            ))
        })
        .mut_arg("login", |a| {
            a.help(sysa::l10n::t_(
                "Start the per-user instance in the background if it is not running (idempotent; for login hooks).",
            ))
        })
        .mut_arg("logout", |a| {
            a.help(sysa::l10n::t_(
                "Stop the running per-user instance and wait for it (for logout hooks).",
            ))
        })
        .mut_arg("version", |a| {
            a.help(sysa::l10n::t_("Print version information and exit."))
        })
        .mut_arg("full_version", |a| {
            a.help(sysa::l10n::t_(
                "Print full version: build options and compile-time paths.",
            ))
        });
    Args::from_arg_matches(&cmd.get_matches()).unwrap_or_else(|e| e.exit())
}

// ---------------------------------------------------------------------------
// Instance identity: pidfile + control-socket probe
// ---------------------------------------------------------------------------

/// `<user-runtime-dir>/systema-sysu.pid` — how `--login` recognises an
/// already-running instance and how `--logout` finds the process to stop.
///
/// Deliberately computed from [`sysa::paths::user_runtime_dir`] rather
/// than the resolved [`sysa::paths`] instance: the pidfile's location *is*
/// the user instance's location by definition, whatever this shell
/// exported.
fn pidfile_path() -> PathBuf {
    PathBuf::from(sysa::paths::user_runtime_dir()).join("systema-sysu.pid")
}

/// The pid recorded in the pidfile (unverified).
fn read_pid() -> Option<i32> {
    let text = std::fs::read_to_string(pidfile_path()).ok()?;
    let pid: i32 = text.trim().parse().ok()?;
    (pid > 0).then_some(pid)
}

/// Signal 0 probes existence (and our permission to signal) without
/// delivering anything.
fn pid_alive(pid: i32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
}

/// The pidfile's pid, but only while that process is still alive — a
/// crashed instance leaves a stale file behind, which must not be mistaken
/// for a running one.
fn running_pid() -> Option<i32> {
    read_pid().filter(|pid| pid_alive(*pid))
}

/// Is anything listening on this instance's control socket?  The backup
/// probe for a running instance whose pidfile was lost (plan §3.3: sysa
/// itself exits when the socket is already served).
fn control_socket_live() -> bool {
    std::os::unix::net::UnixStream::connect(sysa::paths::instance().control_socket_path).is_ok()
}

/// Write the instance's pidfile.  Called once supervision is about to
/// start, which is what lets `--login`'s wait distinguish "coming up"
/// from "failed during startup".
fn write_pidfile() -> Result<()> {
    let path = pidfile_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| {
            sysa::l10n::fmt(
                sysa::l10n::t_("Cannot create runtime directory {dir}."),
                &[("dir", &dir.display().to_string())],
            )
        })?;
    }
    std::fs::write(&path, format!("{}\n", std::process::id())).with_context(|| {
        sysa::l10n::fmt(
            sysa::l10n::t_("Cannot write pidfile {path}."),
            &[("path", &path.display().to_string())],
        )
    })?;
    Ok(())
}

/// Drop a pidfile that still names this process.
fn remove_pidfile_if_ours() {
    if read_pid() == Some(std::process::id() as i32) {
        let _ = std::fs::remove_file(pidfile_path());
    }
}

/// Drop a pidfile unconditionally (used by `--logout` for a stale file).
fn remove_pidfile() {
    let _ = std::fs::remove_file(pidfile_path());
}

/// Create the instance's runtime directory (plan §3.2, step 1).
///
/// Mode 0700 regardless of umask: the whole socket set takes its "only I
/// and root" isolation from this directory — including the
/// `/tmp/systema-<uid>` fallback, whose parent does not provide it.
/// (`$XDG_RUNTIME_DIR/systema` is a subdirectory, so the chmod can never
/// reach the session dir itself.)
fn ensure_runtime_dir() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = sysa::paths::user_runtime_dir();
    std::fs::create_dir_all(&dir).with_context(|| {
        sysa::l10n::fmt(
            sysa::l10n::t_("Cannot create runtime directory {dir}."),
            &[("dir", &dir)],
        )
    })?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).with_context(|| {
        sysa::l10n::fmt(
            sysa::l10n::t_("Cannot make runtime directory private (0700) {dir}."),
            &[("dir", &dir)],
        )
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Login / logout hooks
// ---------------------------------------------------------------------------

/// Arguments for the detached `--daemon` child that `--login` re-executes.
fn detached_child_args(args: &Args) -> Vec<String> {
    let mut out = vec!["--daemon".to_string()];
    if args.debug {
        out.push("--debug".to_string());
    } else {
        out.push("--log-level".to_string());
        out.push(args.log_level.clone());
    }
    if let Some(dir) = &args.bin_dir {
        out.push("--bin-dir".to_string());
        out.push(dir.to_string_lossy().into_owned());
    }
    if args.no_strict {
        out.push("--no-strict".to_string());
    }
    out.push("--shutdown-timeout".to_string());
    out.push(args.shutdown_timeout.to_string());
    out.push("--ready-timeout".to_string());
    out.push(args.ready_timeout.to_string());
    if let Some(file) = &args.log_file {
        out.push("--log-file".to_string());
        out.push(file.to_string_lossy().into_owned());
    }
    out
}

/// `--login`: idempotent background start for a login hook (plan §3.4).
///
/// Re-executes this binary with `--daemon` (the child does the `setsid`),
/// then waits for the child's pidfile — written just before supervision
/// starts — so an immediate failure is reported here instead of being
/// swallowed by the detached process.
fn login(args: &Args) -> Result<i32> {
    if let Some(pid) = running_pid() {
        println!(
            "{}",
            sysa::l10n::fmt(
                sysa::l10n::t_("Per-user instance already running (pid {pid})."),
                &[("pid", &pid.to_string())]
            )
        );
        return Ok(0);
    }
    if control_socket_live() {
        // The pidfile is gone (crashed launcher, foreign start) but System
        // A answers: still an instance — do not start a second one.
        println!("{}", sysa::l10n::t_("Per-user instance already running."));
        return Ok(0);
    }

    let exe = std::env::current_exe().context("current_exe")?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.args(detached_child_args(args));

    // stdio: inherited from this terminal.  The child logs to its own two
    // sinks (stderr + log file) from `logging::init_daemon` on; there is
    // nothing to capture by redirecting here.

    let mut child = cmd.spawn().with_context(|| {
        sysa::l10n::fmt(
            sysa::l10n::t_("Cannot spawn {program}."),
            &[("program", &exe.display().to_string())],
        )
    })?;

    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(pid) = running_pid() {
            println!(
                "{}",
                sysa::l10n::fmt(
                    sysa::l10n::t_("Per-user instance started (pid {pid})."),
                    &[("pid", &pid.to_string())]
                )
            );
            return Ok(0);
        }
        if let Some(status) = child.try_wait().context("try_wait")? {
            // The child exits successfully when *it* found an instance
            // already up (a start raced between our probe and its own).
            if status.success() && control_socket_live() {
                println!("{}", sysa::l10n::t_("Per-user instance already running."));
                return Ok(0);
            }
            eprintln!(
                "{}",
                sysa::l10n::fmt(
                    sysa::l10n::t_("systema-sysu failed to start: {status}"),
                    &[("status", &status.to_string())]
                )
            );
            return Ok(1);
        }
        if std::time::Instant::now() >= deadline {
            // Started, still initialising: not an error.
            println!(
                "{}",
                sysa::l10n::fmt(
                    sysa::l10n::t_("Per-user instance started (pid {pid}); still initialising."),
                    &[("pid", &child.id().to_string())]
                )
            );
            return Ok(0);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// `--logout`: SIGTERM the running instance and wait for the whole group
/// to go away (plan §3.4).  Idempotent — "not running" is success, so the
/// shell's `trap ... EXIT` never reports an error for an instance that was
/// never started.
fn logout(args: &Args) -> i32 {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;

    let Some(pid) = running_pid() else {
        remove_pidfile(); // stale file from a crash
        println!("{}", sysa::l10n::t_("No per-user instance is running."));
        return 0;
    };
    if kill(Pid::from_raw(pid), Signal::SIGTERM).is_err() {
        // Raced with its exit.
        remove_pidfile();
        println!("{}", sysa::l10n::t_("No per-user instance is running."));
        return 0;
    }
    println!(
        "{}",
        sysa::l10n::fmt(
            sysa::l10n::t_("Stopping per-user instance (pid {pid})..."),
            &[("pid", &pid.to_string())]
        )
    );

    // The instance's own shutdown budget is `--shutdown-timeout`
    // (SIGTERM → grace → SIGKILL); add a margin for teardown + reap.
    let deadline = std::time::Instant::now() + Duration::from_secs(args.shutdown_timeout + 15);
    while std::time::Instant::now() < deadline {
        if !pid_alive(pid) {
            remove_pidfile();
            println!("{}", sysa::l10n::t_("Per-user instance stopped."));
            return 0;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    eprintln!(
        "{}",
        sysa::l10n::fmt(
            sysa::l10n::t_("Per-user instance (pid {pid}) did not stop within {secs}s."),
            &[
                ("pid", &pid.to_string()),
                ("secs", &(args.shutdown_timeout + 15).to_string()),
            ]
        )
    );
    1
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    // The per-user matrix governs this process *and* every worker it
    // spawns: install it before paths::init() so every path this run
    // resolves points into the user instance's own runtime dir, whatever
    // the surrounding shell exported (plan §3.1).
    envs::install();
    sysa::paths::init();
    sysa::l10n::init();
    sysa::signals::install_signal_guard();

    let args = build_args();
    if args.version {
        sysa::version::print_version();
        return Ok(());
    }
    if args.full_version {
        sysa::version::print_full_version();
        return Ok(());
    }

    // `--logout` / `--login` are one-shot verbs: they never supervise.
    if args.logout {
        std::process::exit(logout(&args));
    }
    if args.login {
        std::process::exit(login(&args)?);
    }

    if args.daemon {
        // Detached child (`--login` re-executes itself with this flag):
        // leave the launching session's controlling terminal behind so the
        // instance outlives it — stopping it is `--logout`'s job.  A fresh
        // child is never a process-group leader, so setsid cannot fail
        // for that reason.
        if unsafe { libc::setsid() } < 0 {
            eprintln!(
                "{}",
                sysa::l10n::fmt(
                    sysa::l10n::t_("setsid failed: {e}."),
                    &[("e", &std::io::Error::last_os_error().to_string())]
                )
            );
        }
    }

    let log_level = if args.debug { "debug" } else { &args.log_level };
    // The launcher's own log: stderr always, plus the file resolved from
    // --log-file / SYSTEMA_LOG_FILE (the env matrix points it into the XDG
    // log directory) / the built-in default.  Every worker it spawns
    // receives `--log-file <dir>/<binary>.log` derived from this target.
    let log_target = sysa::logging::init_daemon("systema-sysu", args.log_file.clone(), log_level);
    info!("systema-sysu starting (PID {})", std::process::id());

    // Single instance, idempotent (plan §3.3): an already-running
    // instance is a success, not an error — the login hook may fire
    // repeatedly.
    if let Some(pid) = running_pid() {
        let msg = sysa::l10n::fmt(
            sysa::l10n::t_("Per-user instance already running (pid {pid})."),
            &[("pid", &pid.to_string())],
        );
        eprintln!("{msg}");
        info!("{msg}");
        return Ok(());
    }
    if control_socket_live() {
        let msg = sysa::l10n::t_("Per-user instance already running.");
        eprintln!("{msg}");
        info!("{msg}");
        return Ok(());
    }

    // Runtime dir first (0700), so the notify listener and every socket
    // land in a directory only this user can traverse (plan §3.2, step 1).
    ensure_runtime_dir()?;

    // Resolve the worker subset (sysa + syss + syst + sysw); strict by
    // default, same contract as systema-sysi.
    let set = workers::default_workers();
    let (resolved, missing) = workers::resolve_set(&set, args.bin_dir.as_deref());
    for spec in &missing {
        error!(
            "Missing executable for '{name}' ({binary}): not found in --bin-dir, the systema-sysu executable directory, or PATH",
            name = spec.name,
            binary = spec.binary
        );
    }
    if !missing.is_empty() {
        if args.no_strict {
            error!(
                "Continuing without {count} missing executable(s) (--no-strict)",
                count = missing.len()
            );
        } else {
            bail!(sysa::l10n::fmt(sysa::l10n::t_("{count} required executable(s) missing; aborting (use --no-strict to continue)"), &[("count", &(missing.len()).to_string())]));
        }
    }

    write_pidfile()?;
    let result = supervise::run(
        &resolved,
        args.debug,
        &args.log_level,
        Duration::from_secs(args.shutdown_timeout),
        Duration::from_secs(args.ready_timeout),
        &log_target,
    )
    .await;
    remove_pidfile_if_ours();
    let code = result?;
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn detached_child_passes_the_supervision_flags() {
        let args = Args::parse_from([
            "systema-sysu",
            "--log-file",
            "-",
            "--debug",
            "--bin-dir",
            "/opt/bin",
            "--no-strict",
            "--shutdown-timeout",
            "7",
            "--ready-timeout",
            "9",
        ]);
        assert_eq!(
            detached_child_args(&args),
            vec![
                "--daemon",
                "--debug",
                "--bin-dir",
                "/opt/bin",
                "--no-strict",
                "--shutdown-timeout",
                "7",
                "--ready-timeout",
                "9",
                "--log-file",
                "-",
            ]
        );
    }

    #[test]
    fn detached_child_passes_the_log_level_when_not_debug() {
        // Without an explicit --log-file there is nothing to forward: the
        // daemon child resolves SYSTEMA_LOG_FILE (the env matrix) itself.
        let args = Args::parse_from(["systema-sysu", "--log-level", "trace"]);
        assert_eq!(
            detached_child_args(&args),
            vec![
                "--daemon",
                "--log-level",
                "trace",
                "--shutdown-timeout",
                "10",
                "--ready-timeout",
                "30",
            ]
        );
    }

    #[test]
    fn pidfile_lives_in_the_user_runtime_dir() {
        // Shared lock: sibling tests mutate XDG_RUNTIME_DIR, which both
        // calls below read.
        let _guard = envs::env_lock().lock().unwrap();
        let runtime = sysa::paths::user_runtime_dir();
        let path = pidfile_path();
        assert_eq!(path.parent(), Some(Path::new(&runtime)));
        assert_eq!(path.file_name().unwrap(), "systema-sysu.pid");
    }
}
