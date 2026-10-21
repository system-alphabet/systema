//! systema-sysw.systemd — System Wrapper systemd D-Bus bridge
//!
//! Serves the org.freedesktop.systemd1 D-Bus surface on the system bus,
//! backed by System A over the control socket.  Properties, job lists and
//! unit listings are read from a local mirror that is refreshed by
//! control-port lifecycle events; mutations are forwarded to System A as
//! `manager.*` RPCs.

mod bridge;
mod dbus;
mod mirror;

use anyhow::Result;
use clap::Parser;

#[derive(Parser)]
#[command(name = "systema-sysw.systemd", about = "System Wrapper — systemd D-Bus bridge")]
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

    #[arg(long, help = "D-Bus address to use (default: the system bus)")]
    bus_address: Option<String>,

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
            .about(sysa::l10n::t_("System Wrapper — systemd D-Bus bridge"))
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
            .mut_arg("bus_address", |a| {
                a.help(sysa::l10n::t_(
                    "D-Bus address to use (default: the system bus).",
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
    // Fail fast on a malformed address, before anything connects anywhere
    // (plan 备注 11): the workers must not know which bus domain they are
    // in — the launcher decides it per spawn.
    let bus_address = match args.bus_address {
        Some(raw) => Some(raw.parse::<zbus::Address>().map_err(|e| {
            anyhow::anyhow!(sysa::l10n::fmt(
                sysa::l10n::t_("invalid --bus-address: {err}"),
                &[("err", &e.to_string())]
            ))
        })?),
        None => None,
    };
    let log_level = if args.debug { "debug" } else { &args.log_level };
    // Self-managed logging: stderr always, plus the file resolved from
    // --log-file / SYSTEMA_LOG_FILE / the built-in default.
    sysa::logging::init_daemon("systema-sysw.systemd", args.log_file.clone(), log_level);

    // Reconnect loop: keep serving D-Bus across control-session restarts.
    // A SIGTERM/SIGINT request (from System Init or a console) ends it.
    loop {
        let result = tokio::select! {
            biased;
            _sig = sysa::signals::shutdown_signal() => {
                break;
            }
            result = bridge::run(bus_address.clone()) => result,
        };
        match result {
            Ok(()) => {
                tracing::info!("Bridge exited cleanly; reconnecting");
            }
            Err(e) => {
                tracing::warn!("Bridge session ended: {e:#}; reconnecting");
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    Ok(())
}