//! systema-sysw.systemd — System Wrapper systemd D-Bus bridge
//!
//! Serves the org.freedesktop.systemd1 D-Bus surface on the system bus,
//! backed by System A over the control socket.  Properties, job lists and
//! unit listings are read from a local mirror that is refreshed by
//! control-port lifecycle events; mutations are forwarded to System A as
//! `manager.*` RPCs.

mod bridge;
mod dbus;
mod file_state;
mod install;
#[cfg(unix)]
mod managed_oom;
mod mirror;
mod service_install;

use anyhow::Result;
use clap::Parser;

#[derive(Parser)]
#[command(
    name = "systema-sysw.systemd",
    about = "System Wrapper — systemd D-Bus bridge"
)]
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

    #[arg(long, help = "D-Bus address to use (default: the system bus)")]
    bus_address: Option<String>,

    #[arg(
        long,
        value_enum,
        help = "How to install units for enable/disable/mask/... (auto, always, never)"
    )]
    service_install_fallback: Option<service_install::ServiceInstallFallback>,

    #[arg(long, short = 'v', help = "Print version information and exit")]
    version: bool,

    #[arg(
        long,
        help = "Print full version: build options and compile-time paths"
    )]
    full_version: bool,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    sysa::paths::init();
    sysa::l10n::init(sysa::paths::instance().locale_dir);

    let args = {
        use clap::{CommandFactory, FromArgMatches};
        let cmd = Args::command()
            .about(sysa::l10n::t_("System Wrapper — systemd D-Bus bridge"))
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
            .mut_arg("bus_address", |a| {
                a.help(sysa::l10n::t_(
                    "D-Bus address to use (default: the system bus).",
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
    // Fail fast on a malformed address, before anything connects anywhere
    // (plan 备注 11): the workers must not know which bus domain they are
    // in — the launcher decides it per spawn.
    let bus_address = match args.bus_address {
        Some(raw) => Some(raw.parse::<zbus::Address>().map_err(|e| {
            anyhow::anyhow!(sysa::l10n::fmt(
                sysa::l10n::t_("invalid --bus-address: {err}"),
                &[("err", &e.to_string())]
            ))
        })?),
        None => None,
    };
    let log_level = if args.debug { "debug" } else { &args.log_level };
    // Self-managed logging: stderr always, plus the file resolved from
    // --log-file / SYSTEMA_LOG_FILE / the built-in default.
    sysa::logging::init_daemon("systema-sysw.systemd", args.log_file.clone(), log_level);

    // Create /run/systemd/system if it doesn't exist.  Many systemd units
    // expect this directory to be present; it belongs to the D-Bus surface
    // (System Wrapper).
    let runstatedir = sysa::paths::instance().runstatedir;
    let systemd_system_dir = format!("{runstatedir}/systemd/system");
    if let Err(e) = std::fs::create_dir_all(&systemd_system_dir) {
        tracing::warn!("Failed to create {systemd_system_dir}: {e}");
    }

    #[cfg(unix)]
    if bus_address.is_none() {
        let runstatedir = runstatedir.to_string();
        tokio::spawn(async move {
            if let Err(error) = managed_oom::serve(&runstatedir).await {
                tracing::warn!("ManagedOOM compatibility endpoint failed: {error:#}");
            }
        });
    }

    // Provision the systemd first-boot marker the way real systemd's pid1
    // does.  The *classification* is systema's own (`sysa::firstboot`: the
    // /firstboot override flag, or no machine-id file anywhere); the marker
    // exists only for the systemd ecosystem in the image, which expects it
    // at /run/systemd/first-boot during a first boot.  Mirror both arms of
    // systemd's lifecycle: create it on a first boot, and drop it (e.g. a
    // stale marker on a non-tmpfs /run) once the machine id is committed —
    // first-boot-only conditions must never see it again afterwards.
    let firstboot_marker = format!("{runstatedir}/systemd/first-boot");
    let marker_path = std::path::Path::new(&firstboot_marker);
    if sysa::firstboot::is_first_boot() {
        if !marker_path.exists() {
            if let Err(e) = std::fs::write(marker_path, b"") {
                tracing::warn!("Failed to create {firstboot_marker}: {e}");
            }
        }
    } else if marker_path.exists() {
        if let Err(e) = std::fs::remove_file(marker_path) {
            tracing::warn!("Failed to remove stale {firstboot_marker}: {e}");
        }
    }

    // Reconnect loop: keep serving D-Bus across control-session restarts.
    // A SIGTERM/SIGINT request (from System Init or a console) ends it.
    let install_fallback = resolve_install_fallback(args.service_install_fallback);
    loop {
        let result = tokio::select! {
            biased;
            _sig = sysa::signals::shutdown_signal() => {
                break;
            }
            result = bridge::run(bus_address.clone(), install_fallback) => result,
        };
        match result {
            Ok(()) => {
                tracing::info!("Bridge exited cleanly; reconnecting");
            }
            Err(e) => {
                tracing::warn!("Bridge session ended: {e:#}; reconnecting");
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    Ok(())
}

/// Resolve the effective fallback mode: command line first, then
/// `SYSTEMA_FORCE_FALLBACK_SERVICE_INSTALL=1`, then `auto`.
fn resolve_install_fallback(
    cli: Option<service_install::ServiceInstallFallback>,
) -> service_install::ServiceInstallFallback {
    use service_install::ServiceInstallFallback;
    if let Some(mode) = cli {
        return mode;
    }
    match std::env::var("SYSTEMA_FORCE_FALLBACK_SERVICE_INSTALL") {
        Ok(value) if is_truthy(&value) => ServiceInstallFallback::Always,
        _ => ServiceInstallFallback::Auto,
    }
}

fn is_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "yes" | "true" | "on"
    )
}
