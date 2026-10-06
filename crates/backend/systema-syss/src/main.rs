//! system-s — System Service Worker
//!
//! The service execution worker for System Alphabet. Responsibilities:
//! - Connect to System A's IPC socket and register as the "service" worker.
//! - Execute service processes (fork/exec), track their lifecycle.
//! - Report `method.result` and `unit.state_update` messages back to System A.

mod controller;
mod dbus;
mod ipc;
mod notify;
mod pam;
mod process;
mod reaper;
mod state;

use anyhow::Result;
use clap::Parser;
use tracing::{info, warn};

use crate::state::ServiceState;

#[derive(Parser)]
#[command(name = "systema-syss", about = "System S — System Service Worker")]
struct Args {
    #[arg(long, short = 'D', help = "Enable debug-level logging")]
    debug: bool,

    #[arg(
        long,
        default_value = "info",
        help = "Log level (trace, debug, info, warn, error)"
    )]
    log_level: String,

    #[arg(long, help = "Do not register as a child subreaper")]
    no_raper: bool,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    sysa::paths::init();
    sysa::l10n::init();

    let args = {
        use clap::{CommandFactory, FromArgMatches};
        let cmd = Args::command()
            .about(sysa::l10n::t_("System S — System Service Worker"))
            .mut_arg("debug", |a| {
                a.help(sysa::l10n::t_("Enable debug-level logging."))
            })
            .mut_arg("log_level", |a| {
                a.help(sysa::l10n::t_(
                    "Log level (trace, debug, info, warn, error).",
                ))
            })
            .mut_arg("no_raper", |a| {
                a.help(sysa::l10n::t_("Do not register as a child subreaper."))
            });
        Args::from_arg_matches(&cmd.get_matches()).unwrap_or_else(|e| e.exit())
    };
    let log_level = if args.debug { "debug" } else { &args.log_level };
    // Self-managed logging: <log-dir>/<name>.log, or stderr for "-".
    sysa::logging::init(sysa::paths::instance().log_dir, "systema-syss", log_level);

    info!("System S (System Service Worker) starting up");

    #[cfg(any(target_os = "linux", target_os = "android"))]
    if !args.no_raper {
        // `nix` gates its `prctl` module on `target_os = "linux"` alone
        // (src/sys/mod.rs:58), so android has no `set_child_subreaper`.
        // This is nix's own `prctl_set_bool` body spelled out — same
        // arguments, and bionic's prctl(2) takes them the same way.
        let on = true as libc::c_ulong;
        let rc = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, on, 0, 0, 0) };
        if rc != 0 {
            let e = std::io::Error::last_os_error();
            warn!("PR_SET_CHILD_SUBREAPER failed (services may be orphaned): {e}");
        }
        // Adopted orphans — and, worse, zombies inherited with them — are
        // then collected by `reaper::run()` for the rest of our life.
    }

    #[cfg(any(target_os = "freebsd", target_os = "dragonfly"))]
    if !args.no_raper {
        // SAFETY: procctl(P_PID, 0, PROC_REAP_ACQUIRE, NULL) declares the
        // calling process as a reaper; NULL data is required and safe here.
        if unsafe { libc::procctl(libc::P_PID, 0, libc::PROC_REAP_ACQUIRE, std::ptr::null_mut()) } != 0 {
            let err = std::io::Error::last_os_error();
            warn!("PROC_REAP_ACQUIRE failed (services may be orphaned): {err}");
        }
    }

    let shared = ipc::shared_registry();
    tokio::select! {
        // SIGTERM/SIGINT are handled inside the IPC loop: it stops our
        // services first (the `on_shutdown` hook wired up in `ipc::run`),
        // then sends `worker.exit`, waits for System A to close the
        // connection, and only then returns — racing a signal branch here
        // would drop the socket before the goodbye is exchanged.
        result = ipc::run(&shared) => result?,
        // Orphan reaper: while the worker runs, collect every child no unit
        // is waiting for (adopted orphans under subreaper mode, plus direct
        // children nobody reaps anymore).  It overlaps the shutdown hook —
        // both it and `reap_children()` merely collect zombies, and the
        // kernel refuses a second reap of the same child — and is dropped
        // once the goodbye completes.
        _reaped = reaper::run(&shared) => {}
    }

    // Fallback for leaving the loop *without* a signal (System A went away
    // first): the hook never ran, so stop the services now.  When it did
    // run, the registry is already taken and this returns immediately.
    shutdown(shared).await;

    Ok(())
}

/// Graceful shutdown: SIGTERM every running child, wait, then SIGKILL
/// stragglers and reap the rest.
async fn shutdown(shared: ipc::SharedRegistry) {
    let registry = match shared.lock().await.take() {
        Some(r) => r,
        None => return,
    };

    // Collect PIDs of all running services.
    let targets: Vec<(String, u32)> = {
        let reg = registry.lock();
        reg.iter()
            .filter_map(|(name, inst)| {
                if matches!(inst.state, ServiceState::Running | ServiceState::Starting) {
                    inst.main_pid.map(|pid| (name.clone(), pid))
                } else {
                    None
                }
            })
            .collect()
    };

    if targets.is_empty() {
        info!("Shutdown: no running services to stop");
        reap_children();
        return;
    }

    info!(
        "Shutdown: stopping {} service(s): {:?}",
        targets.len(),
        targets.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>()
    );

    // Phase 1: SIGTERM every running child.
    use nix::sys::signal::{self, Signal};
    use nix::unistd::Pid;
    for (name, pid) in &targets {
        let _ = signal::kill(Pid::from_raw(*pid as i32), Signal::SIGTERM);
        info!("Shutdown: sent SIGTERM to {name} (PID {pid})");
    }

    // Phase 2: wait up to 15 seconds for them to exit.
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(15);
    let mut remaining: std::collections::HashSet<u32> =
        targets.iter().map(|(_, pid)| *pid).collect();
    while !remaining.is_empty() && tokio::time::Instant::now() < deadline {
        for pid in remaining.clone().iter() {
            if !process::is_alive(*pid) {
                remaining.remove(pid);
            }
        }
        if !remaining.is_empty() {
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
    }

    // Phase 3: SIGKILL stragglers.
    for pid in &remaining {
        warn!("Shutdown: PID {pid} did not exit in 15s; sending SIGKILL");
        let _ = signal::kill(Pid::from_raw(*pid as i32), Signal::SIGKILL);
    }

    // Phase 4: reap every remaining child.
    reap_children();
    info!("Shutdown: all children reaped");
}

/// Reap every remaining child (zombie) with a blocking `waitpid(-1)` loop.
fn reap_children() {
    loop {
        // SAFETY: waitpid(-1, WNOHANG) collects any zombie child without
        // blocking.  Returns -1 (ECHILD) when there are no more children.
        let ret = unsafe { nix::libc::waitpid(-1, std::ptr::null_mut(), nix::libc::WNOHANG) };
        if ret <= 0 {
            break;
        }
        info!("Shutdown: reaped PID {ret}");
    }
}
