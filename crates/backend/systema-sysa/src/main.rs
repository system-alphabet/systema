//! system-a — System Allocator
//!
//! The global control plane of System Alphabet.  Responsibilities:
//! - Load and parse systemd unit files.
//! - Build and maintain the unit dependency graph.
//! - Generate and dispatch Tasks to System Workers via Unix-socket IPC.
//! - Maintain *desired state* for each unit (never actual state).
//! - Expose the allocator control plane on the control socket.  The
//!   well-known `systemd1` D-Bus surface is served by the System Wrapper
//!   bridge flavors (systema-sysw.systemd) instead.

mod event;
mod events;
mod graph;
mod ipc;
mod reload_task;
mod scheduler;
mod snapshot;
mod state;
mod unit;
mod unitstate;

use anyhow::Result;
use clap::Parser;
use tracing::info;

#[derive(Parser)]
#[command(name = "systema-sysa", about = "System A — System Allocator")]
struct Args {
    #[arg(long, short = 'D', help = "Enable debug-level logging")]
    debug: bool,

    #[arg(
        long,
        default_value = "info",
        help = "Log level (trace, debug, info, warn, error)"
    )]
    log_level: String,

    #[arg(long, short = 'v', help = "Print version information and exit")]
    version: bool,

    #[arg(
        long,
        help = "Print full version: build options and compile-time paths"
    )]
    full_version: bool,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    sysa::paths::init();
    sysa::l10n::init();

    let args = {
        use clap::{CommandFactory, FromArgMatches};
        let cmd = Args::command()
            .about(sysa::l10n::t_("System A — System Allocator"))
            .mut_arg("debug", |a| {
                a.help(sysa::l10n::t_("Enable debug-level logging."))
            })
            .mut_arg("log_level", |a| {
                a.help(sysa::l10n::t_(
                    "Log level (trace, debug, info, warn, error).",
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
    };
    if args.version {
        sysa::version::print_version();
        return Ok(());
    }
    if args.full_version {
        sysa::version::print_full_version();
        return Ok(());
    }
    let log_level = if args.debug { "debug" } else { &args.log_level };
    // Self-managed logging: <log-dir>/<name>.log, or stderr for "-".
    sysa::logging::init(sysa::paths::instance().log_dir, "systema-sysa", log_level);

    info!("System A (System Allocator) starting up");

    // Create /run/systemd/system if it doesn't exist.  Many systemd units
    // expect this directory to be present.
    let runstatedir = sysa::paths::instance().runstatedir;
    let systemd_system_dir = format!("{runstatedir}/systemd/system");
    if let Err(e) = std::fs::create_dir_all(&systemd_system_dir) {
        tracing::warn!("Failed to create {systemd_system_dir}: {e}");
    }

    // Shared allocator state accessible from the IPC server and the
    // control-plane event dispatch.
    let allocator = state::Allocator::handle();

    // Spawn the ReloadTask: serialises all finder-commit and reload
    // operations through a single consumer.  System A never scans unit
    // files directly.
    let (reload_tx, _reload_handle) = reload_task::ReloadTask::spawn(allocator.clone());
    {
        let mut state = allocator.write();
        state.reload_tx = Some(reload_tx);
    }

    // Register in-process event-bus subscribers.
    {
        use std::sync::Arc;
        let bus = allocator.read().event_bus.clone();
        let mut bus_w = bus.write().await;
        bus_w.subscribe(Arc::new(event::RestartHandler::new(allocator.clone())));
        bus_w.subscribe(Arc::new(event::NotifyBroadcaster::new()));
    }

    // Start the IPC server (accepts System Worker & Finder connections).
    // The Finder (System F) may connect at any time — each commit replaces
    // the entire unit set.  There is no "first load" special case.
    let ipc_handle = tokio::spawn(ipc::server::run(allocator.clone()));

    // Start the in-process lifecycle-event dispatch (consumed by control-port
    // sessions and forwarded to the System Wrapper bridge flavors).  Runs for
    // the process lifetime.
    tokio::spawn(events::run(allocator.clone()));

    // Wait for the IPC server, or a graceful-shutdown request (SIGTERM from
    // System Init during a power transition, or an interactive SIGINT).  On a
    // signal we unwind the runtime so sockets and sessions are dropped
    // cleanly.
    tokio::select! {
        result = ipc_handle => {
            result??;
        }
        _sig = sysa::signals::shutdown_signal() => {}
    }

    Ok(())
}
