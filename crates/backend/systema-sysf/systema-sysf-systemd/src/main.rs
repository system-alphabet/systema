//! systema-sysf.systemd — System F systemd finder
//!
//! The systemd-specific finder executable.  Parses systemd unit files
//! from the standard systemd search paths and submits them to the System A
//! staging area (`finder.register_units`).  It has no commit command: the
//! generic `systema-sysf` worker runs every finder executable and commits
//! the staging area into the active set afterwards.
//!
//! The `list` subcommand runs the same discovery without contacting System A
//! and renders every detected `UnitIR` as YAML on stdout, using the pager and
//! highlighter shared with `systema-unitstatectl`.

use std::collections::{BTreeMap, HashMap};
use std::io::IsTerminal;

use anyhow::Result;
use clap::{CommandFactory, FromArgMatches, Parser, ValueEnum};
use colored::Colorize;
use sysa::finder::UnitFinder;
use sysa_pager::{pager_eprintln, pager_println, PagerConfig, PagerGuard};
use systema_sysf::ir::UnitIR;
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
        default_value = "warn",
        help = "Log level (trace, debug, info, warn, error)"
    )]
    log_level: String,

    #[arg(
        long,
        help = "Path of this process's log file (\"-\" writes to stderr only)"
    )]
    log_file: Option<std::path::PathBuf>,

    #[arg(
        long,
        short = 'n',
        default_value = "systema-sysf/discovery",
        help = "Name for the staging area"
    )]
    name: String,

    #[arg(long, short = 'v', help = "Print version information and exit")]
    version: bool,

    #[arg(
        long,
        help = "Print full version: build options and compile-time paths"
    )]
    full_version: bool,

    #[arg(long, global = true, help = "Do not pipe output into a pager")]
    no_pager: bool,

    #[arg(
        long,
        global = true,
        value_enum,
        default_value_t = ColorChoice::Auto,
        help = "When to use colors"
    )]
    color: ColorChoice,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand)]
enum Command {
    /// List all detected units as YAML
    List,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum ColorChoice {
    /// Use colors when output is (or is piped to) a terminal.
    Auto,
    /// Never use colors.
    Never,
    /// Always use colors.
    Always,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    sysa::paths::init();
    sysa::l10n::init(sysa::paths::instance().locale_dir);
    sysa::signals::install_signal_guard();

    let args = {
        let cmd = Args::command()
            .about(sysa::l10n::t_("System F — systemd finder"))
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
            .mut_arg("name", |a| {
                a.help(sysa::l10n::t_("Name for the staging area."))
            })
            .mut_arg("version", |a| {
                a.help(sysa::l10n::t_("Print version information and exit."))
            })
            .mut_arg("full_version", |a| {
                a.help(sysa::l10n::t_(
                    "Print full version: build options and compile-time paths.",
                ))
            })
            .mut_arg("no_pager", |a| {
                a.help(sysa::l10n::t_("Do not pipe output into a pager."))
            })
            .mut_arg("color", |a| {
                a.help(sysa::l10n::t_("When to use colors (always, auto, never)."))
            })
            .mut_subcommand("list", |cmd| {
                cmd.about(sysa::l10n::t_("List all detected units as YAML."))
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
    match args.command {
        Some(Command::List) => {
            // Tool policy: stderr only with --debug, a file only with an
            // explicit --log-file (same as systema-unitstatectl).
            sysa::logging::init_tool(
                "systema-sysf.systemd",
                args.log_file.clone(),
                log_level,
                args.debug,
            );
            run_list(args.color, args.no_pager).await
        }
        None => {
            // Self-managed logging: stderr always, plus the file resolved
            // from --log-file / SYSTEMA_LOG_FILE / the built-in default.
            sysa::logging::init_daemon("systema-sysf.systemd", args.log_file.clone(), log_level);
            run_register(&args.name).await
        }
    }
}

/// Discover the units this finder sees and render each one as YAML.
///
/// This is the offline counterpart of [`run_register`]: it never contacts
/// System A, so it also works when no allocator is running.  The pager and
/// the highlighter are the ones `systema-unitstatectl list` uses.
async fn run_list(color: ColorChoice, no_pager: bool) -> Result<()> {
    let use_pager = !no_pager;
    let _guard: PagerGuard = sysa_pager::open(PagerConfig {
        disable: !use_pager,
    })?;

    // `auto`: color when stdout is a terminal, or when paging (the pager
    // renders the ANSI escape sequences for the terminal it writes to).
    let color_enabled = match color {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => use_pager || std::io::stdout().is_terminal(),
    };
    // The `--color` decision also drives the process-wide `colored` override,
    // so the footer message matches the YAML body.
    colored::control::set_override(color_enabled);

    let mut registry = FinderRegistry::new();
    registry.register(std::sync::Arc::new(SystemdFinder));
    let units: BTreeMap<String, UnitIR> = registry.discover_all().await?.into_iter().collect();

    for (name, unit) in &units {
        pager_println!(
            "{}",
            sysa::unit_yaml::render_unit_ir(name, unit, color_enabled)?
        );
    }

    pager_eprintln!(
        "{}",
        sysa::l10n::fmt(
            sysa::l10n::t_("Total {total} unit(s)."),
            &[("total", &units.len().to_string())]
        )
        .dimmed()
    );
    Ok(())
}

async fn run_register(name: &str) -> Result<()> {
    info!("System F systemd finder registering units (name='{name}')");

    let mut registry = FinderRegistry::new();
    registry.register(std::sync::Arc::new(SystemdFinder));
    let units: HashMap<String, systema_sysf::ir::UnitIR> = registry.discover_all().await?;
    info!("Discovered {} units", units.len());

    let json = serde_json::to_vec(&units)?;
    let client = UnitFinder::with_socket(sysa::paths::instance().ipc_socket_path);
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
