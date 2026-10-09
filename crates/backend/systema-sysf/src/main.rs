//! systema-sysf — System F (generic System Finder Worker)
//!
//! The generic finder runner.  Scans the finder search paths
//! (`sysa::paths::finder_search_paths`) for finder executables — every
//! file with the executable bit set, e.g. `systema-sysf.systemd` — runs
//! all of them concurrently (each stages its discovered units into the
//! shared staging area), then commits the staging area into the active
//! set.  The `commit` / `query` subcommands manage the staging area
//! explicitly.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use sysa::finder::UnitFinder;
use sysa::paths;
use systema_sysf::ir::UnitIR;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{error, info, warn};

#[derive(Parser)]
#[command(name = "systema-sysf", about = "System F — System Finder Worker")]
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

    #[arg(long, short = 'v', help = "Print version information and exit")]
    version: bool,

    #[arg(
        long,
        help = "Print full version: build options and compile-time paths"
    )]
    full_version: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Commit the UID-bound staging area into the active set
    Commit,
    /// Query the current UID-bound staging area contents
    Query,
    /// Listen for reload notifications from System A and re-run finders
    DaemonReload,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    sysa::paths::init();
    sysa::l10n::init();

    let args = {
        use clap::{CommandFactory, FromArgMatches};
        let cmd = Args::command()
            .about(sysa::l10n::t_("System F — System Finder Worker"))
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
            })
            .mut_subcommand("commit", |cmd| {
                cmd.about(sysa::l10n::t_("Commit the UID-bound staging area."))
            })
            .mut_subcommand("query", |cmd| {
                cmd.about(sysa::l10n::t_("Query the UID-bound staging area."))
            })
            .mut_subcommand("daemon-reload", |cmd| {
                cmd.about(sysa::l10n::t_(
                    "Listen for reload notifications from System A.",
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
    // Self-managed logging: <log-dir>/<name>.log, or stderr for "-".
    sysa::logging::init(sysa::paths::instance().log_dir, "systema-sysf", log_level);

    match args.command {
        Some(Command::Commit) => run_commit(&args.name).await,
        Some(Command::Query) => run_query(&args.name).await,
        Some(Command::DaemonReload) => run_daemon_reload(&args.name).await,
        None => run_finders(&args.name).await,
    }
}

/// Scan the finder search paths for finder executables.
///
/// Every regular file with the executable bit set is considered a finder
/// executable.  Directories are visited in search-path order; within a
/// directory, entries are sorted for determinism.
fn discover_finders() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for dir in &paths::instance().finder_search_paths {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let path = PathBuf::from(dir).join(&name);
                is_executable(&path).then_some(name)
            })
            .collect();
        names.sort();
        for name in names {
            out.push(PathBuf::from(dir).join(name));
        }
    }
    out
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Run every finder executable concurrently, then commit the staging area.
///
/// Each finder receives `--name` so all of them stage into the same
/// UID-bound staging area.  All finders are spawned before any is awaited;
/// the commit runs only after every finder has exited.
async fn run_finders(name: &str) -> Result<()> {
    info!("System F running finder executables (name='{name}')");

    let finders = discover_finders();
    if finders.is_empty() {
        warn!("No finder executables found in finder search paths");
    }

    let mut children: Vec<(PathBuf, tokio::process::Child)> = Vec::new();
    for path in &finders {
        info!("Spawning finder {}", path.display());
        let child = tokio::process::Command::new(path)
            .arg("--name")
            .arg(name)
            .spawn()
            .with_context(|| {
                sysa::l10n::fmt(
                    sysa::l10n::t_("Failed to spawn finder {path}."),
                    &[("path", &path.display().to_string())],
                )
            })?;
        children.push((path.clone(), child));
    }

    let mut failed: Vec<PathBuf> = Vec::new();
    for (path, mut child) in children {
        let status = child.wait().await?;
        if status.success() {
            info!("Finder {} finished successfully", path.display());
        } else {
            error!("Finder {} exited with {status}", path.display());
            failed.push(path);
        }
    }

    run_commit(name).await?;

    if !failed.is_empty() {
        let names: Vec<String> = failed.iter().map(|p| p.display().to_string()).collect();
        anyhow::bail!(sysa::l10n::fmt(
            sysa::l10n::t_("{count} finder(s) failed: {names}."),
            &[
                ("count", &failed.len().to_string()),
                ("names", &names.join(", ")),
            ]
        ));
    }

    info!("System F finder run complete");
    Ok(())
}

async fn run_commit(name: &str) -> Result<()> {
    info!("System F committing staging area (name='{name}')");

    let client = UnitFinder::new();
    let ack = client.commit_units(name).await?;
    if ack.success {
        info!("Commit successful: {} units committed", ack.unit_count);
    } else {
        error!("Commit failed: {}", ack.message);
        anyhow::bail!(sysa::l10n::fmt(
            sysa::l10n::t_("Commit failed: {message}."),
            &[("message", &ack.message)]
        ));
    }

    info!("System F commit complete");
    Ok(())
}

async fn run_query(name: &str) -> Result<()> {
    info!("System F querying staging area (name='{name}')");

    let client = UnitFinder::new();
    let result = client.query_staging(name).await?;
    if result.success {
        info!("Staging area contains {} units", result.unit_count);
        let units: HashMap<String, UnitIR> = serde_json::from_slice(&result.units_json)?;
        for id in units.keys() {
            info!("  {id}");
        }
    } else {
        error!("Query failed: {}", result.message);
        anyhow::bail!(sysa::l10n::fmt(
            sysa::l10n::t_("Query failed: {message}."),
            &[("message", &result.message)]
        ));
    }

    Ok(())
}

/// Daemon-reload listener: bind a Unix socket and wait for System A to
/// trigger a re-scan.
///
/// # Unix-socket protocol
///
/// ```text
/// System A connects → writes [1u8] → System F runs finders → System F writes [1u8] ack
/// ```
///
/// The socket file lives in `/run` (tmpfs) and is removed when this
/// function returns (on SIGTERM / signal-driven shutdown from SysI).
async fn run_daemon_reload(name: &str) -> Result<()> {
    let socket_path = paths::instance().reload_socket;
    let socket_path_owned = std::path::PathBuf::from(socket_path);

    // Ensure the parent directory exists.
    if let Some(parent) = socket_path_owned.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            sysa::l10n::fmt(
                sysa::l10n::t_("Failed to create socket directory {path}."),
                &[("path", &parent.display().to_string())],
            )
        })?;
    }

    // Remove any stale socket file from a previous run.
    let _ = std::fs::remove_file(&socket_path_owned);

    let listener = tokio::net::UnixListener::bind(&socket_path_owned).with_context(|| {
        sysa::l10n::fmt(
            sysa::l10n::t_("Failed to bind reload socket {path}."),
            &[("path", &socket_path_owned.display().to_string())],
        )
    })?;
    info!("System F daemon-reload listening on {}", socket_path_owned.display());

    // The result of the loop is the exit code; clean up the socket on return.
    let result = run_daemon_reload_loop(&listener, name).await;
    let _ = std::fs::remove_file(&socket_path_owned);
    info!("Reload socket cleaned up: {}", socket_path_owned.display());
    result
}

/// Inner loop: accept connections, process triggers, send acks.
async fn run_daemon_reload_loop(
    listener: &tokio::net::UnixListener,
    name: &str,
) -> Result<()> {
    loop {
        let (mut stream, addr) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                warn!("Reload socket accept error: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        };
        info!("System A connected from {:?}", addr);

        let mut buf = [0u8; 1];
        match stream.read_exact(&mut buf).await {
            Ok(_n) if buf[0] == 1 => {
                info!("Reload trigger received; running finders (name='{name}')");
            }
            Ok(_n) => {
                warn!("Unexpected trigger byte {:?}; ignoring", buf[0]);
                continue;
            }
            Err(e) => {
                warn!("Failed to read trigger from System A: {e}");
                continue;
            }
        }

        // Run all finders and commit the staging area.
        match run_finders(name).await {
            Ok(()) => {
                // Send ack byte back to System A.
                if let Err(e) = stream.write_all(&[1u8]).await {
                    warn!("Failed to send ack to System A: {e}");
                } else {
                    info!("Reload complete; ack sent");
                }
            }
            Err(e) => {
                error!("Finder reload failed: {e}");
                // Send error ack (value 0).
                let _ = stream.write_all(&[0u8]).await;
            }
        }
    }
}