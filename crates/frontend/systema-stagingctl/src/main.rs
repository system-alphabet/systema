use std::collections::BTreeMap;

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, ValueEnum};
use colored::*;
use regex::Regex;
use serde_json::Value;
use sysa::l10n;
use sysa::proto::StagingAreaEntry;
use sysa::staging_admin::StagingAdmin;
use sysa_pager::{pager_eprintln, pager_print, pager_println, PagerConfig, PagerGuard};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "systema-stagingctl",
    about = "System A — Staging Area Controller"
)]
struct Args {
    #[arg(long, global = true, help = "Output in JSON format")]
    json: bool,

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

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum ColorChoice {
    Always,
    Auto,
    Never,
}

#[derive(clap::Subcommand)]
enum Commands {
    #[command(about = "List staging areas and their contents")]
    List {
        #[arg(long, help = "Filter by UID (can be specified multiple times)")]
        uid: Vec<u32>,

        #[arg(help = "Regex pattern(s) to match staging area name")]
        regex: Vec<String>,
    },

    #[command(about = "Commit staging areas into the active unit set")]
    Commit {
        #[arg(
            short,
            long,
            default_value_t = 0,
            help = "Commit every staging area owned by this UID (default: 0)"
        )]
        uid: u32,
    },
}

fn init_tracing(level: &str) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| level.parse().unwrap());
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

fn parse_units_json(bytes: &[u8]) -> Result<BTreeMap<String, Value>> {
    if bytes.is_empty() {
        return Ok(BTreeMap::new());
    }
    serde_json::from_slice(bytes).context(l10n::t_("Failed to parse units JSON."))
}

fn print_json(entries: &[StagingAreaEntry]) -> Result<()> {
    let list: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| {
            let units = parse_units_json(&e.units_json).unwrap_or_default();
            serde_json::json!({
                "uid": e.uid,
                "name": e.name,
                "unit_count": e.unit_count,
                "units": units,
            })
        })
        .collect();
    let output = serde_json::to_string_pretty(&list)?;
    pager_println!("{output}");
    Ok(())
}

fn print_entry_friendly(entry: &StagingAreaEntry) -> Result<()> {
    pager_println!(
        "  {} {}  {} {}",
        "UID:".bold().cyan(),
        entry.uid.to_string().bold(),
        "Name:".bold().cyan(),
        entry.name.bold(),
    );

    let units = parse_units_json(&entry.units_json)?;
    if units.is_empty() {
        pager_println!(
            "  {} {}",
            "Units:".bold().yellow(),
            l10n::t_("(none)").dimmed()
        );
        pager_println!();
        return Ok(());
    }

    pager_println!(
        "  {} {}:",
        "Units".bold().yellow(),
        format!("({})", units.len()).bold().yellow()
    );

    for (unit_id, unit_ir) in &units {
        pager_println!("    {}", unit_id.bold().yellow());
        print_value(unit_ir, "      ");
    }
    pager_println!();
    Ok(())
}

fn print_value(value: &Value, indent: &str) {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            for (k, v) in &entries {
                match v {
                    Value::Object(_) | Value::Array(_) => {
                        pager_println!("{}{}:", indent, k.bold());
                        print_value(v, &format!("  {indent}"));
                    }
                    Value::String(s) => {
                        pager_println!("{}{}: {}", indent, k.bold(), s);
                    }
                    other => {
                        pager_println!("{}{}: {}", indent, k.bold(), other);
                    }
                }
            }
        }
        Value::Array(arr) => {
            if arr.is_empty() {
                pager_println!("{}{}", indent, l10n::t_("(empty)").dimmed());
                return;
            }
            if arr.len() <= 3 && arr.iter().all(|v| matches!(v, Value::String(_))) {
                let items: Vec<&str> = arr.iter().map(|v| v.as_str().unwrap_or("?")).collect();
                pager_println!("{}{}", indent, items.join(", "));
                return;
            }
            for v in arr {
                pager_print!("{}  - ", indent);
                print_value(v, &format!("  {indent}"));
            }
        }
        Value::Null => {
            pager_println!("{}{}", indent, l10n::t_("(null)").dimmed());
        }
        _ => {
            pager_println!("{indent}{value}");
        }
    }
}

async fn run_list(json: bool, uids: Vec<u32>, regex_strs: Vec<String>) -> Result<()> {
    let admin = StagingAdmin::new();

    let regexes: Vec<Regex> = regex_strs
        .iter()
        .map(|s| {
            Regex::new(s).with_context(|| {
                l10n::fmt(l10n::t_("Invalid regex '{pattern}'."), &[("pattern", s)])
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let has_uid_filter = !uids.is_empty();
    let has_regex_filter = !regexes.is_empty();

    let entries: Vec<StagingAreaEntry> = if has_uid_filter {
        let mut results = Vec::new();
        for uid in &uids {
            let result = admin.query_by_uid(*uid).await?;
            if result.success {
                results.extend(result.entries);
            } else {
                pager_eprintln!(
                    "{}",
                    l10n::fmt(
                        l10n::t_("Warning: UID {uid}: {message}"),
                        &[("uid", &uid.to_string()), ("message", &result.message)]
                    )
                    .yellow()
                );
            }
        }
        if has_regex_filter {
            results
                .into_iter()
                .filter(|e| regexes.iter().any(|r| r.is_match(&e.name)))
                .collect()
        } else {
            results
        }
    } else if has_regex_filter {
        let all = admin.query_all().await?;
        if !all.success {
            anyhow::bail!("{}", all.message);
        }
        all.entries
            .into_iter()
            .filter(|e| regexes.iter().any(|r| r.is_match(&e.name)))
            .collect()
    } else {
        let all = admin.query_all().await?;
        if !all.success {
            anyhow::bail!("{}", all.message);
        }
        all.entries
    };

    if entries.is_empty() {
        pager_println!("{}", l10n::t_("No staging areas found.").dimmed());
        return Ok(());
    }

    if json {
        print_json(&entries)?;
    } else {
        for entry in &entries {
            print_entry_friendly(entry)?;
        }
    }

    Ok(())
}

async fn run_commit(uid: u32) -> Result<()> {
    let admin = StagingAdmin::new();
    let result = admin.commit(uid, "").await?;
    if result.success {
        pager_println!(
            "{}",
            l10n::fmt(
                l10n::t_("Staging areas for UID {uid}: {message}"),
                &[("uid", &uid.to_string()), ("message", &result.message)]
            )
            .green()
        );
    } else {
        pager_eprintln!(
            "{}",
            l10n::fmt(
                l10n::t_("Commit failed: {message}"),
                &[("message", &result.message)]
            )
            .red()
        );
    }
    Ok(())
}

fn build_localized_cli() -> clap::Command {
    
    Args::command()
        .about(l10n::t_("System A — Staging Area Controller"))
        .mut_arg("json", |a| a.help(l10n::t_("Output in JSON format.")))
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
            cmd.about(l10n::t_("List staging areas and their contents."))
                .mut_arg("uid", |a| {
                    a.help(l10n::t_("Filter by UID (can be specified multiple times)."))
                })
                .mut_arg("regex", |a| {
                    a.help(l10n::t_("Regex pattern(s) to match staging area name."))
                })
        })
        .mut_subcommand("commit", |cmd| {
            cmd.about(l10n::t_("Commit staging areas into the active unit set."))
                .mut_arg("uid", |a| {
                    a.help(l10n::t_(
                        "Commit every staging area owned by this UID (default: 0).",
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
    let json = *matches.get_one::<bool>("json").unwrap_or(&false);
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

    let use_pager = !no_pager && !json;
    let _guard: PagerGuard = sysa_pager::open(PagerConfig {
        disable: !use_pager,
    })?;

    match color {
        ColorChoice::Always => colored::control::set_override(true),
        ColorChoice::Never => colored::control::set_override(false),
        ColorChoice::Auto => {
            if use_pager {
                colored::control::set_override(true);
            }
        }
    }

    let result = match matches.subcommand() {
        Some(("list", sub_m)) => {
            let uids: Vec<u32> = sub_m
                .get_many::<u32>("uid")
                .unwrap_or_default()
                .copied()
                .collect();
            let regex_strs: Vec<String> = sub_m
                .get_many::<String>("regex")
                .unwrap_or_default()
                .cloned()
                .collect();
            run_list(json, uids, regex_strs).await
        }
        Some(("commit", sub_m)) => {
            let uid = sub_m.get_one::<u32>("uid").copied().unwrap_or(0);
            run_commit(uid).await
        }
        None => run_list(json, vec![], vec![]).await,
        _ => {
            let mut cmd = build_localized_cli();
            cmd.print_help()?;
            pager_println!();
            Ok(())
        }
    };

    match result {
        Err(e) => match e.downcast_ref::<std::io::Error>() {
            Some(ioe) if ioe.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
            _ => Err(e),
        },
        Ok(()) => Ok(()),
    }
}
