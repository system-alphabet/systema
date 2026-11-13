#[path = "automount.rs"]
mod automount;
#[path = "controller.rs"]
mod controller;
#[path = "ipc/mod.rs"]
mod ipc;
#[path = "mount.rs"]
mod mount;
#[path = "mountinfo.rs"]
mod mountinfo;
#[path = "reaper.rs"]
mod reaper;
#[path = "state.rs"]
mod state;

use anyhow::Result;
use clap::Parser;
use tracing::{info, warn};

#[derive(Parser)]
#[command(
    name = "systema-sysm",
    about = "System M — System Mount Worker (Linux)"
)]
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

pub async fn run() -> Result<()> {
    sysa::paths::init();
    sysa::l10n::init(sysa::paths::instance().locale_dir);
    sysa::signals::install_signal_guard();

    let args = {
        use clap::{CommandFactory, FromArgMatches};
        let cmd = Args::command()
            .about(sysa::l10n::t_("System M — System Mount Worker (Linux)"))
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
    sysa::logging::init_daemon("systema-sysm.linux", args.log_file.clone(), log_level);

    info!("System M (System Mount Worker for Linux) starting up");

    // Adopt the daemons `mount(8)` leaves behind: the intermediate
    // `mount.fuse` parent exits, so without a subreaper the FUSE daemon is
    // reparented to PID 1.  As their parent we are also the one that has to
    // stop and reap them on shutdown (`reaper::stop_children`).
    if let Err(e) = sysa::reaper::set_child_subreaper(true) {
        warn!("Failed to register as a child subreaper (mount daemons may be orphaned): {e}");
    }

    // SIGTERM/SIGINT are handled inside the IPC loop: it sends
    // `worker.exit`, waits for System A to close the connection, and only
    // then returns here.
    let result = ipc::run().await;

    // Fallback for leaving the loop without its shutdown hook (the hook only
    // runs for a requested shutdown); idempotent when it already ran.
    reaper::stop_children().await;

    result
}
