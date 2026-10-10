//! system-n — System Notify Worker
//!
//! The path-trigger worker for System Alphabet. Responsibilities:
//! - Connect to System A's IPC socket and register as the "path" worker.
//! - Own every `.path` unit: watch the filesystem for its conditions
//!   (`PathExists=`, `PathExistsGlob=`, `PathChanged=`, `PathModified=`,
//!   `DirectoryNotEmpty=`).
//! - When a condition fires, ask System A to activate the path unit's target
//!   unit (`path.fired`), then wait for the target to terminate before
//!   re-arming.
//! - Report `method.result` and `unit.state_update` messages back to System A.

mod backend;
mod controller;
mod engine;
mod ipc;
mod spec;
mod state;

use anyhow::Result;
use clap::Parser;
use tracing::info;

#[derive(Parser)]
#[command(name = "systema-sysn", about = "System N — System Notify Worker")]
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
            .about(sysa::l10n::t_("System N — System Notify Worker"))
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
    sysa::logging::init(sysa::paths::instance().log_dir, "systema-sysn", log_level);

    info!("System N (System Notify Worker) starting up");

    // SIGTERM/SIGINT are handled inside the IPC loop: it sends
    // `worker.exit`, waits for System A to close the connection, and only
    // then returns here.
    ipc::run().await?;

    Ok(())
}
