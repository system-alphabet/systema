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

    #[arg(
        long,
        global = true,
        help = "Log file to append to; without it, nothing is written to a file"
    )]
    log_file: Option<std::path::PathBuf>,

    #[arg(long, global = true, help = "Do not pipe output into a pager")]
    no_pager: bool,

    #[arg(
        long,
        global = true,
        help = "Talk to the per-user instance (systema-sysu) instead of the system instance"
    )]
    user: bool,

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
    let admin = UnitStateAdmin::with_socket(sysa::paths::instance().control_socket_path);
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
        .mut_arg("log_file", |a| {
            a.help(l10n::t_(
                "Log file to append to; without it, nothing is written to a file.",
            ))
        })
        .mut_arg("log_level", |a| {
            a.help(l10n::t_("Log level (trace, debug, info, warn, error)."))
        })
        .mut_arg("no_pager", |a| {
            a.help(l10n::t_("Do not pipe output into a pager."))
        })
        .mut_arg("user", |a| {
            a.help(l10n::t_(
                "Talk to the per-user instance (systema-sysu) instead of the system instance.",
            ))
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
    // `--user` must take effect before paths::init() freezes the resolved
    // socket paths: scan argv first, then point the control socket at the
    // per-user instance.  clap still registers the flag (so `--help`
    // documents it); its parsed value is used below for the
    // "instance not running" check.
    if std::env::args().any(|a| a == "--user") {
        std::env::set_var("SYSTEMA_CONTROL_SOCKET", sysa::paths::user_control_socket());
    }
    sysa::paths::init();
    sysa::l10n::init(sysa::paths::instance().locale_dir);
    sysa::signals::install_signal_guard();

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
    let user = *matches.get_one::<bool>("user").unwrap_or(&false);
    let color = *matches
        .get_one::<ColorChoice>("color")
        .unwrap_or(&ColorChoice::Auto);

    let level = if debug { "debug" } else { log_level };
    // Tool policy: stderr only with -D/--debug, a file only with an
    // explicit --log-file (see the logging module).
    let log_file = matches.get_one::<std::path::PathBuf>("log_file").cloned();
    sysa::logging::init_tool("systema-unitstatectl", log_file, level, debug);

    // In `--user` mode a missing socket means the per-user instance was
    // never started — and a *stale* socket file (left behind by a crash or
    // an old logout) means it is not running either.  Probe with a real
    // connect so both cases (ENOENT, ECONNREFUSED) get this message up
    // front instead of a raw error deep inside the admin client; any other
    // failure (e.g. EACCES) is left to the admin client, which renders the
    // permission hint.
    if user {
        let socket = sysa::paths::instance().control_socket_path;
        if let Err(e) = std::os::unix::net::UnixStream::connect(socket) {
            match e.kind() {
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
                    anyhow::bail!(l10n::fmt(
                        l10n::t_(
                            "No per-user instance is listening on {socket}; start one with 'systema-sysu --login'.",
                        ),
                        &[("socket", socket)]
                    ))
                }
                _ => {}
            }
        }
    }

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
