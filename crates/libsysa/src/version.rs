//! `--version` / `--full-version` output shared by every System Alphabet
//! binary: the program name and version in color, plus the full
//! compile-time build record for `--full-version` (target, profile,
//! toolchain, features and the baked-in path defaults).

use colored::Colorize;

/// Version shared by every workspace crate (`workspace.package.version`).
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Compile-time build record, emitted by build.rs via `cargo:rustc-env`.
const TARGET: &str = env!("SYSTEMA_BUILD_TARGET");
const PROFILE: &str = env!("SYSTEMA_BUILD_PROFILE");
const OPT_LEVEL: &str = env!("SYSTEMA_BUILD_OPT_LEVEL");
const DEBUG_INFO: &str = env!("SYSTEMA_BUILD_DEBUG");
const RUSTC: &str = env!("SYSTEMA_BUILD_RUSTC");
const FEATURES: &str = env!("SYSTEMA_BUILD_FEATURES");

/// Program name as invoked: the basename of `argv[0]`, so flavor
/// filenames (`systema-sysm.unix`, `systema-sysf.systemd`, ...) are
/// shown exactly as they are on disk.  Falls back to the current
/// executable, then to a generic name.
fn program_name() -> String {
    std::env::args()
        .next()
        .and_then(|argv0| {
            std::path::Path::new(&argv0)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .or_else(|| {
            std::env::current_exe().ok().and_then(|exe| {
                exe.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
        })
        .unwrap_or_else(|| "systema".to_string())
}

/// `--version`: one line — program name in cyan, version in green.
/// Colors follow the `colored` crate's usual rules (disabled when
/// stdout is not a terminal, or when `NO_COLOR` is set).
pub fn print_version() {
    println!(
        "{} {}",
        program_name().bold().cyan(),
        VERSION.bold().green()
    );
}

/// `--full-version`: the version line plus the compile-time build
/// record — target triple, profile, toolchain, features and the path
/// defaults baked in at build time.
pub fn print_full_version() {
    print_version();
    println!();
    let label = |text: &str| format!("{text:<10}").dimmed();
    println!("  {} {}", label("target:"), TARGET);
    println!(
        "  {} {}",
        label("profile:"),
        format!("{PROFILE} (opt-level={OPT_LEVEL}, debug={DEBUG_INFO})")
    );
    println!("  {} {}", label("features:"), FEATURES);
    println!("  {} {}", label("rustc:"), RUSTC);
    println!();
    println!(
        "  {}",
        "compile-time path defaults (SYSTEMA_* / SYSTEMD_* env overrides apply at runtime):"
            .dimmed()
    );
    let defaults = crate::paths::builtin_defaults();
    let width = defaults
        .iter()
        .map(|(name, _)| name.len())
        .max()
        .unwrap_or(0);
    for (name, value) in defaults {
        println!("    {} {}", format!("{name:<width$}").blue().bold(), value);
    }
}
