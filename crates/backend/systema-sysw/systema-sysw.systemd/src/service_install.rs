//! Dispatch for the `systemctl enable/disable/...` D-Bus methods.
//!
//! The preferred path runs the real `systemctl` in its offline, client-side
//! mode (`SYSTEMD_OFFLINE=1`, `SYSTEMCTL_INSTALL_CLIENT_SIDE=1`, plus
//! `--no-reload` so it never talks back to this manager).  When `systemctl`
//! is unavailable — or fails, or the fallback is forced — the built-in
//! implementation in [`crate::install`] performs the same filesystem work.

use std::path::PathBuf;
use std::process::Output;

use tracing::warn;

use crate::install::{self, ChangeTuple, LookupPaths};

/// `--service-install-fallback=` / `SYSTEMA_FORCE_FALLBACK_SERVICE_INSTALL`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum ServiceInstallFallback {
    /// Use `systemctl` when present; fall back when it is missing or fails.
    Auto,
    /// Always use the built-in implementation.
    Always,
    /// Always use `systemctl`; report its failure instead of falling back.
    Never,
}

/// The D-Bus install verb to execute.
#[derive(Clone, Debug)]
pub enum Verb {
    Enable,
    /// `DisableUnitFiles*`; `install_info` selects the variants that return
    /// `carries_install_info`.
    Disable {
        install_info: bool,
    },
    Reenable,
    Link,
    Mask,
    Unmask,
    Revert,
    /// `PresetUnitFiles(WithMode)`; `None` means the default (full) mode.
    Preset(Option<String>),
    PresetAll(String),
    SetDefault,
    GetDefault,
}

impl Verb {
    fn word(&self) -> &'static str {
        match self {
            Self::Enable => "enable",
            Self::Disable { .. } => "disable",
            Self::Reenable => "reenable",
            Self::Link => "link",
            Self::Mask => "mask",
            Self::Unmask => "unmask",
            Self::Revert => "revert",
            Self::Preset(_) => "preset",
            Self::PresetAll(_) => "preset-all",
            Self::SetDefault => "set-default",
            Self::GetDefault => "get-default",
        }
    }

    fn returns_install_info(&self) -> bool {
        matches!(
            self,
            Self::Enable | Self::Disable { install_info: true } | Self::Reenable | Self::Preset(_)
        )
    }

    fn accepts_scope_flags(&self) -> bool {
        !matches!(self, Self::Revert | Self::SetDefault | Self::GetDefault)
    }

    fn preset_mode(&self) -> Option<&str> {
        match self {
            Self::Preset(Some(mode)) | Self::PresetAll(mode) => Some(mode),
            _ => None,
        }
    }
}

/// Result of an install verb, shaped like the D-Bus reply.
#[derive(Debug, Default)]
pub struct Outcome {
    pub carries_install_info: Option<bool>,
    pub changes: Vec<ChangeTuple>,
    pub default_target: Option<String>,
}

/// Execute `verb` according to `mode`.
pub async fn run(
    mode: ServiceInstallFallback,
    verb: Verb,
    names: Vec<String>,
    runtime: bool,
    force: bool,
) -> Result<Outcome, String> {
    if let Some(mode_name) = verb.preset_mode() {
        if install::preset::PresetMode::from_name(mode_name).is_none() {
            return Err(sysa::l10n::fmt(
                sysa::l10n::t_("Invalid preset mode {mode}."),
                &[("mode", mode_name)],
            ));
        }
    }

    match mode {
        ServiceInstallFallback::Always => run_fallback(verb, names, runtime, force).await,
        ServiceInstallFallback::Never => match find_systemctl() {
            Some(systemctl) => run_systemctl(&systemctl, &verb, &names, runtime, force)
                .await
                .map_err(|e| {
                    sysa::l10n::fmt(
                        sysa::l10n::t_("Failed to install unit files: {e}."),
                        &[("e", &e)],
                    )
                }),
            None => Err(sysa::l10n::t_("systemctl was not found.")),
        },
        ServiceInstallFallback::Auto => match find_systemctl() {
            Some(systemctl) => match run_systemctl(&systemctl, &verb, &names, runtime, force).await
            {
                Ok(outcome) => Ok(outcome),
                Err(message) => {
                    warn!(
                        "systemctl {} failed: {}; using the built-in install fallback",
                        verb.word(),
                        message
                    );
                    run_fallback(verb, names, runtime, force).await
                }
            },
            None => run_fallback(verb, names, runtime, force).await,
        },
    }
}

fn find_systemctl() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("systemctl");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

async fn run_systemctl(
    systemctl: &PathBuf,
    verb: &Verb,
    names: &[String],
    runtime: bool,
    force: bool,
) -> Result<Outcome, String> {
    let mut command = tokio::process::Command::new(systemctl);
    command
        .env("SYSTEMD_OFFLINE", "1")
        .env("SYSTEMCTL_INSTALL_CLIENT_SIDE", "1")
        // The change list is read back from the client-side run below, so pin
        // the language and log level that make its wording predictable.
        .env("LC_ALL", "C")
        .env("SYSTEMD_LOG_LEVEL", "info")
        .arg("--no-reload")
        .arg("--system");

    if verb.accepts_scope_flags() {
        if runtime {
            command.arg("--runtime");
        }
        if force {
            command.arg("--force");
        }
        if let Some(mode) = verb.preset_mode() {
            if !mode.is_empty() && mode != "full" {
                command.arg(format!("--preset-mode={mode}"));
            }
        }
    }

    command.arg(verb.word());
    match verb {
        Verb::SetDefault | Verb::GetDefault => {
            if let Some(name) = names.first() {
                command.arg(name);
            }
        }
        _ => {
            command.args(names);
        }
    }

    let output = command.output().await.map_err(|e| {
        sysa::l10n::fmt(
            sysa::l10n::t_("Failed to execute {path}: {e}."),
            &[
                ("path", &systemctl.to_string_lossy()),
                ("e", &e.to_string()),
            ],
        )
    })?;
    if !output.status.success() {
        return Err(systemctl_failure(&output));
    }

    let mut outcome = Outcome::default();
    if matches!(verb, Verb::GetDefault) {
        outcome.default_target = Some(
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .next()
                .unwrap_or("default.target")
                .trim()
                .to_string(),
        );
    } else {
        // The client-side run performs the filesystem work itself; relay the
        // step list it printed so the calling `systemctl` dumps it just like
        // it would for a manager that answered the call directly.
        outcome.changes = parse_systemctl_changes(&output);
        if verb.returns_install_info() {
            let names = names.to_vec();
            outcome.carries_install_info = Some(
                tokio::task::spawn_blocking(move || probe_install_info(&names))
                    .await
                    .unwrap_or(true),
            );
        }
    }
    Ok(outcome)
}

/// Parse the step list `systemctl` prints in client-side mode into the
/// `(change type, path, source)` tuples the D-Bus API returns.
///
/// systemd's manager answers the same call with an `a(sss)` array and the
/// calling `systemctl` dumps it; the subprocess output is the equivalent
/// information when the real `systemctl` did the work itself.
fn parse_systemctl_changes(output: &Output) -> Vec<ChangeTuple> {
    parse_change_text(
        &String::from_utf8_lossy(&output.stderr),
        &String::from_utf8_lossy(&output.stdout),
    )
}

/// Parse the step lines from the child's stderr (where systemctl logs them)
/// and then its stdout, preserving the order within each stream.
fn parse_change_text(stderr: &str, stdout: &str) -> Vec<ChangeTuple> {
    let mut changes = Vec::new();
    for text in [stderr, stdout] {
        for line in text.lines() {
            if let Some(change) = parse_change_line(line) {
                changes.push(change);
            }
        }
    }
    changes
}

fn parse_change_line(line: &str) -> Option<ChangeTuple> {
    let line = line.trim();

    if let Some(rest) = line.strip_prefix("Created symlink ") {
        let (path, rest) = quoted(rest)?;
        let rest = rest.trim_start();
        let rest = rest
            .strip_prefix("->")
            .or_else(|| rest.strip_prefix('\u{2192}'))?
            .trim_start();
        let (source, _) = quoted(rest)?;
        return Some(("symlink".to_string(), path, source));
    }
    if let Some(rest) = line.strip_prefix("Removed ") {
        let (path, _) = quoted(rest)?;
        return Some(("unlink".to_string(), path, String::new()));
    }
    if let Some(rest) = line.strip_prefix("Masked dependency ") {
        let (path, _) = quoted(rest)?;
        return Some(("mask-dependency".to_string(), path, String::new()));
    }
    if let Some(rest) = line.strip_prefix("Unmasked dependency ") {
        let (path, _) = quoted(rest)?;
        return Some(("unmask-dependency".to_string(), path, String::new()));
    }
    if let Some(name) = line
        .strip_prefix("Failed to enable auxiliary unit ")
        .and_then(|rest| rest.strip_suffix(", ignoring."))
    {
        return Some((
            "auxiliary unit failed".to_string(),
            name.to_string(),
            String::new(),
        ));
    }

    let rest = line.strip_prefix("Unit ")?;
    if let Some(name) = rest.strip_suffix(" is masked, ignoring.") {
        return Some(("masked".to_string(), name.to_string(), String::new()));
    }
    if let Some(name) =
        rest.strip_suffix(" is masked via a generator and cannot be unmasked, skipping.")
    {
        return Some((
            "masked by generator".to_string(),
            name.to_string(),
            String::new(),
        ));
    }
    if let Some(name) = rest.strip_suffix(" is an alias to a non-existent unit, ignoring.") {
        return Some(("dangling".to_string(), name.to_string(), String::new()));
    }
    if let Some((source, path)) =
        rest.split_once(" is added as a dependency to a non-existent unit ")
    {
        let path = path.strip_suffix('.').unwrap_or(path);
        return Some((
            "destination not present".to_string(),
            path.to_string(),
            source.to_string(),
        ));
    }
    None
}

/// Split a single-quoted token off the start of `input`.
fn quoted(input: &str) -> Option<(String, &str)> {
    let rest = input.strip_prefix('\'')?;
    let end = rest.find('\'')?;
    let value = rest[..end].to_string();
    Some((value, &rest[end + 1..]))
}

fn systemctl_failure(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr.trim();
    if detail.is_empty() {
        output.status.to_string()
    } else {
        detail.to_string()
    }
}

/// Whether the named units carry `[Install]` information.  Used for the
/// `carries_install_info` reply when the real `systemctl` did the work (its
/// own change list is not relayed).
fn probe_install_info(names: &[String]) -> bool {
    let lp = LookupPaths::system();
    names.iter().any(|name| match install::resolve(name, &lp) {
        Ok(info) => !info.masked && info.info.has_install_info(),
        Err(_) => false,
    })
}

async fn run_fallback(
    verb: Verb,
    names: Vec<String>,
    runtime: bool,
    force: bool,
) -> Result<Outcome, String> {
    tokio::task::spawn_blocking(move || fallback(&verb, &names, runtime, force))
        .await
        .map_err(|e| {
            sysa::l10n::fmt(
                sysa::l10n::t_("The install fallback task failed: {e}."),
                &[("e", &e.to_string())],
            )
        })?
}

fn fallback(verb: &Verb, names: &[String], runtime: bool, force: bool) -> Result<Outcome, String> {
    let lp = LookupPaths::system();
    let opts = install::Options::new(runtime, force);
    let mut outcome = Outcome::default();
    let changes = match verb {
        Verb::Enable => {
            let (carries, changes) = install::enable(&lp, names, &opts);
            outcome.carries_install_info = Some(carries);
            changes
        }
        Verb::Disable { install_info } => {
            let (carries, changes) = install::disable(&lp, names, &opts);
            if *install_info {
                outcome.carries_install_info = Some(carries);
            }
            changes
        }
        Verb::Reenable => {
            let (carries, changes) = install::reenable(&lp, names, &opts);
            outcome.carries_install_info = Some(carries);
            changes
        }
        Verb::Link => install::link(&lp, names, &opts),
        Verb::Mask => install::mask(&lp, names, &opts),
        Verb::Unmask => install::unmask(&lp, names, &opts),
        Verb::Revert => install::revert(&lp, names),
        Verb::Preset(mode) => {
            let mode = mode
                .as_deref()
                .and_then(install::preset::PresetMode::from_name)
                .unwrap_or(install::preset::PresetMode::Full);
            let (carries, changes) = install::preset(&lp, names, &opts, mode);
            outcome.carries_install_info = Some(carries);
            changes
        }
        Verb::PresetAll(mode) => {
            let mode = install::preset::PresetMode::from_name(mode)
                .unwrap_or(install::preset::PresetMode::Full);
            let (_, changes) = install::preset_all(&lp, &opts, mode);
            changes
        }
        Verb::SetDefault => {
            let name = names.first().cloned().unwrap_or_default();
            install::set_default(&lp, &name, force)
        }
        Verb::GetDefault => {
            outcome.default_target = Some(install::get_default(&lp)?);
            install::Changes::default()
        }
    };

    if let Some(message) = changes.error {
        return Err(message);
    }
    outcome.changes = changes.records;
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    /// Create an executable fake `systemctl` with the given script body.
    fn fake_systemctl(tag: &str, body: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "systema-sysw-systemctl-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let script = dir.join("systemctl");
        fs::write(&script, body).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    #[test]
    fn parses_client_side_change_lines() {
        assert_eq!(
            parse_change_line(
                "Created symlink '/etc/systemd/system/sshd.service' \u{2192} '/usr/lib/systemd/system/ssh.service'."
            ),
            Some((
                "symlink".to_string(),
                "/etc/systemd/system/sshd.service".to_string(),
                "/usr/lib/systemd/system/ssh.service".to_string(),
            ))
        );
        assert_eq!(
            parse_change_line(
                "Created symlink '/etc/systemd/system/a.service' -> '/usr/lib/systemd/system/a.service'."
            ),
            Some((
                "symlink".to_string(),
                "/etc/systemd/system/a.service".to_string(),
                "/usr/lib/systemd/system/a.service".to_string(),
            ))
        );
        assert_eq!(
            parse_change_line("Removed '/etc/systemd/system/sshd.service'."),
            Some((
                "unlink".to_string(),
                "/etc/systemd/system/sshd.service".to_string(),
                String::new(),
            ))
        );
        assert_eq!(
            parse_change_line("Unit foo.service is masked, ignoring."),
            Some((
                "masked".to_string(),
                "foo.service".to_string(),
                String::new(),
            ))
        );
        assert_eq!(
            parse_change_line(
                "Unit /usr/lib/systemd/system/foo.service is added as a dependency to a non-existent unit bar.target."
            ),
            Some((
                "destination not present".to_string(),
                "bar.target".to_string(),
                "/usr/lib/systemd/system/foo.service".to_string(),
            ))
        );
        // Unrelated output from the client-side run is ignored.
        assert_eq!(
            parse_change_line(
                "Synchronizing state of ssh.service with SysV service script with /usr/lib/systemd/systemd-sysv-install."
            ),
            None
        );
    }

    #[test]
    fn parses_every_change_type() {
        let path = "/etc/systemd/system/multi-user.target.wants/a.service";
        let cases = [
            (
                format!("Masked dependency '{path}'."),
                ("mask-dependency", path, ""),
            ),
            (
                format!("Unmasked dependency '{path}'."),
                ("unmask-dependency", path, ""),
            ),
            (
                "Unit foo.service is masked via a generator and cannot be unmasked, skipping."
                    .to_string(),
                ("masked by generator", "foo.service", ""),
            ),
            (
                "Unit dangling.service is an alias to a non-existent unit, ignoring.".to_string(),
                ("dangling", "dangling.service", ""),
            ),
            (
                "Failed to enable auxiliary unit also.service, ignoring.".to_string(),
                ("auxiliary unit failed", "also.service", ""),
            ),
        ];
        for (line, (kind, change_path, source)) in cases {
            assert_eq!(
                parse_change_line(&line),
                Some((
                    kind.to_string(),
                    change_path.to_string(),
                    source.to_string()
                )),
                "line: {line}"
            );
        }
    }

    #[test]
    fn aggregates_streams_in_order_and_skips_noise() {
        let stderr = "\
Synchronizing state of ssh.service with SysV service script with /usr/lib/systemd/systemd-sysv-install.
Executing: /usr/lib/systemd/systemd-sysv-install enable ssh
Created symlink '/etc/systemd/system/a.service' -> '/usr/lib/systemd/system/a.service'.
Removed '/etc/systemd/system/b.service'.
";
        let stdout = "Created symlink '/etc/systemd/system/c.service' -> '/usr/lib/systemd/system/c.service'.\n";
        assert_eq!(
            parse_change_text(stderr, stdout),
            vec![
                (
                    "symlink".to_string(),
                    "/etc/systemd/system/a.service".to_string(),
                    "/usr/lib/systemd/system/a.service".to_string(),
                ),
                (
                    "unlink".to_string(),
                    "/etc/systemd/system/b.service".to_string(),
                    String::new(),
                ),
                (
                    "symlink".to_string(),
                    "/etc/systemd/system/c.service".to_string(),
                    "/usr/lib/systemd/system/c.service".to_string(),
                ),
            ]
        );
    }

    #[tokio::test]
    async fn run_systemctl_relays_changes_and_offline_environment() {
        let dir = std::env::temp_dir().join(format!(
            "systema-sysw-systemctl-env-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let env_file = dir.join("env");
        let script = format!(
            "#!/bin/sh\n\
             printf '%s|%s|%s|%s' \"$SYSTEMD_OFFLINE\" \"$SYSTEMCTL_INSTALL_CLIENT_SIDE\" \"$LC_ALL\" \"$SYSTEMD_LOG_LEVEL\" > '{}'\n\
             echo \"Created symlink '/etc/systemd/system/a.service' -> '/usr/lib/systemd/system/a.service'.\" >&2\n\
             echo \"Removed '/etc/systemd/system/b.service'.\" >&2\n\
             exit 0\n",
            env_file.display()
        );
        let systemctl = fake_systemctl("relay", &script);

        let outcome = run_systemctl(
            &systemctl,
            &Verb::Mask,
            &["a.service".to_string()],
            false,
            false,
        )
        .await
        .unwrap();

        assert_eq!(
            outcome.changes,
            vec![
                (
                    "symlink".to_string(),
                    "/etc/systemd/system/a.service".to_string(),
                    "/usr/lib/systemd/system/a.service".to_string(),
                ),
                (
                    "unlink".to_string(),
                    "/etc/systemd/system/b.service".to_string(),
                    String::new(),
                ),
            ]
        );
        assert!(outcome.carries_install_info.is_none());
        assert_eq!(fs::read_to_string(&env_file).unwrap(), "1|1|C|info");
    }

    #[tokio::test]
    async fn failing_systemctl_reports_stderr() {
        let systemctl = fake_systemctl(
            "failure",
            "#!/bin/sh\necho 'boom: cannot install' >&2\nexit 3\n",
        );
        let error = run_systemctl(
            &systemctl,
            &Verb::Mask,
            &["a.service".to_string()],
            false,
            false,
        )
        .await
        .unwrap_err();
        assert!(error.contains("boom: cannot install"), "{error}");
    }

    #[test]
    fn fallback_mode_is_a_clap_value_enum() {
        use clap::ValueEnum;
        let values = ServiceInstallFallback::value_variants();
        assert_eq!(values.len(), 3);
        assert_eq!(
            ServiceInstallFallback::from_str("always", true).unwrap(),
            ServiceInstallFallback::Always
        );
        assert!(ServiceInstallFallback::from_str("bogus", true).is_err());
    }
}
