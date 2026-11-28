//! systema-sysf.rc — System F SysV init (`rc`) finder
//!
//! The SysV-specific finder executable.  Parses init scripts from the
//! configured init.d search paths and submits them to the System A staging
//! area (`finder.register_units`).  It has no commit command: the generic
//! `systema-sysf` worker runs every finder executable and commits the staging
//! area into the active set afterwards.

use std::collections::HashMap;
use std::io::IsTerminal;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use colored::Colorize;
use sysa::finder::UnitFinder;
use sysa_pager::{pager_eprintln, pager_println, PagerConfig, PagerGuard};
use systema_sysf::ir::UnitIR;
use systema_sysf::FinderRegistry;
use systema_sysf_rc::finder::RcFinder;
use tracing::{error, info};

#[derive(Parser)]
#[command(name = "systema-sysf.rc", about = "System F — rc finder")]
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
    command: Option<Commands>,
}

#[derive(clap::Subcommand)]
enum Commands {
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
        use clap::{CommandFactory, FromArgMatches};
        let cmd = Args::command()
            .about(sysa::l10n::t_("System F — rc finder"))
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
            .mut_arg("no_pager", |a| {
                a.help(sysa::l10n::t_("Do not pipe output into a pager."))
            })
            .mut_arg("color", |a| {
                a.help(sysa::l10n::t_("When to use colors (always, auto, never)."))
            })
            .mut_subcommand("list", |cmd| {
                cmd.about(sysa::l10n::t_("List all detected units."))
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
    if let Some(Commands::List) = args.command {
        // Interactive command: same logging policy as `systema-unitstatectl`
        // (no implicit log file; stderr only with --debug).
        sysa::logging::init_tool(
            "systema-sysf.rc",
            args.log_file.clone(),
            log_level,
            args.debug,
        );
        return run_list(args.color, args.no_pager).await;
    }

    // Worker mode: stderr always, plus the file resolved from
    // --log-file / SYSTEMA_LOG_FILE / the built-in default.
    sysa::logging::init_daemon("systema-sysf.rc", args.log_file.clone(), log_level);

    run_register(&args.name).await
}

async fn run_register(name: &str) -> Result<()> {
    info!("System F rc finder registering units (name='{name}')");

    let mut registry = FinderRegistry::new();
    registry.register(RcFinder::new());
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

    info!("System F rc finder register complete");
    Ok(())
}

/// Render one detected [`UnitIR`] as a YAML document, in the same shape as
/// `systema-unitstatectl`.
fn render_unit(name: &str, unit: &UnitIR, color: bool) -> Result<()> {
    let value =
        serde_json::to_value(unit).context(sysa::l10n::t_("Failed to render unit as YAML."))?;
    let mut buf = String::new();
    let render_error = sysa::l10n::t_("Failed to render unit as YAML.");
    sysa::unit_yaml::render_doc(&mut buf, name, &value, color)
        .map_err(|e| anyhow::anyhow!("{}: {e}", render_error))?;
    pager_println!("{buf}");
    Ok(())
}

/// `list`: discover every unit and print the resulting IR set as YAML.
async fn run_list(color: ColorChoice, no_pager: bool) -> Result<()> {
    let use_pager = !no_pager;
    let _guard: PagerGuard = sysa_pager::open(PagerConfig {
        disable: !use_pager,
    })?;

    let color_enabled = match color {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        // auto: color when stdout is a terminal, or when paging (the pager
        // renders the ANSI escape sequences for the terminal it writes to).
        ColorChoice::Auto => use_pager || std::io::stdout().is_terminal(),
    };
    colored::control::set_override(color_enabled);

    let mut registry = FinderRegistry::new();
    registry.register(RcFinder::new());
    let units = registry.discover_all().await?;
    let mut units: Vec<(String, UnitIR)> = units.into_iter().collect();
    units.sort_by(|a, b| a.0.cmp(&b.0));

    for (id, unit) in &units {
        render_unit(id, unit, color_enabled)?;
    }

    let total = units.len().to_string();
    pager_eprintln!(
        "{}",
        sysa::l10n::fmt(
            sysa::l10n::t_("Total {total} unit(s)."),
            &[("total", &total)]
        )
        .dimmed()
    );
    Ok(())
}
