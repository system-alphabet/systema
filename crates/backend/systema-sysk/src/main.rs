mod activation;
mod controller;
mod ipc;
mod socket;

use anyhow::Result;
use clap::Parser;
use tracing::info;

#[derive(Parser)]
#[command(name = "systema-sysk", about = "System K — System Socket Worker")]
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
        help = "Path of this process's log file (\"-\" writes to stderr only)"
    )]
    log_file: Option<std::path::PathBuf>,

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
    sysa::signals::install_signal_guard();

    let args = {
        use clap::{CommandFactory, FromArgMatches};
        let cmd = Args::command()
            .about(sysa::l10n::t_("System K — System Socket Worker"))
            .mut_arg("debug", |a| {
                a.help(sysa::l10n::t_("Enable debug-level logging."))
            })
            .mut_arg("log_file", |a| {
                a.help(sysa::l10n::t_(
                    "Path of this process's log file (\"-\" writes to stderr only).",
                ))
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
    // Self-managed logging: stderr always, plus the file resolved from
    // --log-file / SYSTEMA_LOG_FILE / the built-in default.
    sysa::logging::init_daemon("systema-sysk", args.log_file.clone(), log_level);

    info!("System K (System Socket Worker) starting up");

    // SIGTERM/SIGINT are handled inside the IPC loop: it sends
    // `worker.exit`, waits for System A to close the connection, and only
    // then returns here.
    ipc::run().await?;

    Ok(())
}
