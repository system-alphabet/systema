mod controller;
mod ipc;
mod mount;
mod mounttable;
mod state;

use anyhow::Result;
use clap::Parser;
use tracing::info;

#[derive(Parser)]
#[command(name = "systema-sysm", about = "System M — System Mount Worker")]
struct Args {
    #[arg(long, short = 'D', help = "Enable debug-level logging")]
    debug: bool,

    #[arg(
        long,
        default_value = "info",
        help = "Log level (trace, debug, info, warn, error)"
    )]
    log_level: String,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    sysa::paths::init();
    sysa::l10n::init();

    let args = {
        use clap::{CommandFactory, FromArgMatches};
        let cmd = Args::command()
            .about(sysa::l10n::t_("System M — System Mount Worker"))
            .mut_arg("debug", |a| {
                a.help(sysa::l10n::t_("Enable debug-level logging."))
            })
            .mut_arg("log_level", |a| {
                a.help(sysa::l10n::t_(
                    "Log level (trace, debug, info, warn, error).",
                ))
            });
        Args::from_arg_matches(&cmd.get_matches()).unwrap_or_else(|e| e.exit())
    };
    let log_level = if args.debug { "debug" } else { &args.log_level };
    // Self-managed logging: <log-dir>/<name>.log, or stderr when the log
    // file cannot be opened — same contract as the other daemons.
    sysa::logging::init(sysa::paths::instance().log_dir, "systema-sysm", log_level);

    info!("System M (System Mount Worker) starting up");

    // SIGTERM/SIGINT are handled inside the IPC loop: it sends
    // `worker.exit`, waits for System A to close the connection, and only
    // then returns here.
    ipc::run().await?;

    Ok(())
}
