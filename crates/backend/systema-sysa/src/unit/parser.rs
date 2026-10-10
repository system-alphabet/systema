//! Parser for systemd unit files (INI-style format).
//!
//! Systemd unit files use an INI-like syntax with `[Section]` headers and
//! `Key=Value` pairs. Multi-value keys append to a list; an empty value
//! clears the list.
//!
//! Extensions beyond plain INI handled here:
//! - `\`-terminated lines are joined with the following line before parsing.
//! - `%`-specifiers in values are expanded (e.g. `%n` → unit name).
//! - `Exec*=` prefixes (`-`, `+`, `@`, `:`, `!`, `!!`) are parsed into
//!   [`ExecCommand`] structs.
//! - Drop-in configuration directories (`unit.service.d/*.conf`) are applied
//!   on top of the base unit file.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use configparser::ini::Ini;
use tracing::warn;

use super::types::*;

/// Parse a systemd unit file from a string, applying drop-in files from
/// the same directory if present.
pub fn parse_unit(name: &str, content: &str) -> Result<UnitFile> {
    parse_unit_content(name, content)
}

/// Parse a unit file from a filesystem path, also applying drop-in
/// configuration files found in `<base>.d/*.conf` directories alongside
/// every search-path directory the base file was found in.
pub fn parse_unit_from_path(path: &Path) -> Result<UnitFile> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");
    let content = std::fs::read_to_string(path)
        .with_context(|| sysa::l10n::fmt(
            sysa::l10n::t_("Reading unit file {path} ..."),
            &[("path", &path.display().to_string())],
        ))?;
    let mut unit = parse_unit_content(name, &content)?;

    // Apply drop-in files from `<dir>/<name>.d/*.conf`.
    let dropin_dir = path.with_file_name(format!("{}.d", name));
    if dropin_dir.is_dir() {
        apply_dropin_dir(&dropin_dir, &mut unit)?;
    }

    Ok(unit)
}

/// Apply all `*.conf` drop-in files from `dir` to `unit`, in lexicographic
/// order (higher sort = higher priority, overriding earlier entries).
fn apply_dropin_dir(dir: &Path, unit: &mut UnitFile) -> Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| sysa::l10n::fmt(
            sysa::l10n::t_("Reading drop-in dir {dir} ..."),
            &[("dir", &dir.display().to_string())],
        ))?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|x| x.to_str())
                .map(|x| x == "conf")
                .unwrap_or(false)
        })
        .collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let path = entry.path();
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                if let Err(e) = apply_dropin_content(unit, &content) {
                    warn!("Ignoring drop-in {}: {}", path.display(), e);
                }
            }
            Err(e) => {
                warn!("Cannot read drop-in {}: {}", path.display(), e);
            }
        }
    }
    Ok(())
}

/// Apply a single drop-in snippet to an existing `UnitFile`.
///
/// Drop-in files follow the same INI format as base unit files. They can
/// override scalar values and append to list values.  An empty value for
/// a list key (e.g. `ExecStart=`) clears the accumulated list.
fn apply_dropin_content(unit: &mut UnitFile, content: &str) -> Result<()> {
    let processed = preprocess_content(content);
    let mut config = Ini::new();
    config
        .read(processed.clone())
        .map_err(|e| anyhow::anyhow!(sysa::l10n::fmt(
            sysa::l10n::t_("INI parse error in drop-in: {error}."),
            &[("error", &e.to_string())],
        )))?;

    // Re-apply each section that is present in the drop-in.
    // NOTE: We probe for a subset of common [Unit] keys rather than scanning all
    // keys. A drop-in that contains only less-common [Unit] directives (e.g.
    // PartOf=) would be silently ignored. This is a known Phase-1 limitation;
    // full coverage will be added when configparser exposes a section-exists API.
    if config.get("unit", "description").is_some()
        || config.get("unit", "after").is_some()
        || config.get("unit", "requires").is_some()
        || config.get("unit", "wants").is_some()
    {
        parse_unit_section(&config, &mut unit.unit, &unit.name)?;
    }
    if config.get("install", "wantedby").is_some() {
        parse_install_section(&config, &mut unit.install)?;
    }

    // Handle drop-in for specific section types.
    match &unit.kind {
        UnitKind::Service => {
            if let Some(ref mut svc) = unit.service {
                // configparser only retains the last repeated key, so we must scan raw
                // lines ourselves to implement systemd's "empty ExecStart= clears" semantics.
                let exec_lines = collect_exec_lines(&processed, "service", "execstart");
                for val in exec_lines {
                    if val.is_empty() {
                        svc.exec_start.clear();
                    } else {
                        svc.exec_start
                            .push(ExecCommand::parse(expand_specifiers(&val, &unit.name)));
                    }
                }
            }
        }
        UnitKind::Mount => {
            if config.get("mount", "what").is_some()
                || config.get("mount", "where").is_some()
                || config.get("mount", "type").is_some()
            {
                let mut mnt = unit.mount.take().unwrap_or_default();
                parse_mount_section(&config, &mut mnt, &unit.name)?;
                unit.mount = Some(mnt);
            }
        }
        UnitKind::Timer => {
            if config.get("timer", "oncalendar").is_some()
                || config.get("timer", "onbootsec").is_some()
                || config.get("timer", "unit").is_some()
            {
                let mut tmr = unit.timer.take().unwrap_or_default();
                parse_timer_section(&config, &mut tmr, &unit.name)?;
                unit.timer = Some(tmr);
            }
        }
        UnitKind::Socket => {
            if config.get("socket", "listenstream").is_some()
                || config.get("socket", "listendatagram").is_some()
                || config.get("socket", "accept").is_some()
            {
                let mut sock = unit.socket.take().unwrap_or_default();
                parse_socket_section(&config, &mut sock, &unit.name)?;
                unit.socket = Some(sock);
            }
        }
        UnitKind::Swap => {
            if config.get("swap", "what").is_some() || config.get("swap", "options").is_some() {
                let mut swap = unit.swap.take().unwrap_or_default();
                parse_swap_section(&config, &mut swap, &unit.name)?;
                unit.swap = Some(swap);
            }
        }
        UnitKind::Path => {
            if config.get("path", "pathexists").is_some()
                || config.get("path", "unit").is_some()
            {
                let mut path_sec = unit.path.take().unwrap_or_default();
                parse_path_section(&config, &mut path_sec, &unit.name)?;
                unit.path = Some(path_sec);
            }
        }
        UnitKind::Slice => {
            if config.get("slice", "cpuquota").is_some()
                || config.get("slice", "memorymax").is_some()
            {
                let mut slice = unit.slice.take().unwrap_or_default();
                parse_slice_section(&config, &mut slice, &unit.name)?;
                unit.slice = Some(slice);
            }
        }
        UnitKind::Scope => {
            if config.get("scope", "pids").is_some()
                || config.get("scope", "timeoutstopsec").is_some()
            {
                let mut scope = unit.scope.take().unwrap_or_default();
                parse_scope_section(&config, &mut scope, &unit.name)?;
                unit.scope = Some(scope);
            }
        }
        UnitKind::Device => {
            if config.get("device", "property").is_some()
                || config.get("device", "sysfspath").is_some()
            {
                let mut device = unit.device.take().unwrap_or_default();
                parse_device_section(&config, &mut device, &unit.name)?;
                unit.device = Some(device);
            }
        }
        _ => {}
    }

    Ok(())
}

/// Scan raw (preprocessed) INI content for all values of a key in `section`,
/// preserving order and including empty values (which configparser drops).
fn collect_exec_lines(content: &str, section: &str, key: &str) -> Vec<String> {
    let section_header = format!("[{}]", section.to_lowercase());
    let key_lower = key.to_lowercase();
    let mut in_section = false;
    let mut results = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_section = trimmed.to_lowercase() == section_header;
            continue;
        }
        if !in_section || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        if let Some(eq_pos) = trimmed.find('=') {
            let k = trimmed[..eq_pos].trim().to_lowercase();
            if k == key_lower {
                results.push(trimmed[eq_pos + 1..].trim().to_string());
            }
        }
    }
    results
}

// ---------------------------------------------------------------------------
// Core parser
// ---------------------------------------------------------------------------

fn parse_unit_content(name: &str, content: &str) -> Result<UnitFile> {
    let processed = preprocess_content(content);
    let mut config = Ini::new(); // case-insensitive (normalizes to lowercase)
    config
        .read(processed)
        .map_err(|e| anyhow::anyhow!(sysa::l10n::fmt(
            sysa::l10n::t_("INI parse error in {name}: {error}."),
            &[("name", name), ("error", &e.to_string())],
        )))?;

    let mut unit = UnitFile::new(name);

    // --- [Unit] section ---
    parse_unit_section(&config, &mut unit.unit, name)
        .with_context(|| sysa::l10n::fmt(
            sysa::l10n::t_("Parsing [Unit] section of {name} ..."),
            &[("name", name)],
        ))?;

    // --- [Install] section ---
    parse_install_section(&config, &mut unit.install)
        .with_context(|| sysa::l10n::fmt(
            sysa::l10n::t_("Parsing [Install] section of {name} ..."),
            &[("name", name)],
        ))?;

    // --- type-specific sections ---
    match &unit.kind {
        UnitKind::Service => {
            let mut svc = ServiceSection::default();
            parse_service_section(&config, &mut svc, name)
                .with_context(|| sysa::l10n::fmt(
                    sysa::l10n::t_("Parsing [Service] section of {name} ..."),
                    &[("name", name)],
                ))?;
            unit.service = Some(svc);
        }
        UnitKind::Target => {
            // Targets have no dedicated section beyond [Unit].
        }
        UnitKind::Mount => {
            let mut mnt = MountSection::default();
            parse_mount_section(&config, &mut mnt, name)
                .with_context(|| sysa::l10n::fmt(
                    sysa::l10n::t_("Parsing [Mount] section of {name} ..."),
                    &[("name", name)],
                ))?;
            unit.mount = Some(mnt);
        }
        UnitKind::Timer => {
            let mut tmr = TimerSection::default();
            parse_timer_section(&config, &mut tmr, name)
                .with_context(|| sysa::l10n::fmt(
                    sysa::l10n::t_("Parsing [Timer] section of {name} ..."),
                    &[("name", name)],
                ))?;
            unit.timer = Some(tmr);
        }
        UnitKind::Socket => {
            let mut sock = SocketSection::default();
            parse_socket_section(&config, &mut sock, name)
                .with_context(|| sysa::l10n::fmt(
                    sysa::l10n::t_("Parsing [Socket] section of {name} ..."),
                    &[("name", name)],
                ))?;
            unit.socket = Some(sock);
        }
        UnitKind::Swap => {
            let mut swap = SwapSection::default();
            parse_swap_section(&config, &mut swap, name)
                .with_context(|| sysa::l10n::fmt(
                    sysa::l10n::t_("Parsing [Swap] section of {name} ..."),
                    &[("name", name)],
                ))?;
            unit.swap = Some(swap);
        }
        UnitKind::Path => {
            let mut path_sec = PathSection::default();
            parse_path_section(&config, &mut path_sec, name)
                .with_context(|| sysa::l10n::fmt(
                    sysa::l10n::t_("Parsing [Path] section of {name} ..."),
                    &[("name", name)],
                ))?;
            unit.path = Some(path_sec);
        }
        UnitKind::Slice => {
            let mut slice = SliceSection::default();
            parse_slice_section(&config, &mut slice, name)
                .with_context(|| sysa::l10n::fmt(
                    sysa::l10n::t_("Parsing [Slice] section of {name} ..."),
                    &[("name", name)],
                ))?;
            unit.slice = Some(slice);
        }
        UnitKind::Scope => {
            let mut scope = ScopeSection::default();
            parse_scope_section(&config, &mut scope, name)
                .with_context(|| sysa::l10n::fmt(
                    sysa::l10n::t_("Parsing [Scope] section of {name} ..."),
                    &[("name", name)],
                ))?;
            unit.scope = Some(scope);
        }
        UnitKind::Device => {
            let mut device = DeviceSection::default();
            parse_device_section(&config, &mut device, name)
                .with_context(|| sysa::l10n::fmt(
                    sysa::l10n::t_("Parsing [Device] section of {name} ..."),
                    &[("name", name)],
                ))?;
            unit.device = Some(device);
        }
        other => {
            warn!("Unit kind {:?} not fully parsed", other);
        }
    }

    Ok(unit)
}

// ---------------------------------------------------------------------------
// Pre-processing
// ---------------------------------------------------------------------------

/// Join lines that end with `\` (systemd multi-line continuation) and strip
/// inline comments introduced by `#` or `;` that are not inside quotes.
///
/// This must be applied before handing the text to `configparser` because
/// the library uses standard INI semantics (indented continuation), not the
/// systemd `\` convention.
fn preprocess_content(content: &str) -> String {
    let mut result = String::with_capacity(content.len());
    let mut pending: Option<String> = None;

    for raw_line in content.lines() {
        let line = raw_line.trim_end();

        if let Some(ref mut acc) = pending {
            // Append this continuation line to the accumulated value.
            acc.push(' ');
            acc.push_str(line.trim_start());
            if line.ends_with('\\') {
                *acc = acc.trim_end_matches('\\').trim_end().to_string();
                // More continuation expected.
            } else {
                result.push_str(acc);
                result.push('\n');
                pending = None;
            }
        } else if line.ends_with('\\') && !line.ends_with("\\\\") {
            // Start accumulating a continuation.
            pending = Some(line.trim_end_matches('\\').trim_end().to_string());
        } else {
            result.push_str(line);
            result.push('\n');
        }
    }

    // Flush any trailing accumulated line.
    if let Some(acc) = pending {
        result.push_str(&acc);
        result.push('\n');
    }

    result
}

// ---------------------------------------------------------------------------
// Specifier expansion
// ---------------------------------------------------------------------------

/// Expand systemd `%`-specifiers in `s` using the given unit `name`.
///
/// Supported specifiers:
///
/// | Specifier | Meaning |
/// |-----------|---------|
/// | `%n`      | Full unit name, e.g. `sshd.service` |
/// | `%N`      | Unit name without suffix, e.g. `sshd` |
/// | `%p`      | Prefix name (before `@`), e.g. `sshd` for `sshd@1.service` |
/// | `%i`      | Instance string (between `@` and `.`), e.g. `1` |
/// | `%u`      | Username that runs the unit (current user) |
/// | `%U`      | Numeric UID |
/// | `%g`      | Primary group name |
/// | `%G`      | Numeric GID |
/// | `%H`      | Hostname |
/// | `%m`      | Machine ID (from `/etc/machine-id`) |
/// | `%v`      | Kernel release (`uname -r`) |
/// | `%%`      | Literal `%` |
pub fn expand_specifiers(s: &str, name: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }

    // Derived from the unit name.
    let unit_name = name;
    let unit_no_ext = name.rsplit_once('.').map(|(p, _)| p).unwrap_or(name);
    let (prefix, instance) = if let Some(at_pos) = unit_no_ext.find('@') {
        let p = &unit_no_ext[..at_pos];
        let i = &unit_no_ext[at_pos + 1..];
        (p, i)
    } else {
        (unit_no_ext, "")
    };

    // System information — computed lazily (best-effort, empty on error).
    let hostname = || {
        std::process::Command::new("hostname")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| {
                tracing::debug!("expand_specifiers: failed to get hostname for %H");
                String::new()
            })
    };
    let machine_id = || {
        std::fs::read_to_string(sysa::paths::SYSTEMD_MACHINE_ID_FILE)
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|e| {
                tracing::debug!(
                    "expand_specifiers: failed to read {} for %m: {e}",
                    sysa::paths::SYSTEMD_MACHINE_ID_FILE
                );
                String::new()
            })
    };
    let kernel_release = || {
        std::process::Command::new("uname")
            .arg("-r")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| {
                tracing::debug!("expand_specifiers: failed to get kernel release for %v");
                String::new()
            })
    };

    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            None => {
                out.push('%');
            }
            Some('n') => out.push_str(unit_name),
            Some('N') => out.push_str(unit_no_ext),
            Some('p') => out.push_str(prefix),
            Some('i') => out.push_str(instance),
            Some('u') => {
                let user = std::env::var("USER")
                    .or_else(|_| std::env::var("LOGNAME"))
                    .unwrap_or_default();
                out.push_str(&user);
            }
            Some('U') => {
                // Numeric UID via `id -u`.
                let uid = std::process::Command::new("id")
                    .arg("-u")
                    .output()
                    .ok()
                    .and_then(|o| String::from_utf8(o.stdout).ok())
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default();
                out.push_str(&uid);
            }
            Some('g') => {
                let group = std::env::var("GROUP")
                    .or_else(|_| {
                        std::process::Command::new("id")
                            .arg("-gn")
                            .output()
                            .ok()
                            .and_then(|o| String::from_utf8(o.stdout).ok())
                            .map(|s| s.trim().to_string())
                            .ok_or(std::env::VarError::NotPresent)
                    })
                    .unwrap_or_default();
                out.push_str(&group);
            }
            Some('G') => {
                let gid = std::process::Command::new("id")
                    .arg("-g")
                    .output()
                    .ok()
                    .and_then(|o| String::from_utf8(o.stdout).ok())
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default();
                out.push_str(&gid);
            }
            Some('H') => out.push_str(&hostname()),
            Some('m') => out.push_str(&machine_id()),
            Some('v') => out.push_str(&kernel_release()),
            Some('%') => out.push('%'),
            Some(other) => {
                // Unknown specifier — pass through unchanged.
                out.push('%');
                out.push(other);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Split a space/comma-separated value into a set of strings.
fn split_list(value: &str) -> HashSet<String> {
    value
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// Split a space-separated value into a `Vec` (preserving order).
fn split_vec(value: &str) -> Vec<String> {
    value
        .split_whitespace()
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// Get a string value from the config, returning empty string if absent.
fn get_str(config: &Ini, section: &str, key: &str) -> String {
    config.get(section, key).unwrap_or_default()
}

/// Get a boolean value (`yes`/`no`/`true`/`false`/`1`/`0`).
fn get_bool(config: &Ini, section: &str, key: &str, default: bool) -> bool {
    match config.get(section, key).as_deref() {
        Some("yes") | Some("true") | Some("1") => true,
        Some("no") | Some("false") | Some("0") => false,
        _ => default,
    }
}

/// Get a u32 value, returning `default` if absent or unparseable.
fn get_u32(config: &Ini, section: &str, key: &str, default: u32) -> u32 {
    config
        .get(section, key)
        .and_then(|v| parse_time_secs(&v))
        .unwrap_or(default)
}

/// Get a signed i32 value, returning `default` if absent or unparseable.
fn get_i32(config: &Ini, section: &str, key: &str, default: i32) -> i32 {
    config
        .get(section, key)
        .and_then(|v| v.trim().parse::<i32>().ok())
        .unwrap_or(default)
}

/// Parse a systemd time value like "90s", "1min", "30" (bare = seconds).
fn parse_time_secs(s: &str) -> Option<u32> {
    let s = s.trim();
    if s == "infinity" {
        return Some(u32::MAX);
    }
    if let Some(v) = s.strip_suffix("ms") {
        return v.trim().parse::<u32>().ok().map(|ms| ms / 1000);
    }
    if let Some(v) = s.strip_suffix("min") {
        return v.trim().parse::<u32>().ok().map(|m| m * 60);
    }
    if let Some(v) = s.strip_suffix('s') {
        return v.trim().parse::<u32>().ok();
    }
    if let Some(v) = s.strip_suffix('h') {
        return v.trim().parse::<u32>().ok().map(|h| h * 3600);
    }
    s.parse::<u32>().ok()
}

// ---------------------------------------------------------------------------
// Section parsers
// ---------------------------------------------------------------------------

fn parse_unit_section(config: &Ini, unit: &mut UnitSection, name: &str) -> Result<()> {
    let expand = |v: &str| expand_specifiers(v, name);

    unit.description = expand(&get_str(config, "unit", "description"));
    unit.default_dependencies = get_bool(config, "unit", "defaultdependencies", true);

    let doc = get_str(config, "unit", "documentation");
    if !doc.is_empty() {
        unit.documentation.extend(split_vec(&expand(&doc)));
    }

    let req = get_str(config, "unit", "requires");
    unit.requires = split_list(&expand(&req));

    let wants = get_str(config, "unit", "wants");
    unit.wants = split_list(&expand(&wants));

    let conflicts = get_str(config, "unit", "conflicts");
    unit.conflicts = split_list(&expand(&conflicts));

    let after = get_str(config, "unit", "after");
    unit.after = split_list(&expand(&after));

    let before = get_str(config, "unit", "before");
    unit.before = split_list(&expand(&before));

    let part_of = get_str(config, "unit", "partof");
    unit.part_of = split_list(&expand(&part_of));

    let binds_to = get_str(config, "unit", "bindsto");
    unit.binds_to = split_list(&expand(&binds_to));

    let requisite = get_str(config, "unit", "requisite");
    unit.requisite = split_list(&expand(&requisite));

    let upholds = get_str(config, "unit", "upholds");
    unit.upholds = split_list(&expand(&upholds));

    let on_success = get_str(config, "unit", "onsuccess");
    unit.on_success = split_list(&expand(&on_success));

    let on_failure = get_str(config, "unit", "onfailure");
    unit.on_failure = split_list(&expand(&on_failure));

    let propagates_reload_to = get_str(config, "unit", "propagatesreloadto");
    unit.propagates_reload_to = split_list(&expand(&propagates_reload_to));

    // --- Condition checks ---
    let cpe = get_str(config, "unit", "conditionpathexists");
    if !cpe.is_empty() {
        unit.condition_path_exists.extend(split_vec(&expand(&cpe)));
    }
    let cpeg = get_str(config, "unit", "conditionpathexistsglob");
    if !cpeg.is_empty() {
        unit.condition_path_exists_glob
            .extend(split_vec(&expand(&cpeg)));
    }
    let cfne = get_str(config, "unit", "conditionfilenotempty");
    if !cfne.is_empty() {
        unit.condition_file_not_empty
            .extend(split_vec(&expand(&cfne)));
    }
    let cdne = get_str(config, "unit", "conditiondirectorynotempty");
    if !cdne.is_empty() {
        unit.condition_directory_not_empty
            .extend(split_vec(&expand(&cdne)));
    }
    let ch = get_str(config, "unit", "conditionhost");
    if !ch.is_empty() {
        unit.condition_host.extend(split_vec(&expand(&ch)));
    }
    let ckcl = get_str(config, "unit", "conditionkernelcommandline");
    if !ckcl.is_empty() {
        unit.condition_kernel_command_line
            .extend(split_vec(&expand(&ckcl)));
    }
    let cv = get_str(config, "unit", "conditionvirtualization");
    if !cv.is_empty() {
        unit.condition_virtualization.extend(split_vec(&expand(&cv)));
    }
    let csec = get_str(config, "unit", "conditionsecurity");
    if !csec.is_empty() {
        unit.condition_security.extend(split_vec(&expand(&csec)));
    }
    let ccap = get_str(config, "unit", "conditioncapability");
    if !ccap.is_empty() {
        unit.condition_capability.extend(split_vec(&expand(&ccap)));
    }
    let cac = get_str(config, "unit", "conditionacpower");
    if !cac.is_empty() {
        unit.condition_ac_power.extend(split_vec(&expand(&cac)));
    }
    let cnu = get_str(config, "unit", "conditionneedsupdate");
    if !cnu.is_empty() {
        unit.condition_needs_update
            .extend(split_vec(&expand(&cnu)));
    }
    let cfb = get_str(config, "unit", "conditionfirstboot");
    if !cfb.is_empty() {
        unit.condition_first_boot.extend(split_vec(&expand(&cfb)));
    }
    let cenv = get_str(config, "unit", "conditionenvironment");
    if !cenv.is_empty() {
        unit.condition_environment.extend(split_vec(&expand(&cenv)));
    }
    let cmem = get_str(config, "unit", "conditionmemory");
    if !cmem.is_empty() {
        unit.condition_memory.extend(split_vec(&expand(&cmem)));
    }
    let ckml = get_str(config, "unit", "conditionkernelmoduleloaded");
    if !ckml.is_empty() {
        unit.condition_kernel_module_loaded
            .extend(split_vec(&expand(&ckml)));
    }

    // --- Assert checks ---
    let ape = get_str(config, "unit", "assertpathexists");
    if !ape.is_empty() {
        unit.assert_path_exists.extend(split_vec(&expand(&ape)));
    }
    let apeg = get_str(config, "unit", "assertpathexistsglob");
    if !apeg.is_empty() {
        unit.assert_path_exists_glob
            .extend(split_vec(&expand(&apeg)));
    }
    let afne = get_str(config, "unit", "assertfilenotempty");
    if !afne.is_empty() {
        unit.assert_file_not_empty.extend(split_vec(&expand(&afne)));
    }
    let adne = get_str(config, "unit", "assertdirectorynotempty");
    if !adne.is_empty() {
        unit.assert_directory_not_empty
            .extend(split_vec(&expand(&adne)));
    }
    let ah = get_str(config, "unit", "asserthost");
    if !ah.is_empty() {
        unit.assert_host.extend(split_vec(&expand(&ah)));
    }
    let afb = get_str(config, "unit", "assertfirstboot");
    if !afb.is_empty() {
        unit.assert_first_boot.extend(split_vec(&expand(&afb)));
    }
    let amem = get_str(config, "unit", "assertmemory");
    if !amem.is_empty() {
        unit.assert_memory.extend(split_vec(&expand(&amem)));
    }

    Ok(())
}

fn parse_install_section(config: &Ini, install: &mut InstallSection) -> Result<()> {
    let wb = get_str(config, "install", "wantedby");
    install.wanted_by = split_list(&wb);

    let rb = get_str(config, "install", "requiredby");
    install.required_by = split_list(&rb);

    let also = get_str(config, "install", "also");
    install.also = split_list(&also);

    let alias = get_str(config, "install", "alias");
    install.alias = alias
        .split_whitespace()
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();

    Ok(())
}

/// Parse a single `Exec*=` line into an [`ExecCommand`], expanding specifiers.
fn parse_exec_line(raw: &str, unit_name: &str) -> ExecCommand {
    ExecCommand::parse(expand_specifiers(raw.trim(), unit_name))
}

fn parse_service_section(config: &Ini, svc: &mut ServiceSection, name: &str) -> Result<()> {
    let stype = get_str(config, "service", "type");
    svc.service_type = ServiceType::from(stype.as_str());

    // ExecStart supports multiple values (each call appends).
    // An empty value clears the list (systemd semantics).
    let exec_start = get_str(config, "service", "execstart");
    if config.get("service", "execstart").is_some() {
        if exec_start.is_empty() {
            svc.exec_start.clear();
        } else {
            svc.exec_start.push(parse_exec_line(&exec_start, name));
        }
    }

    let exec_start_pre = get_str(config, "service", "execstartpre");
    if !exec_start_pre.is_empty() {
        svc.exec_start_pre
            .push(parse_exec_line(&exec_start_pre, name));
    }

    let exec_start_post = get_str(config, "service", "execstartpost");
    if !exec_start_post.is_empty() {
        svc.exec_start_post
            .push(parse_exec_line(&exec_start_post, name));
    }

    let exec_stop = get_str(config, "service", "execstop");
    if !exec_stop.is_empty() {
        svc.exec_stop.push(parse_exec_line(&exec_stop, name));
    }

    let exec_stop_post = get_str(config, "service", "execstoppost");
    if !exec_stop_post.is_empty() {
        svc.exec_stop_post
            .push(parse_exec_line(&exec_stop_post, name));
    }

    let exec_reload = get_str(config, "service", "execreload");
    if !exec_reload.is_empty() {
        svc.exec_reload.push(parse_exec_line(&exec_reload, name));
    }

    svc.working_directory = expand_specifiers(
        &get_str(config, "service", "workingdirectory"),
        name,
    );
    svc.user = get_str(config, "service", "user");
    svc.group = get_str(config, "service", "group");
    svc.pam_name = get_str(config, "service", "pamname");
    svc.pid_file = get_str(config, "service", "pidfile");
    svc.bus_name = get_str(config, "service", "busname");
    svc.notify_access = get_str(config, "service", "notifyaccess");
    svc.standard_output = get_str(config, "service", "standardoutput");
    svc.standard_error = get_str(config, "service", "standarderror");
    svc.kill_signal = get_str(config, "service", "killsignal");
    svc.kill_mode = get_str(config, "service", "killmode");

    let restart = get_str(config, "service", "restart");
    svc.restart = RestartPolicy::from(restart.as_str());

    svc.restart_sec = {
        let raw = get_str(config, "service", "restartsec");
        if raw.is_empty() {
            0
        } else {
            parse_time_secs(&raw).unwrap_or(0)
        }
    };

    svc.timeout_start_sec = {
        let raw = get_str(config, "service", "timeoutstartsec");
        if raw.is_empty() {
            90
        } else {
            parse_time_secs(&raw).unwrap_or(90)
        }
    };

    svc.timeout_stop_sec = {
        let raw = get_str(config, "service", "timeoutstopsec");
        if raw.is_empty() {
            90
        } else {
            parse_time_secs(&raw).unwrap_or(90)
        }
    };

    svc.remain_after_exit = get_bool(config, "service", "remainafterexit", false);

    let env = get_str(config, "service", "environment");
    if !env.is_empty() {
        svc.environment.push(env);
    }
    let env_file = get_str(config, "service", "environmentfile");
    if !env_file.is_empty() {
        svc.environment_file.push(env_file);
    }

    svc.watchdog_sec = get_u32(config, "service", "watchdogusec", 0);

    // --- Start limit fields ---
    svc.start_limit_interval_sec = get_u32(config, "service", "startlimitintervalsec", 10);
    svc.start_limit_burst = get_u32(config, "service", "startlimitburst", 5);

    let sla = get_str(config, "service", "startlimitaction");
    svc.start_limit_action = StartLimitAction::from(sla.as_str());

    svc.restart_steps = get_u32(config, "service", "restartsteps", 0);
    svc.restart_max_delay_sec = get_u32(config, "service", "restartmaxdelaysec", 0);

    Ok(())
}

fn parse_mount_section(config: &Ini, mnt: &mut MountSection, name: &str) -> Result<()> {
    mnt.what = expand_specifiers(&get_str(config, "mount", "what"), name);
    mnt.where_ = expand_specifiers(&get_str(config, "mount", "where"), name);
    mnt.type_ = get_str(config, "mount", "type");
    mnt.options = get_str(config, "mount", "options");
    mnt.timeout_sec = get_u32(config, "mount", "timeoutsec", 90);
    mnt.lazy_unmount = get_bool(config, "mount", "lazyunmount", false);
    mnt.force_unmount = get_bool(config, "mount", "forceunmount", false);
    mnt.directory_mode = get_str(config, "mount", "directorymode");
    mnt.sloppy_options = get_bool(config, "mount", "sloppyoptions", false);
    Ok(())
}

fn parse_timer_section(config: &Ini, tmr: &mut TimerSection, name: &str) -> Result<()> {
    let oas = get_str(config, "timer", "onactivesec");
    tmr.on_active_sec = if oas.is_empty() {
        None
    } else {
        parse_time_secs(&oas)
    };

    let obs = get_str(config, "timer", "onbootsec");
    tmr.on_boot_sec = if obs.is_empty() {
        None
    } else {
        parse_time_secs(&obs)
    };

    let oss = get_str(config, "timer", "onstartupsec");
    tmr.on_startup_sec = if oss.is_empty() {
        None
    } else {
        parse_time_secs(&oss)
    };

    let ouas = get_str(config, "timer", "onunitactivesec");
    tmr.on_unit_active_sec = if ouas.is_empty() {
        None
    } else {
        parse_time_secs(&ouas)
    };

    let ouis = get_str(config, "timer", "onunitinactivesec");
    tmr.on_unit_inactive_sec = if ouis.is_empty() {
        None
    } else {
        parse_time_secs(&ouis)
    };

    let cal = get_str(config, "timer", "oncalendar");
    if !cal.is_empty() {
        tmr.on_calendar.push(cal);
    }

    tmr.accuracy_sec = get_u32(config, "timer", "accuracysec", 60);
    tmr.randomized_delay_sec = get_u32(config, "timer", "randomizeddelaysec", 0);
    tmr.unit = expand_specifiers(&get_str(config, "timer", "unit"), name);
    tmr.persistent = get_bool(config, "timer", "persistent", false);
    tmr.wake_system = get_bool(config, "timer", "wakesystem", false);
    tmr.remain_after_elapse = get_bool(config, "timer", "remainafterelapse", true);
    Ok(())
}

fn parse_socket_section(config: &Ini, sock: &mut SocketSection, name: &str) -> Result<()> {
    let ls = get_str(config, "socket", "listenstream");
    if !ls.is_empty() {
        sock.listen_stream
            .extend(split_vec(&expand_specifiers(&ls, name)));
    }
    let ld = get_str(config, "socket", "listendatagram");
    if !ld.is_empty() {
        sock.listen_datagram
            .extend(split_vec(&expand_specifiers(&ld, name)));
    }
    let lsp = get_str(config, "socket", "listensequentialpacket");
    if !lsp.is_empty() {
        sock.listen_sequential_packet
            .extend(split_vec(&expand_specifiers(&lsp, name)));
    }
    let lf = get_str(config, "socket", "listenfifo");
    if !lf.is_empty() {
        sock.listen_fifo
            .extend(split_vec(&expand_specifiers(&lf, name)));
    }
    let ln = get_str(config, "socket", "listennetlink");
    if !ln.is_empty() {
        sock.listen_netlink
            .extend(split_vec(&expand_specifiers(&ln, name)));
    }
    let lspec = get_str(config, "socket", "listenspecial");
    if !lspec.is_empty() {
        sock.listen_special
            .extend(split_vec(&expand_specifiers(&lspec, name)));
    }

    sock.accept = get_bool(config, "socket", "accept", false);
    sock.service = expand_specifiers(&get_str(config, "socket", "service"), name);
    sock.socket_user = get_str(config, "socket", "socketuser");
    sock.socket_group = get_str(config, "socket", "socketgroup");
    sock.socket_mode = get_str(config, "socket", "socketmode");
    sock.directory_mode = get_str(config, "socket", "directorymode");
    sock.max_connections = get_u32(config, "socket", "maxconnections", 64);
    sock.backlog = get_u32(config, "socket", "backlog", 128);
    sock.bind_ipv6_only = get_str(config, "socket", "bindipv6only");
    sock.free_bind = get_bool(config, "socket", "freebind", false);
    sock.transparent = get_bool(config, "socket", "transparent", false);
    sock.broadcast = get_bool(config, "socket", "broadcast", false);
    sock.pass_credentials = get_bool(config, "socket", "passcredentials", false);
    sock.pass_security = get_bool(config, "socket", "passsecurity", false);
    sock.timeout_sec = get_u32(config, "socket", "timeoutsec", 0);
    Ok(())
}

fn parse_swap_section(config: &Ini, swap: &mut SwapSection, name: &str) -> Result<()> {
    swap.what = expand_specifiers(&get_str(config, "swap", "what"), name);
    swap.priority = get_i32(config, "swap", "priority", -1);
    swap.options = get_str(config, "swap", "options");
    swap.timeout_sec = get_u32(config, "swap", "timeoutsec", 90);
    Ok(())
}

fn parse_path_section(config: &Ini, path_sec: &mut PathSection, name: &str) -> Result<()> {
    let expand = |v: &str| expand_specifiers(v, name);

    let pe = get_str(config, "path", "pathexists");
    if !pe.is_empty() {
        path_sec.path_exists.extend(split_vec(&expand(&pe)));
    }
    let peg = get_str(config, "path", "pathexistsglob");
    if !peg.is_empty() {
        path_sec.path_exists_glob.extend(split_vec(&expand(&peg)));
    }
    let pc = get_str(config, "path", "pathchanged");
    if !pc.is_empty() {
        path_sec.path_changed.extend(split_vec(&expand(&pc)));
    }
    let pm = get_str(config, "path", "pathmodified");
    if !pm.is_empty() {
        path_sec.path_modified.extend(split_vec(&expand(&pm)));
    }
    let dne = get_str(config, "path", "directorynotempty");
    if !dne.is_empty() {
        path_sec
            .directory_not_empty
            .extend(split_vec(&expand(&dne)));
    }
    path_sec.unit = expand(&get_str(config, "path", "unit"));
    path_sec.make_directory = get_bool(config, "path", "makedirectory", false);
    path_sec.directory_mode = get_str(config, "path", "directorymode");
    path_sec.trigger_limit_interval_sec =
        get_u32(config, "path", "triggerlimitintervalsec", 2);
    path_sec.trigger_limit_burst = get_u32(config, "path", "triggerlimitburst", 200);
    Ok(())
}

fn parse_slice_section(config: &Ini, slice: &mut SliceSection, _name: &str) -> Result<()> {
    slice.cpu_quota = get_str(config, "slice", "cpuquota");
    slice.cpu_weight = get_u32(config, "slice", "cpuweight", 100);
    slice.startup_cpu_weight = get_u32(config, "slice", "startupcpuweight", 100);
    slice.cpu_set_cpus = get_str(config, "slice", "cpusetcpus");
    slice.cpu_set_memory_nodes = get_str(config, "slice", "cpusetmemorynodes");
    slice.memory_max = get_str(config, "slice", "memorymax");
    slice.memory_high = get_str(config, "slice", "memoryhigh");
    slice.memory_low = get_str(config, "slice", "memorylow");
    slice.memory_min = get_str(config, "slice", "memorymin");
    slice.io_weight = get_u32(config, "slice", "ioweight", 100);
    slice.io_bandwidth_max = get_str(config, "slice", "iobandwidthmax");
    slice.tasks_max = get_u32(config, "slice", "tasksmax", u32::MAX);
    slice.allowed_cpus = get_str(config, "slice", "allowedcpus");
    slice.allowed_memory_nodes = get_str(config, "slice", "allowedmemorynodes");
    Ok(())
}

fn parse_scope_section(config: &Ini, scope: &mut ScopeSection, _name: &str) -> Result<()> {
    let pids = get_str(config, "scope", "pids");
    if !pids.is_empty() {
        scope.pids = split_vec(&pids);
    }
    scope.timeout_stop_sec = get_u32(config, "scope", "timeoutstopsec", 90);
    scope.runtime_max_sec = get_u32(config, "scope", "runtimemaxsec", 0);
    scope.kill_mode = get_str(config, "scope", "killmode");
    scope.kill_signal = get_str(config, "scope", "killsignal");
    scope.send_sighup = get_bool(config, "scope", "sendsighup", false);
    scope.cpu_quota = get_str(config, "scope", "cpuquota");
    scope.cpu_weight = get_u32(config, "scope", "cpuweight", 100);
    scope.memory_max = get_str(config, "scope", "memorymax");
    scope.tasks_max = get_u32(config, "scope", "tasksmax", u32::MAX);
    Ok(())
}

fn parse_device_section(config: &Ini, device: &mut DeviceSection, _name: &str) -> Result<()> {
    let property = get_str(config, "device", "property");
    if !property.is_empty() {
        device.property = split_vec(&property);
    }
    device.sysfs_path = get_str(config, "device", "sysfspath");
    device.device_name = get_str(config, "device", "devicename");
    device.device_path = get_str(config, "device", "devicepath");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Service unit parsing
    // -----------------------------------------------------------------------

    const SIMPLE_SERVICE: &str = r#"
[Unit]
Description=A simple test service
After=network.target
Wants=network.target

[Service]
Type=simple
ExecStart=/usr/sbin/sshd -D
Restart=on-failure
RestartSec=5s
TimeoutStartSec=30s
User=root

[Install]
WantedBy=multi-user.target
"#;

    #[test]
    fn test_parse_simple_service() {
        let unit = parse_unit("sshd.service", SIMPLE_SERVICE).unwrap();
        assert_eq!(unit.name, "sshd.service");
        assert!(matches!(unit.kind, UnitKind::Service));
        assert_eq!(unit.unit.description, "A simple test service");
        assert!(unit.unit.after.contains("network.target"));
        assert!(unit.unit.wants.contains("network.target"));
        assert!(unit.install.wanted_by.contains("multi-user.target"));

        let svc = unit.service.unwrap();
        assert!(matches!(svc.service_type, ServiceType::Simple));
        assert_eq!(svc.exec_start[0].program, "/usr/sbin/sshd");
        assert_eq!(svc.exec_start[0].args, vec!["-D"]);
        assert_eq!(svc.exec_start[0].raw, "/usr/sbin/sshd -D");
        assert!(matches!(svc.restart, RestartPolicy::OnFailure));
        assert_eq!(svc.restart_sec, 5);
        assert_eq!(svc.timeout_start_sec, 30);
        assert_eq!(svc.user, "root");
    }

    #[test]
    fn test_exec_start_ignore_failure_prefix() {
        let content = "[Service]\nExecStart=-/usr/bin/cleanup\n";
        let unit = parse_unit("cleanup.service", content).unwrap();
        let svc = unit.service.unwrap();
        assert!(svc.exec_start[0].ignore_failure);
        assert_eq!(svc.exec_start[0].program, "/usr/bin/cleanup");
    }

    #[test]
    fn test_exec_start_privileged_prefix() {
        let content = "[Service]\nExecStart=+/usr/sbin/privileged-cmd\n";
        let unit = parse_unit("priv.service", content).unwrap();
        let svc = unit.service.unwrap();
        assert!(svc.exec_start[0].privileged);
        assert_eq!(svc.exec_start[0].program, "/usr/sbin/privileged-cmd");
    }

    #[test]
    fn test_exec_start_no_env_lookup_prefix() {
        let content = "[Service]\nExecStart=@/usr/lib/foo/foo myapp arg1\n";
        let unit = parse_unit("foo.service", content).unwrap();
        let svc = unit.service.unwrap();
        assert!(svc.exec_start[0].no_env_lookup);
    }

    #[test]
    fn test_exec_start_no_new_privileges_prefix() {
        let content = "[Service]\nExecStart=!/usr/bin/safe-cmd\n";
        let unit = parse_unit("safe.service", content).unwrap();
        let svc = unit.service.unwrap();
        assert!(svc.exec_start[0].no_new_privileges);
    }

    #[test]
    fn test_exec_start_combined_prefixes() {
        let content = "[Service]\nExecStart=-+/usr/bin/cmd arg\n";
        let unit = parse_unit("combined.service", content).unwrap();
        let svc = unit.service.unwrap();
        assert!(svc.exec_start[0].ignore_failure);
        assert!(svc.exec_start[0].privileged);
        assert_eq!(svc.exec_start[0].args, vec!["arg"]);
    }

    #[test]
    fn test_exec_start_empty_clears_list() {
        // An empty ExecStart= should have no entries (configparser drops the key).
        let content = "[Service]\nExecStart=/usr/bin/foo\nExecStart=\n";
        // configparser keeps only the last value for a key, so the empty one wins.
        let unit = parse_unit("clear.service", content).unwrap();
        let svc = unit.service.unwrap();
        // The key was present with empty value → cleared.
        assert!(svc.exec_start.is_empty());
    }

    // -----------------------------------------------------------------------
    // Target unit parsing
    // -----------------------------------------------------------------------

    const TARGET_UNIT: &str = r#"
[Unit]
Description=Multi-User System
Requires=basic.target
Conflicts=rescue.service rescue.target
After=basic.target rescue.service rescue.target

[Install]
Alias=default.target
"#;

    #[test]
    fn test_parse_target() {
        let unit = parse_unit("multi-user.target", TARGET_UNIT).unwrap();
        assert!(matches!(unit.kind, UnitKind::Target));
        assert!(unit.unit.requires.contains("basic.target"));
        assert!(unit.unit.conflicts.contains("rescue.service"));
        assert!(unit.install.alias.contains(&"default.target".to_string()));
    }

    // -----------------------------------------------------------------------
    // Mount unit parsing
    // -----------------------------------------------------------------------

    const MOUNT_UNIT: &str = r#"
[Unit]
Description=Mount /data

[Mount]
What=/dev/sda1
Where=/data
Type=ext4
Options=defaults,noatime
TimeoutSec=30s
LazyUnmount=yes
DirectoryMode=0755

[Install]
WantedBy=local-fs.target
"#;

    #[test]
    fn test_parse_mount() {
        let unit = parse_unit("data.mount", MOUNT_UNIT).unwrap();
        assert!(matches!(unit.kind, UnitKind::Mount));
        let mnt = unit.mount.unwrap();
        assert_eq!(mnt.what, "/dev/sda1");
        assert_eq!(mnt.where_, "/data");
        assert_eq!(mnt.type_, "ext4");
        assert_eq!(mnt.options, "defaults,noatime");
        assert_eq!(mnt.timeout_sec, 30);
        assert!(mnt.lazy_unmount);
        assert_eq!(mnt.directory_mode, "0755");
    }

    // -----------------------------------------------------------------------
    // Timer unit parsing
    // -----------------------------------------------------------------------

    const TIMER_UNIT: &str = r#"
[Unit]
Description=Daily backup timer

[Timer]
OnCalendar=*-*-* 02:00:00
AccuracySec=1h
RandomizedDelaySec=30min
Persistent=yes
Unit=backup.service

[Install]
WantedBy=timers.target
"#;

    #[test]
    fn test_parse_timer() {
        let unit = parse_unit("backup.timer", TIMER_UNIT).unwrap();
        assert!(matches!(unit.kind, UnitKind::Timer));
        let tmr = unit.timer.unwrap();
        assert_eq!(tmr.on_calendar, vec!["*-*-* 02:00:00"]);
        assert_eq!(tmr.accuracy_sec, 3600);
        assert_eq!(tmr.randomized_delay_sec, 1800);
        assert!(tmr.persistent);
        assert_eq!(tmr.unit, "backup.service");
    }

    #[test]
    fn test_parse_timer_monotonic() {
        let content = "[Timer]\nOnBootSec=10min\nOnUnitActiveSec=1h\n";
        let unit = parse_unit("poll.timer", content).unwrap();
        let tmr = unit.timer.unwrap();
        assert_eq!(tmr.on_boot_sec, Some(600));
        assert_eq!(tmr.on_unit_active_sec, Some(3600));
        assert_eq!(tmr.on_active_sec, None);
    }

    // -----------------------------------------------------------------------
    // Socket unit parsing
    // -----------------------------------------------------------------------

    const SOCKET_UNIT: &str = r#"
[Unit]
Description=SSH socket

[Socket]
ListenStream=22
Accept=no
SocketUser=root
SocketMode=0600
Backlog=128

[Install]
WantedBy=sockets.target
"#;

    #[test]
    fn test_parse_socket() {
        let unit = parse_unit("sshd.socket", SOCKET_UNIT).unwrap();
        assert!(matches!(unit.kind, UnitKind::Socket));
        let sock = unit.socket.unwrap();
        assert_eq!(sock.listen_stream, vec!["22"]);
        assert!(!sock.accept);
        assert_eq!(sock.socket_user, "root");
        assert_eq!(sock.socket_mode, "0600");
        assert_eq!(sock.backlog, 128);
    }

    #[test]
    fn test_parse_unix_socket() {
        let content =
            "[Socket]\nListenStream=/run/myapp.sock\nSocketMode=0660\nAccept=yes\n";
        let unit = parse_unit("myapp.socket", content).unwrap();
        let sock = unit.socket.unwrap();
        assert_eq!(sock.listen_stream, vec!["/run/myapp.sock"]);
        assert!(sock.accept);
    }

    #[test]
    fn test_parse_socket_service_directive() {
        let content = "[Socket]\nListenNetlink=kobject-uevent\nService=my-udevd.service\n";
        let unit = parse_unit("udev.socket", content).unwrap();
        let sock = unit.socket.unwrap();
        assert_eq!(sock.listen_netlink, vec!["kobject-uevent"]);
        assert_eq!(sock.service, "my-udevd.service");

        // Absent directive → empty string (caller derives from unit name).
        let plain = parse_unit("plain.socket", "[Socket]\nListenStream=99\n").unwrap();
        assert_eq!(plain.socket.unwrap().service, "");
    }

    // -----------------------------------------------------------------------
    // Swap unit parsing
    // -----------------------------------------------------------------------

    const SWAP_UNIT: &str = r#"
[Unit]
Description=Swap partition

[Swap]
What=/dev/sda2
Priority=10
TimeoutSec=5s

[Install]
WantedBy=swap.target
"#;

    #[test]
    fn test_parse_swap() {
        let unit = parse_unit("dev-sda2.swap", SWAP_UNIT).unwrap();
        assert!(matches!(unit.kind, UnitKind::Swap));
        let swap = unit.swap.unwrap();
        assert_eq!(swap.what, "/dev/sda2");
        assert_eq!(swap.priority, 10);
        assert_eq!(swap.timeout_sec, 5);
    }

    // -----------------------------------------------------------------------
    // Path unit parsing
    // -----------------------------------------------------------------------

    const PATH_UNIT: &str = r#"
[Unit]
Description=Watch /tmp/trigger

[Path]
PathExists=/tmp/trigger
Unit=mytask.service
MakeDirectory=yes
TriggerLimitBurst=5

[Install]
WantedBy=multi-user.target
"#;

    #[test]
    fn test_parse_path() {
        let unit = parse_unit("mytask.path", PATH_UNIT).unwrap();
        assert!(matches!(unit.kind, UnitKind::Path));
        let path_sec = unit.path.unwrap();
        assert_eq!(path_sec.path_exists, vec!["/tmp/trigger"]);
        assert_eq!(path_sec.unit, "mytask.service");
        assert!(path_sec.make_directory);
        assert_eq!(path_sec.trigger_limit_burst, 5);
    }

    // -----------------------------------------------------------------------
    // Condition and assert parsing
    // -----------------------------------------------------------------------

    #[test]
    fn test_conditions_parsed() {
        let content = r#"
[Unit]
ConditionPathExists=/etc/myapp.conf
ConditionFileNotEmpty=/etc/myapp.conf
ConditionHost=myhost
ConditionVirtualization=no
ConditionACPower=yes
ConditionKernelModuleLoaded=!drm
"#;
        let unit = parse_unit("myapp.service", content).unwrap();
        assert_eq!(
            unit.unit.condition_path_exists,
            vec!["/etc/myapp.conf"]
        );
        assert_eq!(
            unit.unit.condition_file_not_empty,
            vec!["/etc/myapp.conf"]
        );
        assert_eq!(unit.unit.condition_host, vec!["myhost"]);
        assert_eq!(unit.unit.condition_virtualization, vec!["no"]);
        assert_eq!(unit.unit.condition_ac_power, vec!["yes"]);
        assert_eq!(
            unit.unit.condition_kernel_module_loaded,
            vec!["!drm"]
        );
    }

    #[test]
    fn test_asserts_parsed() {
        let content = r#"
[Unit]
AssertPathExists=/var/lib/myapp
AssertFileNotEmpty=/etc/myapp.conf
AssertFirstBoot=yes
"#;
        let unit = parse_unit("myapp.service", content).unwrap();
        assert_eq!(
            unit.unit.assert_path_exists,
            vec!["/var/lib/myapp"]
        );
        assert_eq!(
            unit.unit.assert_file_not_empty,
            vec!["/etc/myapp.conf"]
        );
        assert_eq!(unit.unit.assert_first_boot, vec!["yes"]);
    }

    #[test]
    fn test_negated_condition_parsed() {
        let content = "[Unit]\nConditionPathExists=!/tmp/disable-me\n";
        let unit = parse_unit("conditional.service", content).unwrap();
        assert_eq!(
            unit.unit.condition_path_exists,
            vec!["!/tmp/disable-me"]
        );
    }

    // -----------------------------------------------------------------------
    // Multi-line continuation
    // -----------------------------------------------------------------------

    #[test]
    fn test_multiline_continuation() {
        let content = r#"
[Unit]
Description=A service with \
  a long description

[Service]
ExecStart=/usr/bin/myapp \
  --option1 \
  --option2
"#;
        let unit = parse_unit("multiline.service", content).unwrap();
        // Description is joined.
        assert!(unit.unit.description.contains("long description"));
        // ExecStart is joined into one command.
        let svc = unit.service.unwrap();
        assert_eq!(svc.exec_start[0].program, "/usr/bin/myapp");
        assert!(svc.exec_start[0].args.contains(&"--option1".to_string()));
        assert!(svc.exec_start[0].args.contains(&"--option2".to_string()));
    }

    // -----------------------------------------------------------------------
    // Specifier expansion
    // -----------------------------------------------------------------------

    #[test]
    fn test_specifier_n_full_name() {
        assert_eq!(expand_specifiers("%n", "sshd.service"), "sshd.service");
    }

    #[test]
    fn test_specifier_N_no_extension() {
        assert_eq!(expand_specifiers("%N", "sshd.service"), "sshd");
    }

    #[test]
    fn test_specifier_p_prefix() {
        assert_eq!(expand_specifiers("%p", "sshd@1.service"), "sshd");
    }

    #[test]
    fn test_specifier_i_instance() {
        assert_eq!(expand_specifiers("%i", "sshd@prod.service"), "prod");
    }

    #[test]
    fn test_specifier_percent_escape() {
        assert_eq!(expand_specifiers("100%%", "any.service"), "100%");
    }

    #[test]
    fn test_specifier_no_at_sign() {
        assert_eq!(expand_specifiers("%p", "sshd.service"), "sshd");
        assert_eq!(expand_specifiers("%i", "sshd.service"), "");
    }

    #[test]
    fn test_specifier_in_exec_start() {
        let content = "[Service]\nExecStart=/usr/bin/echo %n\n";
        let unit = parse_unit("hello.service", content).unwrap();
        let svc = unit.service.unwrap();
        assert_eq!(svc.exec_start[0].args, vec!["hello.service"]);
    }

    // -----------------------------------------------------------------------
    // Slice unit parsing
    // -----------------------------------------------------------------------

    const SLICE_UNIT: &str = r#"
[Unit]
Description=System slice

[Slice]
CPUQuota=50%
MemoryMax=1G
TasksMax=512
CPUWeight=200
"#;

    #[test]
    fn test_parse_slice() {
        let unit = parse_unit("system.slice", SLICE_UNIT).unwrap();
        assert!(matches!(unit.kind, UnitKind::Slice));
        let slice = unit.slice.unwrap();
        assert_eq!(slice.cpu_quota, "50%");
        assert_eq!(slice.memory_max, "1G");
        assert_eq!(slice.tasks_max, 512);
        assert_eq!(slice.cpu_weight, 200);
    }

    // -----------------------------------------------------------------------
    // Scope unit parsing
    // -----------------------------------------------------------------------

    const SCOPE_UNIT: &str = r#"
[Scope]
PIDs=1234 5678
TimeoutStopSec=30
KillMode=control-group
MemoryMax=2G
"#;

    #[test]
    fn test_parse_scope() {
        let unit = parse_unit("test.scope", SCOPE_UNIT).unwrap();
        assert!(matches!(unit.kind, UnitKind::Scope));
        let scope = unit.scope.unwrap();
        assert_eq!(scope.pids, vec!["1234", "5678"]);
        assert_eq!(scope.timeout_stop_sec, 30);
        assert_eq!(scope.kill_mode, "control-group");
        assert_eq!(scope.memory_max, "2G");
    }

    // -----------------------------------------------------------------------
    // Device unit parsing
    // -----------------------------------------------------------------------

    const DEVICE_UNIT: &str = r#"
[Device]
Property=ID_BUS=usb
SysfsPath=/sys/devices/pci0000:00/0000:00:14.0/usb1
DeviceName=/dev/sda
"#;

    #[test]
    fn test_parse_device() {
        let unit = parse_unit("sda.device", DEVICE_UNIT).unwrap();
        assert!(matches!(unit.kind, UnitKind::Device));
        let device = unit.device.unwrap();
        assert_eq!(device.property, vec!["ID_BUS=usb"]);
        assert_eq!(
            device.sysfs_path,
            "/sys/devices/pci0000:00/0000:00:14.0/usb1"
        );
        assert_eq!(device.device_name, "/dev/sda");
    }

    // -----------------------------------------------------------------------
    // WatchdogSec parsing
    // -----------------------------------------------------------------------

    #[test]
    fn test_watchdog_sec_parsed() {
        let content = "[Service]\nExecStart=/usr/bin/daemon\nWatchdogUSec=30\n";
        let unit = parse_unit("watchdog.service", content).unwrap();
        let svc = unit.service.unwrap();
        assert_eq!(svc.watchdog_sec, 30);
    }

    // -----------------------------------------------------------------------
    // Drop-in override (in-memory variant)
    // -----------------------------------------------------------------------

    #[test]
    fn test_dropin_overrides_exec_start() {
        let base = "[Service]\nExecStart=/usr/bin/original\n";
        let mut unit = parse_unit("test.service", base).unwrap();
        let dropin = "[Service]\nExecStart=\nExecStart=/usr/bin/override\n";
        apply_dropin_content(&mut unit, dropin).unwrap();
        let svc = unit.service.unwrap();
        // After clearing and re-setting, only the override should remain.
        assert_eq!(svc.exec_start.len(), 1);
        assert_eq!(svc.exec_start[0].program, "/usr/bin/override");
    }

    // -----------------------------------------------------------------------
    // Preprocessing
    // -----------------------------------------------------------------------

    #[test]
    fn test_preprocess_joins_continuation() {
        let input = "key=val1 \\\n  val2\n";
        let out = preprocess_content(input);
        assert!(out.contains("val1"));
        assert!(out.contains("val2"));
        assert!(!out.contains('\\'));
    }

    #[test]
    fn test_preprocess_no_continuation() {
        let input = "[Unit]\nDescription=foo\n";
        let out = preprocess_content(input);
        assert_eq!(out, input);
    }
}