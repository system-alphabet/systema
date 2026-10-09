mod yaml;

use std::io::IsTerminal;

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, ValueEnum};
use colored::*;
use regex::Regex;
use serde_json::Value;
use sysa::l10n;
use sysa::unitstate_admin::UnitStateAdmin;
use sysa_pager::{pager_eprintln, pager_println, PagerConfig, PagerGuard};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "systema-unitstatectl",
    about = "System A — Unit State Controller"
)]
struct Cli {
    #[arg(long, global = true, short = 'D', help = "Enable debug-level logging")]
    debug: bool,

    #[arg(
        long,
        global = true,
        default_value = "warn",
        help = "Log level (trace, debug, info, warn, error)"
    )]
    log_level: String,

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

    #[arg(
        long,
        global = true,
        short = 'v',
        help = "Print version information and exit"
    )]
    version: bool,

    #[arg(
        long,
        global = true,
        help = "Print full version: build options and compile-time paths"
    )]
    full_version: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(clap::Subcommand)]
enum Commands {
    /// List the System Allocator's cached unit state.
    List {
        /// Unit name.  Tried as an exact match first; if no unit matches
        /// exactly, treated as a regular expression over unit names.
        name: Option<String>,
    },
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

fn init_tracing(level: &str) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| level.parse().unwrap());
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

fn render_unit(name: &str, json: &[u8], color: bool) -> Result<()> {
    let doc: Value =
        serde_json::from_slice(json).context(l10n::t_("Failed to parse unit state JSON."))?;
    let mut buf = String::new();
    let render_error = l10n::t_("Failed to render unit as YAML.");
    yaml::render_doc(&mut buf, name, &doc, color)
        .map_err(|e| anyhow::anyhow!("{}: {e}", render_error))?;
    pager_println!("{buf}");
    Ok(())
}

async fn run(color: bool, name: Option<String>) -> Result<()> {
    let admin = UnitStateAdmin::new();
    let mut rendered = 0u32;
    let (result, requested, filtered) = match &name {
        Some(requested) => {
            // Stream everything first: whether `requested` selects a single
            // exact unit or filters as a regex depends on the whole set, so
            // the match cannot be decided entry-by-entry.
            let mut units: Vec<(String, Vec<u8>)> = Vec::new();
            let result = admin
                .list(|n, json| {
                    units.push((n.to_string(), json.to_vec()));
                    Ok(())
                })
                .await?;

            if let Some((n, json)) = units.iter().find(|(n, _)| n == requested) {
                render_unit(n, json, color)?;
                rendered = 1;
            } else {
                let re = Regex::new(requested).with_context(|| {
                    l10n::fmt(
                        l10n::t_(
                            "No unit's name matches '{name}' exactly, and '{name}' is not a valid regular expression.",
                        ),
                        &[("name", requested)],
                    )
                })?;
                for (n, json) in &units {
                    if re.is_match(n) {
                        render_unit(n, json, color)?;
                        rendered += 1;
                    }
                }
            }
            (result, requested.as_str(), true)
        }
        None => {
            let result = admin
                .list(|n, json| {
                    rendered += 1;
                    render_unit(n, json, color)
                })
                .await?;
            (result, "", false)
        }
    };

    if !result.message.is_empty() {
        pager_eprintln!(
            "{}",
            l10n::fmt(
                l10n::t_("Warning: {message}"),
                &[("message", &result.message)]
            )
            .yellow()
        );
    } else if filtered && rendered == 0 {
        pager_eprintln!(
            "{}",
            l10n::fmt(
                l10n::t_("No unit matches '{name}'."),
                &[("name", requested)]
            )
            .dimmed()
        );
    } else {
        pager_eprintln!(
            "{}",
            l10n::fmt(
                l10n::t_("Total {total} unit(s)."),
                &[("total", &rendered.to_string())]
            )
            .dimmed()
        );
    }
    Ok(())
}

fn build_localized_cli() -> clap::Command {
    Cli::command()
        .about(l10n::t_("System A — Unit State Controller"))
        .mut_arg("debug", |a| a.help(l10n::t_("Enable debug-level logging.")))
        .mut_arg("log_level", |a| {
            a.help(l10n::t_("Log level (trace, debug, info, warn, error)."))
        })
        .mut_arg("no_pager", |a| {
            a.help(l10n::t_("Do not pipe output into a pager."))
        })
        .mut_arg("color", |a| {
            a.help(l10n::t_("When to use colors (always, auto, never)."))
        })
        .mut_arg("version", |a| {
            a.help(l10n::t_("Print version information and exit."))
        })
        .mut_arg("full_version", |a| {
            a.help(l10n::t_(
                "Print full version: build options and compile-time paths.",
            ))
        })
        .mut_subcommand("list", |cmd| {
            cmd.about(l10n::t_("List the System Allocator's cached unit state."))
                .mut_arg("name", |a| {
                    a.help(l10n::t_(
                        "Unit name; tried as an exact match first, otherwise treated as a regular expression.",
                    ))
                })
        })
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    sysa::paths::init();
    sysa::l10n::init();

    let matches = build_localized_cli().get_matches();
    if *matches.get_one::<bool>("version").unwrap_or(&false) {
        sysa::version::print_version();
        return Ok(());
    }
    if *matches.get_one::<bool>("full_version").unwrap_or(&false) {
        sysa::version::print_full_version();
        return Ok(());
    }
    let debug = *matches.get_one::<bool>("debug").unwrap_or(&false);
    let log_level = matches
        .get_one::<String>("log_level")
        .map(|s| s.as_str())
        .unwrap_or("warn");
    let no_pager = *matches.get_one::<bool>("no_pager").unwrap_or(&false);
    let color = *matches
        .get_one::<ColorChoice>("color")
        .unwrap_or(&ColorChoice::Auto);

    let level = if debug { "debug" } else { log_level };
    init_tracing(level);
    sysa::paths::debug_dump(sysa::paths::instance().log_dir);

    let use_pager = !no_pager;
    let _guard: PagerGuard = sysa_pager::open(PagerConfig {
        disable: !use_pager,
    })?;

    // Resolve `--color` to a concrete toggle.
    let color_enabled = match color {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        // auto: color when stdout is a terminal, or when paging (the pager
        // renders the ANSI escape sequences for the terminal it writes to).
        ColorChoice::Auto => use_pager || std::io::stdout().is_terminal(),
    };
    // The `--color` decision also drives the process-wide `colored` override,
    // so the pager footer messages match the YAML body.
    colored::control::set_override(color_enabled);

    // No subcommand defaults to `list`.
    let name = match matches.subcommand() {
        Some(("list", sub_m)) => sub_m.get_one::<String>("name").cloned(),
        _ => None,
    };

    let result = run(color_enabled, name).await;

    match result {
        Err(e) => match e.downcast_ref::<std::io::Error>() {
            Some(ioe) if ioe.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
            _ => Err(e),
        },
        Ok(()) => Ok(()),
    }
}
