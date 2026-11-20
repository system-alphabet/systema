//! systema-sysf — System F (generic System Finder Worker)
//!
//! The generic finder runner.  Scans the finder search paths
//! (`sysa::paths::finder_search_paths`) for finder executables — every
//! file with the executable bit set, e.g. `systema-sysf.systemd` — and
//! runs the selected ones in order (each stages its discovered units into
//! the shared staging area), then commits the staging area into the active
//! set.  The `--finder` selection may also name a directory: every
//! executable directly inside it is run, sorted by file name.  The
//! `commit` / `query` subcommands manage the staging area explicitly.

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

    #[arg(
        long,
        help = "Run only the selected finders: a file name, a file path, or a directory (default: every finder found)"
    )]
    finder: Option<String>,

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
    sysa::l10n::init(sysa::paths::instance().locale_dir);
    sysa::signals::install_signal_guard();

    let args = {
        use clap::{CommandFactory, FromArgMatches};
        let cmd = Args::command()
            .about(sysa::l10n::t_("System F — System Finder Worker"))
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
            .mut_arg("finder", |a| {
                a.help(sysa::l10n::t_(
                    "Run only the selected finders: a file name, a file path, or a directory (default: every finder found).",
                ))
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
    // Self-managed logging: stderr always, plus the file resolved from
    // --log-file / SYSTEMA_LOG_FILE / the built-in default.
    sysa::logging::init_daemon("systema-sysf", args.log_file.clone(), log_level);

    match args.command {
        Some(Command::Commit) => run_commit(&args.name).await,
        Some(Command::Query) => run_query(&args.name).await,
        Some(Command::DaemonReload) => run_daemon_reload(&args.name, args.finder.as_deref()).await,
        None => run_finders(&args.name, args.finder.as_deref()).await,
    }
}

/// Scan the finder search paths for finder executables.
///
/// `filter` may be:
/// - `None`: every finder in every search path;
/// - a bare file name (`systema-sysf.systemd`): only files with that name,
///   in search-path order;
/// - a file path: that one executable;
/// - a directory path: every executable directly inside it (non-recursive),
///   sorted by file name.
fn discover_finders(filter: Option<&str>) -> Vec<PathBuf> {
    discover_finders_in(&paths::instance().finder_search_paths, filter)
}

fn discover_finders_in(dirs: &[String], filter: Option<&str>) -> Vec<PathBuf> {
    let Some(filter) = filter else {
        return finders_in_search_paths(dirs);
    };
    let direct = Path::new(filter);
    if direct.is_dir() {
        return executables_in_dir(direct);
    }
    if direct.is_file() {
        return is_executable(direct)
            .then(|| direct.to_path_buf())
            .into_iter()
            .collect();
    }
    finders_in_search_paths(dirs)
        .into_iter()
        .filter(|path| path.file_name().and_then(|name| name.to_str()) == Some(filter))
        .collect()
}

/// Every finder executable in the search paths, in search-path order and,
/// within a directory, sorted by file name.
fn finders_in_search_paths(dirs: &[String]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for dir in dirs {
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
        out.extend(names.into_iter().map(|name| PathBuf::from(dir).join(name)));
    }
    out
}

/// Every executable directly inside `dir`, sorted by file name.
/// Subdirectories are not entered.
fn executables_in_dir(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| is_executable(path))
        .collect();
    paths.sort();
    paths
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

/// Run every selected finder executable **in order**, then commit the
/// staging area.
///
/// Each finder is spawned and waited for before the next one starts, so the
/// configured order (search-path order, or file-name order inside a
/// directory) is also the execution order.  Each finder receives `--name`
/// so all of them stage into the same UID-bound staging area.  A failing
/// finder does not stop the others; the commit runs after every finder has
/// exited.
async fn run_finders(name: &str, filter: Option<&str>) -> Result<()> {
    info!("System F running finder executables (name='{name}')");

    let finders = discover_finders(filter);
    if finders.is_empty() {
        if let Some(filter) = filter {
            anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_("Finder '{name}' not found."),
                &[("name", filter)]
            ));
        }
        warn!("No finder executables found in finder search paths");
    }

    let mut failed: Vec<PathBuf> = Vec::new();
    for path in &finders {
        info!("Spawning finder {}", path.display());
        let binary = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "systema-finder".to_string());
        let mut child = tokio::process::Command::new(path)
            .arg("--name")
            .arg(name)
            // Same log directory as ours (see the logging module).
            .arg("--log-file")
            .arg(sysa::logging::child_log_file(&binary))
            .spawn()
            .with_context(|| {
                sysa::l10n::fmt(
                    sysa::l10n::t_("Failed to spawn finder {path}."),
                    &[("path", &path.display().to_string())],
                )
            })?;
        let status = child.wait().await?;
        if status.success() {
            info!("Finder {} finished successfully", path.display());
        } else {
            error!("Finder {} exited with {status}", path.display());
            failed.push(path.clone());
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

    let client = UnitFinder::with_socket(sysa::paths::instance().ipc_socket_path);
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

    let client = UnitFinder::with_socket(sysa::paths::instance().ipc_socket_path);
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
async fn run_daemon_reload(name: &str, finder: Option<&str>) -> Result<()> {
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
    info!(
        "System F daemon-reload listening on {}",
        socket_path_owned.display()
    );

    // The result of the loop is the exit code; clean up the socket on return.
    let result = run_daemon_reload_loop(&listener, name, finder).await;
    let _ = std::fs::remove_file(&socket_path_owned);
    info!("Reload socket cleaned up: {}", socket_path_owned.display());
    result
}

/// Inner loop: accept connections, process triggers, send acks.
async fn run_daemon_reload_loop(
    listener: &tokio::net::UnixListener,
    name: &str,
    finder: Option<&str>,
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

        // Run all selected finders and commit the staging area.
        match run_finders(name, finder).await {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("systema-sysf-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn discover_finders_filters_by_file_name() {
        let dir = temp_dir("filter");
        for name in ["a-finder", "b-finder"] {
            let path = dir.join(name);
            std::fs::write(&path, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let dirs = vec![dir.to_string_lossy().into_owned()];

        assert_eq!(discover_finders_in(&dirs, None).len(), 2);
        assert_eq!(
            discover_finders_in(&dirs, Some("b-finder")),
            vec![dir.join("b-finder")]
        );
        assert!(discover_finders_in(&dirs, Some("missing")).is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discover_finders_visits_search_paths_in_order() {
        let first = temp_dir("order-first");
        let second = temp_dir("order-second");
        for dir in [&first, &second] {
            let path = dir.join("finder");
            std::fs::write(&path, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let dirs = vec![
            first.to_string_lossy().into_owned(),
            second.to_string_lossy().into_owned(),
        ];

        // Within the filter the first search path wins, matching how a
        // finder name shadows a lower-priority copy.
        assert_eq!(
            discover_finders_in(&dirs, Some("finder")),
            vec![first.join("finder"), second.join("finder")]
        );

        let _ = std::fs::remove_dir_all(&first);
        let _ = std::fs::remove_dir_all(&second);
    }

    #[test]
    fn a_directory_selection_runs_executables_in_file_name_order() {
        let dir = temp_dir("dir-selection");
        for name in ["second", "first"] {
            let path = dir.join(name);
            std::fs::write(&path, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // A non-executable file and a subdirectory are not part of the
        // selection, and the walk does not recurse.
        std::fs::write(dir.join("notes.txt"), "not a program").unwrap();
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        let nested = dir.join("nested/third");
        std::fs::write(&nested, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(
            discover_finders_in(&[], Some(dir.to_str().unwrap())),
            vec![dir.join("first"), dir.join("second")]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_path_selects_just_that_finder() {
        let dir = temp_dir("file-selection");
        let path = dir.join("only");
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(
            discover_finders_in(&[], Some(path.to_str().unwrap())),
            vec![path.clone()]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
