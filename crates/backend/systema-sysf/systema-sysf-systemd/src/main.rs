//! systema-sysf.systemd — System F systemd finder
//!
//! The systemd-specific finder executable.  Parses systemd unit files
//! from the standard systemd search paths and submits them to the System A
//! staging area (`finder.register_units`).  It has no commit command: the
//! generic `systema-sysf` worker runs every finder executable and commits
//! the staging area into the active set afterwards.

use std::collections::HashMap;

use anyhow::Result;
use clap::Parser;
use sysa::finder::UnitFinder;
use systema_sysf::FinderRegistry;
use systema_sysf_systemd::finder::SystemdFinder;
use tracing::{error, info};

#[derive(Parser)]
#[command(name = "systema-sysf.systemd", about = "System F — systemd finder")]
struct Args {
    #[arg(long, short = 'D', help = "Enable debug-level logging")]
    debug: bool,

    #[arg(
        long,
        default_value = "info",
        help = "Log level (trace, debug, info, warn, error)"
    )]
    log_level: String,

    #[arg(
        long,
        short = 'n',
        default_value = "systema-sysf/discovery",
        help = "Name for the staging area"
    )]
    name: String,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    sysa::paths::init();
    sysa::l10n::init();

    let args = {
        use clap::{CommandFactory, FromArgMatches};
        let cmd = Args::command()
            .about(sysa::l10n::t_("System F — systemd finder"))
            .mut_arg("debug", |a| {
                a.help(sysa::l10n::t_("Enable debug-level logging."))
            })
            .mut_arg("log_level", |a| {
                a.help(sysa::l10n::t_(
                    "Log level (trace, debug, info, warn, error).",
                ))
            })
            .mut_arg("name", |a| {
                a.help(sysa::l10n::t_("Name for the staging area."))
            });
        Args::from_arg_matches(&cmd.get_matches()).unwrap_or_else(|e| e.exit())
    };
    let log_level = if args.debug { "debug" } else { &args.log_level };
    // Self-managed logging: <log-dir>/<name>.log, or stderr when the log
    // file cannot be opened — same contract as the other daemons.
    sysa::logging::init(
        sysa::paths::instance().log_dir,
        "systema-sysf.systemd",
        log_level,
    );

    run_register(&args.name).await
}

async fn run_register(name: &str) -> Result<()> {
    info!("System F systemd finder registering units (name='{name}')");

    let mut registry = FinderRegistry::new();
    registry.register(std::sync::Arc::new(SystemdFinder));
    let units: HashMap<String, systema_sysf::ir::UnitIR> = registry.discover_all().await?;
    info!("Discovered {} units", units.len());

    let json = serde_json::to_vec(&units)?;
    let client = UnitFinder::new();
    let ack = client.register_units(name, json).await?;
    if ack.success {
        info!("Staging successful: {} units registered", ack.unit_count);
    } else {
        error!("Staging failed: {}", ack.message);
        anyhow::bail!(sysa::l10n::fmt(
            sysa::l10n::t_("Staging failed: {message}."),
            &[("message", &ack.message)]
        ));
    }

    info!("System F systemd finder register complete");
    Ok(())
}
