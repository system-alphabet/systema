//! systema-sysi — SysAInit
//!
//! The init process of System Alphabet.  It spawns System A first, waits
//! for it to report ready on the notify channel, then starts the System
//! Workers **serially** — each worker is only spawned after the previous
//! one reported `WORKER_READY` — and supervises them until they exit.
//! It can run as PID 1 (containers, bare metal without another init) or as
//! a plain process inside a container.
//!
//! Binaries are searched for in an explicit `--bin-dir` (when given), the
//! directory of the SysAInit executable itself, then the canonical systema
//! install directories, then `PATH`.  By default a missing executable is
//! fatal; `--no-strict` downgrades that to an ERROR log and skips the
//! process.
//!
//! Deliberately out of scope: restarts, hostname, dbus-daemon (the system
//! bus must be provided externally), journaling.  API filesystem mounts
//! are a PID 1 duty: SysAInit mounts the cgroup v2 hierarchy, `/dev/shm`
//! and `/dev/pts` itself (see [`mount_setup`]) like systemd's
//! `mount_setup()` does, and only when it has the privileges to mount.

mod mount_setup;
mod power;
mod supervise;
mod workers;

use anyhow::{bail, Result};
use clap::Parser;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tracing::{error, info, warn};

#[derive(Parser)]
#[command(name = "systema-sysi", about = "SysAInit — System Alphabet init")]
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
        value_delimiter = ',',
        help = "Workers to skip (short names or full binary names, e.g. sysd,sysc)"
    )]
    skip_workers: Vec<String>,

    #[arg(
        long,
        help = "Search for the systema-* binaries only in this directory (default: the executable directory, then PATH)"
    )]
    bin_dir: Option<PathBuf>,

    #[arg(
        long,
        help = "Do not abort when an executable is missing; log an ERROR and skip it"
    )]
    no_strict: bool,

    #[arg(
        long,
        default_value_t = 10,
        help = "Grace period in seconds before SIGKILL during shutdown"
    )]
    shutdown_timeout: u64,

    #[arg(
        long,
        default_value_t = 30,
        help = "Time to wait for System A / a worker to report ready before aborting"
    )]
    ready_timeout: u64,

    #[arg(
        long,
        default_value = "auto",
        help = "Power control policy: auto (only when PID 1), always, never"
    )]
    powerctl: power::PowerCtl,

    #[arg(
        long,
        help = "Directory for per-process log files (systema-sysa.log, systema-syss.log, ...)"
    )]
    log_dir: PathBuf,

    #[arg(
        long,
        allow_hyphen_values = true,
        help = "Extra flags for every worker; --<name>-flags / SYSTEMA_SYS*_FLAGS take precedence"
    )]
    worker_flags: Option<String>,

    #[arg(long, allow_hyphen_values = true, help = "Extra flags for System A only")]
    sysa_flags: Option<String>,
    #[arg(long, allow_hyphen_values = true, help = "Extra flags for System S only")]
    syss_flags: Option<String>,
    #[arg(long, allow_hyphen_values = true, help = "Extra flags for System E only")]
    syse_flags: Option<String>,
    #[arg(long, allow_hyphen_values = true, help = "Extra flags for System T only")]
    syst_flags: Option<String>,
    #[arg(long, allow_hyphen_values = true, help = "Extra flags for System C only")]
    sysc_flags: Option<String>,
    #[arg(long, allow_hyphen_values = true, help = "Extra flags for System K only")]
    sysk_flags: Option<String>,
    #[arg(long, allow_hyphen_values = true, help = "Extra flags for System N only")]
    sysn_flags: Option<String>,
    #[arg(long, allow_hyphen_values = true, help = "Extra flags for System D only")]
    sysd_flags: Option<String>,
    #[arg(long, allow_hyphen_values = true, help = "Extra flags for System R only")]
    sysr_flags: Option<String>,
    #[arg(long, allow_hyphen_values = true, help = "Extra flags for System M only")]
    sysm_flags: Option<String>,

    #[arg(long, short = 'v', help = "Print version information and exit")]
    version: bool,

    #[arg(
        long,
        help = "Print full version: build options and compile-time paths"
    )]
    full_version: bool,
}

/// Canonical short names of every supervised worker (must mirror
/// [`workers::default_workers`] + the finder chain).
const WORKER_SHORT_NAMES: &[&str] = &[
    "sysa", "syss", "syse", "syst", "sysc", "sysk", "sysn", "sysd", "sysr", "sysm",
];

impl Args {
    fn specific_flags(&self, name: &str) -> Option<&String> {
        Some(match name {
            "sysa" => self.sysa_flags.as_ref()?,
            "syss" => self.syss_flags.as_ref()?,
            "syse" => self.syse_flags.as_ref()?,
            "syst" => self.syst_flags.as_ref()?,
            "sysc" => self.sysc_flags.as_ref()?,
            "sysk" => self.sysk_flags.as_ref()?,
            "sysn" => self.sysn_flags.as_ref()?,
            "sysd" => self.sysd_flags.as_ref()?,
            "sysr" => self.sysr_flags.as_ref()?,
            "sysm" => self.sysm_flags.as_ref()?,
            _ => return None,
        })
    }
}

/// Per-worker environment override (`SYSTEMA_SYSA_FLAGS`, ...);
/// `"workers"` maps to the global `SYSTEMA_WORKERS_FLAGS`.
fn env_flags(name: &str) -> Option<String> {
    std::env::var(format!("SYSTEMA_{}_FLAGS", name.to_uppercase()))
        .ok()
        .filter(|s| !s.trim().is_empty())
}

/// Resolve the effective extra flags for one worker.  Precedence:
/// `--<name>-flags` > `SYSTEMA_SYS<NAME>_FLAGS` > `--worker-flags` >
/// `SYSTEMA_WORKERS_FLAGS`.
fn resolve_flags(args: &Args, name: &str) -> Vec<String> {
    args.specific_flags(name)
        .cloned()
        .or_else(|| env_flags(name))
        .or_else(|| args.worker_flags.clone())
        .or_else(|| env_flags("workers"))
        .map(|raw| split_flags(&raw))
        .unwrap_or_default()
}

/// Split a shell-like flag string on whitespace, honouring single/double
/// quotes and backslash escapes so values may contain spaces.
fn split_flags(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut has_token = false;
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                has_token = true;
                for c2 in chars.by_ref() {
                    if c2 == '\'' {
                        break;
                    }
                    cur.push(c2);
                }
            }
            '"' => {
                has_token = true;
                while let Some(c2) = chars.next() {
                    match c2 {
                        '"' => break,
                        '\\' => match chars.peek() {
                            Some(&n) if matches!(n, '"' | '\\' | '$' | '`') => {
                                cur.push(n);
                                chars.next();
                            }
                            _ => cur.push('\\'),
                        },
                        _ => cur.push(c2),
                    }
                }
            }
            '\\' => {
                has_token = true;
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            c if c.is_whitespace() => {
                if has_token {
                    out.push(std::mem::take(&mut cur));
                    has_token = false;
                }
            }
            _ => {
                has_token = true;
                cur.push(c);
            }
        }
    }
    if has_token {
        out.push(cur);
    }
    out
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    sysa::paths::init();
    sysa::l10n::init();

    let args = {
        use clap::{CommandFactory, FromArgMatches};
        let cmd = Args::command()
            .about(sysa::l10n::t_("SysAInit — System Alphabet init"))
            // Unknown arguments and options are silently ignored; known
            // ones are still parsed normally.
            .ignore_errors(true)
            .mut_arg("debug", |a| {
                a.help(sysa::l10n::t_("Enable debug-level logging."))
            })
            .mut_arg("log_level", |a| {
                a.help(sysa::l10n::t_(
                    "Log level (trace, debug, info, warn, error).",
                ))
            })
            .mut_arg("skip_workers", |a| {
                a.help(sysa::l10n::t_(
                    "Workers to skip (short names or full binary names).",
                ))
            })
            .mut_arg("bin_dir", |a| {
                a.help(sysa::l10n::t_(
                    "Search for the systema-* binaries only in this directory.",
                ))
            })
            .mut_arg("no_strict", |a| {
                a.help(sysa::l10n::t_(
                    "Do not abort when an executable is missing; log an ERROR and skip it.",
                ))
            })
            .mut_arg("shutdown_timeout", |a| {
                a.help(sysa::l10n::t_(
                    "Grace period in seconds before SIGKILL during shutdown.",
                ))
            })
            .mut_arg("ready_timeout", |a| {
                a.help(sysa::l10n::t_(
                    "Time to wait for System A / a worker to report ready before aborting.",
                ))
            })
            .mut_arg("log_dir", |a| {
                a.help(sysa::l10n::t_(
                    "Directory for per-process log files (systema-sysa.log, ...).",
                ))
                .default_value(sysa::paths::instance().log_dir)
            })
            .mut_arg("powerctl", |a| {
                a.help(sysa::l10n::t_(
                    "Power control policy: auto (only when PID 1), always, never.",
                ))
            })
            .mut_arg("worker_flags", |a| {
                a.help(sysa::l10n::t_(
                    "Extra flags for every worker; --<name>-flags / SYSTEMA_SYS*_FLAGS take precedence.",
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
        let cmd = WORKER_SHORT_NAMES.iter().fold(cmd, |cmd, name| {
            cmd.mut_arg(format!("{name}_flags"), |a| {
                a.help(sysa::l10n::t_(
                    "Extra flags for this worker only (overrides --worker-flags).",
                ))
            })
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
    // SysAInit's own log: <log-dir>/systema-sysi.log, stderr for "-".
    // Workers resolve SYSTEMA_LOG_DIR the same way (see logging module).
    let log_dir_str = args.log_dir.to_string_lossy().into_owned();
    sysa::logging::init(&log_dir_str, "systema-sysi", log_level);
    info!("SysAInit starting (PID {})", std::process::id());

    // Like systemd's `mount_setup()`, SysAInit mounts the API filesystems
    // itself before anything else starts.  Skipped without mount
    // privileges (rootless), and never fatal: resource control degrades
    // to the no-op controller when cgroup2 is unavailable, and Wayland
    // compositors fall back when /dev/shm is missing (they abort, but the
    // boot continues).  Runs after logging is set up so failures land in
    // the log file.
    if let Err(e) = mount_setup::mount_cgroup2() {
        warn!("cgroup2 mount failed; resource control will degrade: {e:#}");
    }
    if let Err(e) = mount_setup::mount_dev_shm() {
        warn!("/dev/shm mount failed; POSIX shared memory unavailable: {e:#}");
    }
    if let Err(e) = mount_setup::mount_dev_pts() {
        warn!("/dev/pts mount failed; pseudo-terminals unavailable: {e:#}");
    }

    if std::process::id() == 1 {
        info!("SysAInit running as PID 1 (reaping orphaned processes)");
    } else {
        info!("SysAInit running as a container child process");
    }

    let power_ctl = args.powerctl;
    info!("Power control policy: --powerctl={power_ctl} (pid {})", std::process::id());

    let set = workers::build_worker_set(&args.skip_workers)?;
    let summary = set.iter().map(|s| s.name).collect::<Vec<_>>().join(", ");
    info!(
        "Supervised processes ({count}): {summary}",
        count = set.len()
    );

    let (resolved, missing) = workers::resolve_set(&set, args.bin_dir.as_deref());
    for spec in &missing {
        error!(
            "Missing executable for '{name}' ({binary}): not found in --bin-dir, the SysAInit executable directory, or PATH",
            name = spec.name,
            binary = spec.binary
        );
    }
    if !missing.is_empty() {
        if args.no_strict {
            error!(
                "Continuing without {count} missing executable(s) (--no-strict)",
                count = missing.len()
            );
        } else {
            bail!(sysa::l10n::fmt(sysa::l10n::t_("{count} required executable(s) missing; aborting (use --no-strict to continue)"), &[("count", &(missing.len()).to_string())]));
        }
    }

    // Per-worker extra flags, resolved once (arg > env precedence).
    let extra_flags: HashMap<&'static str, Vec<String>> = WORKER_SHORT_NAMES
        .iter()
        .map(|name| (*name, resolve_flags(&args, name)))
        .filter(|(_, flags)| !flags.is_empty())
        .collect();
    for (name, flags) in &extra_flags {
        info!("Extra flags for {name}: {}", flags.join(" "));
    }

    let code = supervise::run(
        &resolved,
        args.debug,
        &args.log_level,
        Duration::from_secs(args.shutdown_timeout),
        Duration::from_secs(args.ready_timeout),
        &args.log_dir,
        &extra_flags,
        args.powerctl,
    )
    .await?;
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    /// Serializes tests that read or write `SYSTEMA_*_FLAGS` env vars: the
    /// process-global environment is shared by all running tests, so a
    /// writer test (set_var/remove_var) would race with a reader test unless
    /// both take this lock.
    fn env_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    #[test]
    fn split_flags_whitespace() {
        assert_eq!(split_flags("--foo --bar=1"), vec!["--foo", "--bar=1"]);
        assert_eq!(split_flags("  a   b  "), vec!["a", "b"]);
        assert_eq!(split_flags(""), Vec::<String>::new());
        assert_eq!(split_flags("   "), Vec::<String>::new());
    }

    #[test]
    fn split_flags_quotes_and_escapes() {
        assert_eq!(split_flags("--x 'a b'"), vec!["--x", "a b"]);
        assert_eq!(split_flags("\"a\\\"b\" c"), vec!["a\"b", "c"]);
        assert_eq!(split_flags(r"a\ b"), vec!["a b"]);
        // Empty quoted argument is preserved.
        assert_eq!(split_flags("'' x"), vec!["", "x"]);
    }

    #[test]
    fn resolve_flags_precedence_specific_over_global() {
        let _guard = env_lock().lock().unwrap();
        let args = Args::parse_from([
            "systema-sysi",
            "--log-dir",
            "-",
            "--worker-flags",
            "--global",
            "--syss-flags",
            "--specific",
        ]);
        assert_eq!(resolve_flags(&args, "syss"), vec!["--specific"]);
        assert_eq!(resolve_flags(&args, "sysk"), vec!["--global"]);
    }

    #[test]
    fn resolve_flags_env_fallbacks() {
        let _guard = env_lock().lock().unwrap();
        std::env::set_var("SYSTEMA_SYSA_FLAGS", "--env-specific");
        std::env::set_var("SYSTEMA_WORKERS_FLAGS", "--env-global");
        let args = Args::parse_from(["systema-sysi", "--log-dir", "-"]);
        // Specific env beats global env; unrelated workers get the global one.
        assert_eq!(resolve_flags(&args, "sysa"), vec!["--env-specific"]);
        assert_eq!(resolve_flags(&args, "syst"), vec!["--env-global"]);
        std::env::remove_var("SYSTEMA_SYSA_FLAGS");
        std::env::remove_var("SYSTEMA_WORKERS_FLAGS");
    }

    #[test]
    fn resolve_flags_arg_beats_env() {
        let _guard = env_lock().lock().unwrap();
        std::env::set_var("SYSTEMA_SYSK_FLAGS", "--from-env");
        let args = Args::parse_from([
            "systema-sysi",
            "--log-dir",
            "-",
            "--sysk-flags",
            "--from-arg",
        ]);
        assert_eq!(resolve_flags(&args, "sysk"), vec!["--from-arg"]);
        std::env::remove_var("SYSTEMA_SYSK_FLAGS");
    }

    #[test]
    fn resolve_flags_none_by_default() {
        let _guard = env_lock().lock().unwrap();
        let args = Args::parse_from(["systema-sysi", "--log-dir", "-"]);
        assert!(resolve_flags(&args, "sysd").is_empty());
    }
}
