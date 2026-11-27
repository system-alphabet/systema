//! System P — a configurable state-only worker for selected unit types.

mod controller;
mod ipc;
mod state;

use anyhow::Result;
use clap::Parser;
use tracing::info;

use crate::state::{StateMode, UnitState};

#[derive(Debug, Parser)]
#[command(name = "systema-placeholder")]
struct Args {
    #[arg(
        long,
        value_name = "XXX:YYY:ZZZ",
        value_parser = parse_manage
    )]
    manage: Option<String>,

    #[arg(long, value_enum, default_value = "follow")]
    state: StateMode,

    #[arg(long, value_enum, default_value = "inactive")]
    default: UnitState,

    #[arg(long, short = 'D')]
    debug: bool,

    #[arg(long, default_value = "warn")]
    log_level: String,

    #[arg(long)]
    log_file: Option<std::path::PathBuf>,

    #[arg(long, short = 'v')]
    version: bool,

    #[arg(long)]
    full_version: bool,
}

fn parse_manage(value: &str) -> std::result::Result<String, String> {
    let types: Vec<String> = value
        .split(':')
        .map(str::trim)
        .map(|unit_type| unit_type.strip_prefix('.').unwrap_or(unit_type).trim())
        .map(str::to_string)
        .collect();
    if types
        .iter()
        .any(|unit_type| unit_type.is_empty() || unit_type.chars().any(char::is_whitespace))
    {
        return Err(sysa::l10n::t_(
            "The --manage value must contain non-empty unit types separated by ':'.",
        )
        .to_string());
    }
    Ok(types.join(":"))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    sysa::paths::init();
    sysa::l10n::init(sysa::paths::instance().locale_dir);
    sysa::signals::install_signal_guard();

    let args = {
        use clap::{CommandFactory, FromArgMatches};
        let cmd = Args::command()
            .about(sysa::l10n::t_("System P — Placeholder Worker"))
            .mut_arg("manage", |a| {
                a.help(sysa::l10n::t_(
                    "Required unit types to manage, separated by ':'; leading dots are optional (for example: service:target).",
                ))
            })
            .mut_arg("state", |a| {
                a.help(sysa::l10n::t_("State behavior for start and stop."))
            })
            .mut_arg("default", |a| {
                a.help(sysa::l10n::t_(
                    "State reported before any start or stop action.",
                ))
            })
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

    let Some(manage) = args.manage else {
        use clap::CommandFactory;
        Args::command()
            .error(
                clap::error::ErrorKind::MissingRequiredArgument,
                sysa::l10n::t_(
                    "The --manage value must contain non-empty unit types separated by ':'.",
                ),
            )
            .exit();
    };
    let unit_types: Vec<&str> = manage.split(':').collect();
    let log_level = if args.debug { "debug" } else { &args.log_level };
    sysa::logging::init_daemon("systema-placeholder", args.log_file.clone(), log_level);

    info!(
        "System P (Placeholder Worker) starting; managing {}",
        manage
    );
    ipc::run(&unit_types, args.state, args.default).await
}

#[cfg(test)]
mod tests {
    use super::parse_manage;

    #[test]
    fn manage_value_accepts_unit_type_lists_and_optional_dots() {
        assert_eq!(parse_manage(".service:target").unwrap(), "service:target");
    }

    #[test]
    fn manage_value_rejects_empty_entries() {
        assert!(parse_manage("").is_err());
        assert!(parse_manage("service::target").is_err());
    }
}
