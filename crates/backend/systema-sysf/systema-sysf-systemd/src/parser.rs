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

use std::collections::{HashMap, HashSet};
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
    parse_unit_from_path_as(path, name)
}

/// Parse the unit file at `path` as if it were named `name`.
///
/// This is used for template instantiation: the file on disk is a template
/// (e.g. `getty@.service`) but it is being loaded as an instance
/// (e.g. `getty@tty3.service`), so `%i`/`%p`/`%n` specifiers are expanded
/// with the instance name and the resulting unit is named accordingly.
///
/// Drop-ins are applied from both the instance drop-in directory
/// (`<dir>/<name>.d/`) and, when `name` is an instance unit, the template
/// drop-in directory (`<dir>/<template>.d/`), mirroring systemd's
/// `unit_find_dropin_paths()`.
pub fn parse_unit_from_path_as(path: &Path, name: &str) -> Result<UnitFile> {
    let content = std::fs::read_to_string(path).with_context(|| {
        sysa::l10n::fmt(
            sysa::l10n::t_("Reading unit file {path} ..."),
            &[("path", &path.display().to_string())],
        )
    })?;
    let mut unit = parse_unit_content(name, &content)?;

    // Apply drop-in files from `<dir>/<name>.d/*.conf`.
    let dropin_dir = path.with_file_name(format!("{}.d", name));
    if dropin_dir.is_dir() {
        apply_dropin_dir(&dropin_dir, &mut unit)?;
    }

    // For instance units, also apply the template drop-in directory so that
    // configuration shared by every instance of the template is picked up.
    if let Some(template) = sysa::unit_name::template_of(name) {
        let tpl_dropin_dir = path.with_file_name(format!("{}.d", template));
        if tpl_dropin_dir.is_dir() {
            apply_dropin_dir(&tpl_dropin_dir, &mut unit)?;
        }
    }

    Ok(unit)
}

/// Apply all `*.conf` drop-in files from `dir` to `unit`, in lexicographic
/// order (higher sort = higher priority, overriding earlier entries).
fn apply_dropin_dir(dir: &Path, unit: &mut UnitFile) -> Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| {
            sysa::l10n::fmt(
                sysa::l10n::t_("Reading drop-in dir {dir} ..."),
                &[("dir", &dir.display().to_string())],
            )
        })?
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
/// Build an INI parser configured for systemd unit-file semantics.  The
/// configparser default treats `;` and `#` as inline comment markers that
/// truncate a value anywhere in the line; systemd only recognises them at
/// the start of a line, so inline comments must be disabled or a value like
/// `ExecStart=-/bin/sh -c 'if [ -f /x ]; then …; fi'` is silently cut at the
/// first `;` (single-quoted shell syntax is not an INI comment).
fn unit_ini() -> Ini {
    let mut config = Ini::new();
    config.set_inline_comment_symbols(Some(&[]));
    config
}

fn apply_dropin_content(unit: &mut UnitFile, content: &str) -> Result<()> {
    let processed = merge_append_keys(&preprocess_content(content));
    let mut config = unit_ini();
    config.read(processed.clone()).map_err(|e| {
        anyhow::anyhow!(sysa::l10n::fmt(
            sysa::l10n::t_("INI parse error in drop-in: {e}."),
            &[("e", &e.to_string())]
        ))
    })?;

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
                let exec_lines = collect_key_lines(&processed, "service", "execstart");
                for val in exec_lines {
                    if val.is_empty() {
                        svc.exec_start.clear();
                    } else {
                        svc.exec_start
                            .push(ExecCommand::parse(expand_specifiers(&val, &unit.name)));
                    }
                }

                // Environment= / EnvironmentFile= append to the accumulated lists.
                for val in collect_key_lines(&processed, "service", "environment") {
                    if !val.is_empty() {
                        svc.environment.push(expand_specifiers(&val, &unit.name));
                    }
                }
                for val in collect_key_lines(&processed, "service", "environmentfile") {
                    if !val.is_empty() {
                        svc.environment_file
                            .push(expand_specifiers(&val, &unit.name));
                    }
                }

                // Scalar keys in drop-ins override the base value. Unlike the
                // full re-parse used for other sections, these are applied
                // individually so keys absent from the drop-in do not reset
                // the base values back to their defaults.
                if let Some(v) = config.get("service", "ttypath") {
                    svc.tty_path = expand_specifiers(&v, &unit.name);
                }
                if let Some(v) = config.get("service", "standardinput") {
                    svc.standard_input = v;
                }
                if let Some(v) = config.get("service", "standardoutput") {
                    svc.standard_output = v;
                }
                if let Some(v) = config.get("service", "standarderror") {
                    svc.standard_error = v;
                }

                // Resource-control directives override individually so keys
                // absent from the drop-in keep the base values.
                apply_dropin_resource_control(&config, "service", &mut svc.rc);
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
        UnitKind::Automount => {
            if config.get("automount", "where").is_some()
                || config.get("automount", "extraoptions").is_some()
                || config.get("automount", "timeoutidlesec").is_some()
            {
                let mut amt = unit.automount.take().unwrap_or_default();
                parse_automount_section(&config, &mut amt, &unit.name)?;
                unit.automount = Some(amt);
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
            if config.get("path", "pathexists").is_some() || config.get("path", "unit").is_some() {
                let mut path_sec = unit.path.take().unwrap_or_default();
                parse_path_section(&config, &mut path_sec, &unit.name)?;
                unit.path = Some(path_sec);
            }
        }
        UnitKind::Slice => {
            // [Slice] only carries resource-control directives, applied
            // per-key so drop-ins override exactly what they mention.
            if let Some(ref mut slice) = unit.slice {
                apply_dropin_resource_control(&config, "slice", &mut slice.rc);
            }
        }
        UnitKind::Scope => {
            if config.get("scope", "pids").is_some()
                || config.get("scope", "timeoutstopsec").is_some()
                || config.get("scope", "runtimemaxsec").is_some()
                || config.get("scope", "killmode").is_some()
            {
                let mut scope = unit.scope.take().unwrap_or_default();
                parse_scope_section(&config, &mut scope, &unit.name)?;
                unit.scope = Some(scope);
            }
            if let Some(ref mut scope) = unit.scope {
                apply_dropin_resource_control(&config, "scope", &mut scope.rc);
            }
        }
        UnitKind::Device
            if (config.get("device", "property").is_some()
                || config.get("device", "sysfspath").is_some()) =>
        {
            let mut device = unit.device.take().unwrap_or_default();
            parse_device_section(&config, &mut device, &unit.name)?;
            unit.device = Some(device);
        }
        _ => {}
    }

    Ok(())
}

/// Scan raw (preprocessed) INI content for all values of a key in `section`,
/// preserving order and including empty values (which configparser drops).
fn collect_key_lines(content: &str, section: &str, key: &str) -> Vec<String> {
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

/// Merge repeated list-valued keys (e.g. `Wants=`) into a single assignment.
///
/// systemd appends the values of certain keys across repeated assignments
/// (two `Wants=` lines accumulate), while configparser retains only the last
/// occurrence, silently dropping the earlier values. This pass rewrites the
/// append-only keys so no values are lost; every other key is left untouched
/// (configparser's last-wins behaviour already matches systemd for scalars).
fn merge_append_keys(content: &str) -> String {
    let mut section = String::new();
    let mut out: Vec<String> = Vec::new();
    // (section, key) -> (output line index of the first occurrence, values).
    let mut merged: HashMap<(String, String), (usize, Vec<String>)> = HashMap::new();

    for raw in content.lines() {
        let trimmed = raw.trim();
        if let Some(rest) = trimmed.strip_prefix('[') {
            if let Some(close) = rest.find(']') {
                section = rest[..close].trim().to_lowercase();
                out.push(raw.to_string());
                continue;
            }
        }
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            out.push(raw.to_string());
            continue;
        }
        if let Some(eq) = trimmed.find('=') {
            let key = trimmed[..eq].trim().to_lowercase();
            let value = trimmed[eq + 1..].trim().to_string();
            if is_append_key(&section, &key) {
                match merged.get_mut(&(section.clone(), key.clone())) {
                    Some((_, values)) => values.push(value),
                    None => {
                        let idx = out.len();
                        merged.insert((section.clone(), key), (idx, vec![value]));
                        // Placeholder line, replaced by the merged assignment
                        // once the section is fully scanned.
                        out.push(String::new());
                    }
                }
                continue;
            }
        }
        out.push(raw.to_string());
    }

    for ((_section, key), (idx, values)) in merged {
        out[idx] = format!("{key} = {}", values.join(" "));
    }
    out.join("\n")
}

/// Whether systemd appends the values of `key` across repeated assignments
/// within `section` (as opposed to scalar keys where the last one wins).
fn is_append_key(section: &str, key: &str) -> bool {
    match section {
        "unit" => match key {
            "documentation" | "requires" | "wants" | "conflicts" | "after" | "before"
            | "partof" | "bindsto" | "requisite" | "upholds" | "onsuccess" | "onfailure"
            | "propagatesreloadto" | "propagatesstopto" | "requiresmountsfor"
            | "wantsmountsfor" => true,
            k if k.starts_with("condition") || k.starts_with("assert") => true,
            _ => false,
        },
        "install" => matches!(key, "wantedby" | "requiredby" | "also" | "alias"),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Core parser
// ---------------------------------------------------------------------------

fn parse_unit_content(name: &str, content: &str) -> Result<UnitFile> {
    let processed = merge_append_keys(&preprocess_content(content));
    let mut config = unit_ini(); // case-insensitive (normalizes to lowercase)
    config.read(processed).map_err(|e| {
        anyhow::anyhow!(sysa::l10n::fmt(
            sysa::l10n::t_("INI parse error in {name}: {e}."),
            &[("name", name), ("e", &e.to_string())]
        ))
    })?;

    let mut unit = UnitFile::new(name);

    // --- [Unit] section ---
    parse_unit_section(&config, &mut unit.unit, name).with_context(|| {
        sysa::l10n::fmt(
            sysa::l10n::t_("Parsing [Unit] section of {name} ..."),
            &[("name", name)],
        )
    })?;

    // --- [Install] section ---
    parse_install_section(&config, &mut unit.install).with_context(|| {
        sysa::l10n::fmt(
            sysa::l10n::t_("Parsing [Install] section of {name} ..."),
            &[("name", name)],
        )
    })?;

    // --- type-specific sections ---
    match &unit.kind {
        UnitKind::Service => {
            let mut svc = ServiceSection::default();
            parse_service_section(&config, &mut svc, name).with_context(|| {
                sysa::l10n::fmt(
                    sysa::l10n::t_("Parsing [Service] section of {name} ..."),
                    &[("name", name)],
                )
            })?;
            // systemd service_verify(): a Type=dbus service without BusName=
            // is refused at load time, before anything is spawned.
            if svc.service_type == ServiceType::Dbus && svc.bus_name.is_empty() {
                anyhow::bail!(sysa::l10n::t_(
                    "Service is of type D-Bus but no D-Bus service name has been specified. Refusing."
                ));
            }
            unit.service = Some(svc);
        }
        UnitKind::Target => {
            // Targets have no dedicated section beyond [Unit].
        }
        UnitKind::Mount => {
            let mut mnt = MountSection::default();
            parse_mount_section(&config, &mut mnt, name).with_context(|| {
                sysa::l10n::fmt(
                    sysa::l10n::t_("Parsing [Mount] section of {name} ..."),
                    &[("name", name)],
                )
            })?;
            unit.mount = Some(mnt);
        }
        UnitKind::Automount => {
            let mut amt = AutomountSection::default();
            parse_automount_section(&config, &mut amt, name).with_context(|| {
                sysa::l10n::fmt(
                    sysa::l10n::t_("Parsing [Automount] section of {name} ..."),
                    &[("name", name)],
                )
            })?;
            unit.automount = Some(amt);
        }
        UnitKind::Timer => {
            let mut tmr = TimerSection::default();
            parse_timer_section(&config, &mut tmr, name).with_context(|| {
                sysa::l10n::fmt(
                    sysa::l10n::t_("Parsing [Timer] section of {name} ..."),
                    &[("name", name)],
                )
            })?;
            unit.timer = Some(tmr);
        }
        UnitKind::Socket => {
            let mut sock = SocketSection::default();
            parse_socket_section(&config, &mut sock, name).with_context(|| {
                sysa::l10n::fmt(
                    sysa::l10n::t_("Parsing [Socket] section of {name} ..."),
                    &[("name", name)],
                )
            })?;
            unit.socket = Some(sock);
        }
        UnitKind::Swap => {
            let mut swap = SwapSection::default();
            parse_swap_section(&config, &mut swap, name).with_context(|| {
                sysa::l10n::fmt(
                    sysa::l10n::t_("Parsing [Swap] section of {name} ..."),
                    &[("name", name)],
                )
            })?;
            unit.swap = Some(swap);
        }
        UnitKind::Path => {
            let mut path_sec = PathSection::default();
            parse_path_section(&config, &mut path_sec, name).with_context(|| {
                sysa::l10n::fmt(
                    sysa::l10n::t_("Parsing [Path] section of {name} ..."),
                    &[("name", name)],
                )
            })?;
            unit.path = Some(path_sec);
        }
        UnitKind::Slice => {
            let mut slice = SliceSection::default();
            parse_slice_section(&config, &mut slice, name).with_context(|| {
                sysa::l10n::fmt(
                    sysa::l10n::t_("Parsing [Slice] section of {name} ..."),
                    &[("name", name)],
                )
            })?;
            unit.slice = Some(slice);
        }
        UnitKind::Scope => {
            let mut scope = ScopeSection::default();
            parse_scope_section(&config, &mut scope, name).with_context(|| {
                sysa::l10n::fmt(
                    sysa::l10n::t_("Parsing [Scope] section of {name} ..."),
                    &[("name", name)],
                )
            })?;
            unit.scope = Some(scope);
        }
        UnitKind::Device => {
            let mut device = DeviceSection::default();
            parse_device_section(&config, &mut device, name).with_context(|| {
                sysa::l10n::fmt(
                    sysa::l10n::t_("Parsing [Device] section of {name} ..."),
                    &[("name", name)],
                )
            })?;
            unit.device = Some(device);
        }
        UnitKind::Power => {
            // Power units have no dedicated section beyond [Unit]; the
            // action is encoded in the unit name.
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
/// | `%I`      | Unescaped instance string, e.g. `dev-sda1` → `dev/sda1` |
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
        std::fs::read_to_string(sysa::paths::instance().systemd_machine_id_file)
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|e| {
                tracing::debug!(
                    "expand_specifiers: failed to read {} for %m: {e}",
                    sysa::paths::instance().systemd_machine_id_file
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
            Some('I') => out.push_str(&sysa::unit_name::unescape(instance)),
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
    unit.allow_isolate = get_bool(config, "unit", "allowisolate", false);
    unit.slice = expand(&get_str(config, "unit", "slice"));

    // --- Start rate limiting (systemd v229+: [Unit] section; legacy [Service]) ---
    unit.start_limit_interval_sec = get_u32(
        config,
        "unit",
        "startlimitintervalsec",
        get_u32(config, "service", "startlimitintervalsec", 10),
    );
    unit.start_limit_burst = get_u32(
        config,
        "unit",
        "startlimitburst",
        get_u32(config, "service", "startlimitburst", 5),
    );
    let sla = get_str(config, "unit", "startlimitaction");
    let sla = if sla.is_empty() {
        get_str(config, "service", "startlimitaction")
    } else {
        sla
    };
    unit.start_limit_action = StartLimitAction::from(sla.as_str());

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

    let success_action = get_str(config, "unit", "successaction");
    unit.success_action = SuccessAction::from(success_action.as_str());

    let requires_mounts_for = get_str(config, "unit", "requiresmountsfor");
    if !requires_mounts_for.is_empty() {
        unit.requires_mounts_for
            .extend(split_vec(&expand(&requires_mounts_for)));
    }

    let wants_mounts_for = get_str(config, "unit", "wantsmountsfor");
    if !wants_mounts_for.is_empty() {
        unit.wants_mounts_for
            .extend(split_vec(&expand(&wants_mounts_for)));
    }

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
        unit.condition_virtualization
            .extend(split_vec(&expand(&cv)));
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
        unit.condition_needs_update.extend(split_vec(&expand(&cnu)));
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
    let expand = |v: &str| expand_specifiers(v, name);
    let stype = get_str(config, "service", "type");
    svc.service_type = ServiceType::from(stype.as_str());
    parse_resource_control(config, "service", &mut svc.rc);

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

    svc.working_directory =
        expand_specifiers(&get_str(config, "service", "workingdirectory"), name);
    svc.user = get_str(config, "service", "user");
    svc.group = get_str(config, "service", "group");
    svc.pam_name = get_str(config, "service", "pamname");
    svc.pid_file = get_str(config, "service", "pidfile");
    svc.bus_name = get_str(config, "service", "busname");

    // Sockets= lists socket units for socket activation (distinct from
    // Requires=/BindsTo= which create hard dependencies).
    let sockets = get_str(config, "service", "sockets");
    if !sockets.is_empty() {
        svc.sockets = split_vec(&expand(&sockets));
    }

    // systemd infers Type=dbus when BusName= is set and Type= is not
    // specified (service_verify() in src/core/service.c).
    if !svc.bus_name.is_empty() && stype.is_empty() {
        svc.service_type = ServiceType::Dbus;
    }

    svc.notify_access = get_str(config, "service", "notifyaccess");
    svc.standard_input = get_str(config, "service", "standardinput");
    svc.standard_output = get_str(config, "service", "standardoutput");
    svc.standard_error = get_str(config, "service", "standarderror");
    svc.tty_path = expand_specifiers(&get_str(config, "service", "ttypath"), name);
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

    svc.restart_steps = get_u32(config, "service", "restartsteps", 0);
    svc.restart_max_delay_sec = get_u32(config, "service", "restartmaxdelaysec", 0);

    Ok(())
}

fn parse_mount_section(config: &Ini, mnt: &mut MountSection, name: &str) -> Result<()> {
    mnt.what = expand_specifiers(&get_str(config, "mount", "what"), name);
    mnt.where_ = expand_specifiers(&get_str(config, "mount", "where"), name);
    mnt.type_ = get_str(config, "mount", "type");
    mnt.options = expand_specifiers(&get_str(config, "mount", "options"), name);
    mnt.timeout_sec = get_u32(config, "mount", "timeoutsec", 90);
    mnt.lazy_unmount = get_bool(config, "mount", "lazyunmount", false);
    mnt.force_unmount = get_bool(config, "mount", "forceunmount", false);
    mnt.directory_mode = get_str(config, "mount", "directorymode");
    mnt.sloppy_options = get_bool(config, "mount", "sloppyoptions", false);
    Ok(())
}

fn parse_automount_section(config: &Ini, amt: &mut AutomountSection, name: &str) -> Result<()> {
    amt.where_ = expand_specifiers(&get_str(config, "automount", "where"), name);
    amt.extra_options = expand_specifiers(&get_str(config, "automount", "extraoptions"), name);
    amt.timeout_idle_sec = get_u32(config, "automount", "timeoutidlesec", 0);
    amt.directory_mode = get_str(config, "automount", "directorymode");
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
    path_sec.trigger_limit_interval_sec = get_u32(config, "path", "triggerlimitintervalsec", 2);
    path_sec.trigger_limit_burst = get_u32(config, "path", "triggerlimitburst", 200);
    Ok(())
}

/// Parse the resource-control directives (`systemd.resource-control(5)`)
/// found in `section` into `rc`.
///
/// Used for `[Service]`, `[Slice]`, and `[Scope]`, the three unit sections
/// that own a cgroup and thus accept these directives.
fn parse_resource_control(config: &Ini, section: &str, rc: &mut ResourceControl) {
    rc.cpu_quota = get_str(config, section, "cpuquota");
    rc.cpu_quota_period = get_str(config, section, "cpuquotaperiodsec");
    rc.cpu_weight = get_u32(config, section, "cpuweight", 100);
    rc.startup_cpu_weight = get_u32(config, section, "startupcpuweight", 100);
    rc.cpu_set_cpus = get_str(config, section, "cpusetcpus");
    rc.cpu_set_memory_nodes = get_str(config, section, "cpusetmemorynodes");
    rc.memory_min = get_str(config, section, "memorymin");
    rc.memory_low = get_str(config, section, "memorylow");
    rc.memory_high = get_str(config, section, "memoryhigh");
    rc.memory_max = get_str(config, section, "memorymax");
    rc.memory_swap_max = get_str(config, section, "memoryswapmax");
    rc.io_weight = get_u32(config, section, "ioweight", 100);
    rc.startup_io_weight = get_u32(config, section, "startupioweight", 100);
    rc.io_device_weight = split_vec(&get_str(config, section, "iodeviceweight"));
    rc.io_read_bandwidth_max = split_vec(&get_str(config, section, "ioreadbandwidthmax"));
    rc.io_write_bandwidth_max = split_vec(&get_str(config, section, "iowritebandwidthmax"));
    rc.tasks_max = get_u32(config, section, "tasksmax", u32::MAX);
    rc.allowed_cpus = get_str(config, section, "allowedcpus");
    rc.allowed_memory_nodes = get_str(config, section, "allowedmemorynodes");
}

/// Apply resource-control directives present in a drop-in onto `rc`.
///
/// Mirrors systemd's drop-in semantics: only directives actually listed in
/// the drop-in override the accumulated value; absent ones keep it.
fn apply_dropin_resource_control(config: &Ini, section: &str, rc: &mut ResourceControl) {
    if let Some(v) = config.get(section, "cpuquota") {
        rc.cpu_quota = v.clone();
    }
    if let Some(v) = config.get(section, "cpuquotaperiodsec") {
        rc.cpu_quota_period = v.clone();
    }
    if config.get(section, "cpuweight").is_some() {
        rc.cpu_weight = get_u32(config, section, "cpuweight", 100);
    }
    if config.get(section, "startupcpuweight").is_some() {
        rc.startup_cpu_weight = get_u32(config, section, "startupcpuweight", 100);
    }
    if let Some(v) = config.get(section, "cpusetcpus") {
        rc.cpu_set_cpus = v.clone();
    }
    if let Some(v) = config.get(section, "cpusetmemorynodes") {
        rc.cpu_set_memory_nodes = v.clone();
    }
    if let Some(v) = config.get(section, "memorymin") {
        rc.memory_min = v.clone();
    }
    if let Some(v) = config.get(section, "memorylow") {
        rc.memory_low = v.clone();
    }
    if let Some(v) = config.get(section, "memoryhigh") {
        rc.memory_high = v.clone();
    }
    if let Some(v) = config.get(section, "memorymax") {
        rc.memory_max = v.clone();
    }
    if let Some(v) = config.get(section, "memoryswapmax") {
        rc.memory_swap_max = v.clone();
    }
    if config.get(section, "ioweight").is_some() {
        rc.io_weight = get_u32(config, section, "ioweight", 100);
    }
    if config.get(section, "startupioweight").is_some() {
        rc.startup_io_weight = get_u32(config, section, "startupioweight", 100);
    }
    if let Some(v) = config.get(section, "iodeviceweight") {
        rc.io_device_weight = split_vec(&v);
    }
    if let Some(v) = config.get(section, "ioreadbandwidthmax") {
        rc.io_read_bandwidth_max = split_vec(&v);
    }
    if let Some(v) = config.get(section, "iowritebandwidthmax") {
        rc.io_write_bandwidth_max = split_vec(&v);
    }
    if config.get(section, "tasksmax").is_some() {
        rc.tasks_max = get_u32(config, section, "tasksmax", u32::MAX);
    }
    if let Some(v) = config.get(section, "allowedcpus") {
        rc.allowed_cpus = v.clone();
    }
    if let Some(v) = config.get(section, "allowedmemorynodes") {
        rc.allowed_memory_nodes = v.clone();
    }
}

fn parse_slice_section(config: &Ini, slice: &mut SliceSection, _name: &str) -> Result<()> {
    parse_resource_control(config, "slice", &mut slice.rc);
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
    parse_resource_control(config, "scope", &mut scope.rc);
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
    fn test_parse_dbus_service_requires_bus_name() {
        // systemd service_verify(): Type=dbus without BusName= is refused
        // at load time, before anything is spawned.
        let err = parse_unit(
            "dbus.service",
            r#"
[Service]
Type=dbus
ExecStart=/usr/bin/dbus-daemon
"#,
        )
        .expect_err("Type=dbus without BusName= must be refused at load");
        assert!(
            err.to_string().contains("no D-Bus service name"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_parse_dbus_service_with_bus_name_ok() {
        let unit = parse_unit(
            "dbus.service",
            r#"
[Service]
Type=dbus
BusName=org.freedesktop.DBus
ExecStart=/usr/bin/dbus-daemon --system
"#,
        )
        .expect("Type=dbus with BusName= loads fine");
        let svc = unit.service.unwrap();
        assert!(matches!(svc.service_type, ServiceType::Dbus));
        assert_eq!(svc.bus_name, "org.freedesktop.DBus");
    }

    #[test]
    fn test_busname_implies_type_dbus_when_type_unset() {
        // systemd service_verify(): when Type= is omitted but BusName= is
        // present, the type defaults to Type=dbus.
        let unit = parse_unit(
            "lightdm.service",
            r#"
[Service]
ExecStart=/usr/sbin/lightdm
BusName=org.freedesktop.DisplayManager
"#,
        )
        .expect("BusName= without Type= should infer Type=dbus");
        let svc = unit.service.unwrap();
        assert!(
            matches!(svc.service_type, ServiceType::Dbus),
            "expected Type=dbus from BusName= inference, got {:?}",
            svc.service_type
        );
        assert_eq!(svc.bus_name, "org.freedesktop.DisplayManager");
    }

    #[test]
    fn test_explicit_type_overrides_busname_default() {
        let unit = parse_unit(
            "foo.service",
            r#"
[Service]
Type=simple
ExecStart=/usr/bin/foo
BusName=org.example.Foo
"#,
        )
        .unwrap();
        let svc = unit.service.unwrap();
        assert!(matches!(svc.service_type, ServiceType::Simple));
        assert_eq!(svc.bus_name, "org.example.Foo");
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
    fn test_exec_start_shell_script_with_semicolons_is_not_truncated() {
        // Inline `;` inside single quotes is shell syntax, NOT an INI
        // comment. configparser's default inline-comment handling once cut
        // this value at the first `;`, producing a syntactically invalid
        // `/bin/sh -c 'if [ -f /run/network/restart-hotplug ]'` (networking
        // syntax error, exit code 2).
        let content = r#"[Service]
Type=oneshot
ExecStart=-/bin/sh -c 'if [ -f /run/network/restart-hotplug ]; then /usr/sbin/ifup -a --read-environment --allow=hotplug; fi'
"#;
        let unit = parse_unit("networking.service", content).unwrap();
        let svc = unit.service.unwrap();
        let cmd = &svc.exec_start[0];
        assert!(cmd.ignore_failure);
        assert_eq!(cmd.program, "/bin/sh");
        assert_eq!(cmd.args.len(), 2);
        assert_eq!(cmd.args[0], "-c");
        assert_eq!(
            cmd.args[1],
            "if [ -f /run/network/restart-hotplug ]; then /usr/sbin/ifup -a --read-environment --allow=hotplug; fi"
        );
        assert!(cmd.raw.contains("; then"));
    }

    #[test]
    fn test_whole_line_comments_are_still_ignored() {
        let content = "[Service]\n# comment\n; also a comment\nExecStart=/bin/true\n";
        let unit = parse_unit("foo.service", content).unwrap();
        let svc = unit.service.unwrap();
        assert_eq!(svc.exec_start[0].program, "/bin/true");
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

    // -----------------------------------------------------------------------
    // SuccessAction= parsing (power-transition units)
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_success_action_poweroff_force() {
        // Mirrors the real systemd-poweroff.service on the host: no [Service]
        // section, the power transition rides entirely on SuccessAction=.
        let content = r#"
[Unit]
Description=System Power Off
DefaultDependencies=no
Requires=shutdown.target umount.target final.target
After=shutdown.target umount.target final.target
SuccessAction=poweroff-force
"#;
        let unit = parse_unit("systemd-poweroff.service", content).unwrap();
        // The parser always materialises a `[Service]` section for service
        // units; the point is it carries no ExecStart= (nothing to spawn),
        // which is what makes the unit a no-op on the worker side.
        let svc = unit.service.expect("service section synthesized");
        assert!(svc.exec_start.is_empty());
        assert!(matches!(
            unit.unit.success_action,
            SuccessAction::PoweroffForce
        ));
        assert_eq!(
            unit.unit.success_action.power_unit_name(),
            Some("poweroff.power")
        );
        assert_eq!(unit.unit.success_action.as_str(), "poweroff-force");
    }

    #[test]
    fn test_parse_success_action_all_variants() {
        for (raw, variant) in [
            ("reboot", SuccessAction::Reboot),
            ("reboot-force", SuccessAction::RebootForce),
            ("poweroff-immediate", SuccessAction::PoweroffImmediate),
            ("halt-force", SuccessAction::HaltForce),
            ("kexec", SuccessAction::Kexec),
            ("suspend", SuccessAction::Suspend),
            ("hibernate-force", SuccessAction::HibernateForce),
            ("exit", SuccessAction::Exit),
            ("exit-force", SuccessAction::ExitForce),
        ] {
            let content = format!("[Unit]\nSuccessAction={raw}\n");
            let unit = parse_unit("x.service", &content).unwrap();
            assert_eq!(unit.unit.success_action, variant, "raw={raw}");
        }
        // Unknown values resolve to None (no transition).
        let content = "[Unit]\nSuccessAction=not-a-thing\n";
        let unit = parse_unit("x.service", content).unwrap();
        assert_eq!(unit.unit.success_action, SuccessAction::None);
    }

    #[test]
    fn test_success_action_power_unit_mapping() {
        assert_eq!(
            SuccessAction::Suspend.power_unit_name(),
            Some("suspend.power")
        );
        assert_eq!(
            SuccessAction::Hibernate.power_unit_name(),
            Some("hibernate.power")
        );
        assert_eq!(SuccessAction::None.power_unit_name(), None);
        assert_eq!(SuccessAction::ExitForce.power_unit_name(), None);
    }

    #[test]
    fn test_start_limit_action_power_unit_mapping() {
        // StartLimitAction= transitions funnel through the same `.power`
        // dispatch path as SuccessAction= (System Init executes them via
        // libsystema-sysp).
        assert_eq!(StartLimitAction::Reboot.power_unit_name(), Some("reboot.power"));
        assert_eq!(
            StartLimitAction::RebootForce.power_unit_name(),
            Some("reboot.power")
        );
        assert_eq!(
            StartLimitAction::RebootImmediate.power_unit_name(),
            Some("reboot.power")
        );
        assert_eq!(
            StartLimitAction::Poweroff.power_unit_name(),
            Some("poweroff.power")
        );
        assert_eq!(StartLimitAction::None.power_unit_name(), None);
        assert_eq!(StartLimitAction::Exit.power_unit_name(), None);
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
    // Automount unit parsing
    // -----------------------------------------------------------------------

    const AUTOMOUNT_UNIT: &str = r#"
[Unit]
Description=Automount /data

[Automount]
Where=/data
ExtraOptions=allow_other
TimeoutIdleSec=5min
DirectoryMode=0750

[Install]
WantedBy=local-fs.target
"#;

    #[test]
    fn test_parse_automount() {
        let unit = parse_unit("data.automount", AUTOMOUNT_UNIT).unwrap();
        assert!(matches!(unit.kind, UnitKind::Automount));
        let amt = unit.automount.unwrap();
        assert_eq!(amt.where_, "/data");
        assert_eq!(amt.extra_options, "allow_other");
        assert_eq!(amt.timeout_idle_sec, 300);
        assert_eq!(amt.directory_mode, "0750");
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
        let content = "[Socket]\nListenStream=/run/myapp.sock\nSocketMode=0660\nAccept=yes\n";
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
        assert_eq!(unit.unit.condition_path_exists, vec!["/etc/myapp.conf"]);
        assert_eq!(unit.unit.condition_file_not_empty, vec!["/etc/myapp.conf"]);
        assert_eq!(unit.unit.condition_host, vec!["myhost"]);
        assert_eq!(unit.unit.condition_virtualization, vec!["no"]);
        assert_eq!(unit.unit.condition_ac_power, vec!["yes"]);
        assert_eq!(unit.unit.condition_kernel_module_loaded, vec!["!drm"]);
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
        assert_eq!(unit.unit.assert_path_exists, vec!["/var/lib/myapp"]);
        assert_eq!(unit.unit.assert_file_not_empty, vec!["/etc/myapp.conf"]);
        assert_eq!(unit.unit.assert_first_boot, vec!["yes"]);
    }

    #[test]
    fn test_negated_condition_parsed() {
        let content = "[Unit]\nConditionPathExists=!/tmp/disable-me\n";
        let unit = parse_unit("conditional.service", content).unwrap();
        assert_eq!(unit.unit.condition_path_exists, vec!["!/tmp/disable-me"]);
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
    fn test_specifier_n_no_extension() {
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
    fn test_specifier_i_upper_unescaped() {
        assert_eq!(
            expand_specifiers("%I", "foo@dev\\x2fsda1.service"),
            "dev/sda1"
        );
        assert_eq!(expand_specifiers("%I", "sshd@prod.service"), "prod");
        assert_eq!(expand_specifiers("%I", "sshd.service"), "");
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
        assert_eq!(slice.rc.cpu_quota, "50%");
        assert_eq!(slice.rc.memory_max, "1G");
        assert_eq!(slice.rc.tasks_max, 512);
        assert_eq!(slice.rc.cpu_weight, 200);
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
        assert_eq!(scope.rc.memory_max, "2G");
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

    #[test]
    fn test_dropin_tty_path_overrides() {
        let base = "[Service]\nExecStart=/usr/bin/foo\nTTYPath=/dev/tty1\nStandardInput=tty\n";
        let mut unit = parse_unit("test.service", base).unwrap();
        let dropin = "[Service]\nTTYPath=/dev/tty2\n";
        apply_dropin_content(&mut unit, dropin).unwrap();
        let svc = unit.service.unwrap();
        assert_eq!(svc.tty_path, "/dev/tty2");
        // Keys absent from the drop-in are preserved (no default reset).
        assert_eq!(svc.standard_input, "tty");
        assert_eq!(svc.exec_start[0].program, "/usr/bin/foo");
    }

    #[test]
    fn test_dropin_standard_input_overrides() {
        let base = "[Service]\nExecStart=/usr/bin/foo\nStandardOutput=tty\n";
        let mut unit = parse_unit("test.service", base).unwrap();
        let dropin = "[Service]\nStandardInput=tty\nStandardError=tty\n";
        apply_dropin_content(&mut unit, dropin).unwrap();
        let svc = unit.service.unwrap();
        assert_eq!(svc.standard_input, "tty");
        assert_eq!(svc.standard_error, "tty");
        assert_eq!(svc.standard_output, "tty");
    }

    // -----------------------------------------------------------------------
    // Resource-control parsing
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_service_resource_control() {
        let content = r#"
[Service]
ExecStart=/usr/bin/foo
MemoryMax=1G
MemoryHigh=768M
CPUQuota=50%
CPUWeight=200
TasksMax=512
AllowedCPUs=0-3
"#;
        let unit = parse_unit("foo.service", content).unwrap();
        let svc = unit.service.unwrap();
        assert_eq!(svc.rc.memory_max, "1G");
        assert_eq!(svc.rc.memory_high, "768M");
        assert_eq!(svc.rc.cpu_quota, "50%");
        assert_eq!(svc.rc.cpu_weight, 200);
        assert_eq!(svc.rc.tasks_max, 512);
        assert_eq!(svc.rc.allowed_cpus, "0-3");
        // Keys absent from the section keep parser defaults.
        assert_eq!(svc.rc.cpu_quota_period, "");
        assert_eq!(svc.rc.memory_swap_max, "");
        assert_eq!(svc.rc.io_weight, 100);
    }

    #[test]
    fn test_dropin_resource_control_overrides_only_listed_keys() {
        let base = "[Service]\nExecStart=/usr/bin/foo\nMemoryMax=1G\nCPUQuota=50%\n";
        let mut unit = parse_unit("foo.service", base).unwrap();
        let dropin = "[Service]\nMemoryMax=2G\n";
        apply_dropin_content(&mut unit, dropin).unwrap();
        let svc = unit.service.unwrap();
        // Listed key overrides, absent key keeps its base value.
        assert_eq!(svc.rc.memory_max, "2G");
        assert_eq!(svc.rc.cpu_quota, "50%");
    }

    #[test]
    fn test_dropin_slice_resource_control_overrides() {
        let base = "[Slice]\nCPUQuota=50%\nMemoryMax=1G\n";
        let mut unit = parse_unit("app.slice", base).unwrap();
        let dropin = "[Slice]\nTasksMax=256\n";
        apply_dropin_content(&mut unit, dropin).unwrap();
        let slice = unit.slice.unwrap();
        assert_eq!(slice.rc.tasks_max, 256);
        assert_eq!(slice.rc.memory_max, "1G");
        assert_eq!(slice.rc.cpu_quota, "50%");
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

    // -----------------------------------------------------------------------
    // Template instantiation via parse_unit_from_path_as
    // -----------------------------------------------------------------------

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "systema-sysf-parser-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_parse_template_as_instance_expands_specifiers() {
        let dir = temp_dir("tpl");
        let tpl = dir.join("getty@.service");
        std::fs::write(
            &tpl,
            "[Unit]\nDescription=Getty for %I\n[Service]\nExecStart=/sbin/agetty %i -- %p\n",
        )
        .unwrap();

        let unit = parse_unit_from_path_as(&tpl, "getty@tty3.service").unwrap();
        assert_eq!(unit.name, "getty@tty3.service");
        assert_eq!(unit.unit.description, "Getty for tty3");
        let svc = unit.service.unwrap();
        assert_eq!(svc.exec_start[0].raw, "/sbin/agetty tty3 -- getty");
    }

    #[test]
    fn test_parse_tty_path() {
        let content = "[Service]\nTTYPath=/dev/ttyS0\nStandardInput=tty\nExecStart=/bin/sh\n";
        let unit = parse_unit("serial-getty@ttyS0.service", content).unwrap();
        let svc = unit.service.unwrap();
        assert_eq!(svc.tty_path, "/dev/ttyS0");
        assert_eq!(svc.standard_input, "tty");
    }

    #[test]
    fn test_parse_tty_path_defaults_to_empty() {
        let unit = parse_unit("plain.service", "[Service]\nExecStart=/bin/sh\n").unwrap();
        let svc = unit.service.unwrap();
        // Unset TTYPath/Standard* default to empty; the runtime resolves the
        // default device (/dev/console) only when tty stdio is requested.
        assert_eq!(svc.tty_path, "");
        assert_eq!(svc.standard_input, "");
    }

    #[test]
    fn test_parse_template_tty_path_expands_instance() {
        let dir = temp_dir("tty-tpl");
        let tpl = dir.join("getty@.service");
        std::fs::write(
            &tpl,
            "[Unit]\nDescription=Getty\n[Service]\nExecStart=/sbin/agetty %i\nTTYPath=/dev/%I\nStandardInput=tty\nStandardOutput=tty\n",
        )
        .unwrap();

        let unit = parse_unit_from_path_as(&tpl, "getty@tty3.service").unwrap();
        let svc = unit.service.unwrap();
        assert_eq!(svc.tty_path, "/dev/tty3");
    }

    #[test]
    fn test_parse_template_under_its_own_name() {
        let dir = temp_dir("own");
        let tpl = dir.join("getty@.service");
        std::fs::write(
            &tpl,
            "[Unit]\nDescription=Getty %I\n[Service]\nExecStart=/sbin/agetty %i\n",
        )
        .unwrap();

        // Parsing the template verbatim keeps %i empty.
        let unit = parse_unit_from_path(&tpl).unwrap();
        assert_eq!(unit.name, "getty@.service");
        assert_eq!(unit.unit.description, "Getty ");
    }

    #[test]
    fn test_parse_template_applies_template_dropin() {
        let dir = temp_dir("tpl-dropin");
        let tpl = dir.join("getty@.service");
        std::fs::write(
            &tpl,
            "[Unit]\nDescription=Getty\n[Service]\nExecStart=/sbin/agetty %i\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("getty@.service.d")).unwrap();
        std::fs::write(
            dir.join("getty@.service.d/override.conf"),
            "[Service]\nEnvironment=EXTRA=1\n",
        )
        .unwrap();

        let unit = parse_unit_from_path_as(&tpl, "getty@tty3.service").unwrap();
        let svc = unit.service.unwrap();
        assert!(svc.environment.contains(&"EXTRA=1".to_string()));
    }

    #[test]
    fn test_parse_template_instance_dropin_wins_over_template() {
        let dir = temp_dir("inst-dropin");
        let tpl = dir.join("getty@.service");
        std::fs::write(
            &tpl,
            "[Unit]\nDescription=Getty\n[Service]\nExecStart=/sbin/agetty %i\nEnvironment=BASE=1\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("getty@.service.d")).unwrap();
        std::fs::write(
            dir.join("getty@.service.d/base.conf"),
            "[Service]\nEnvironment=TPL=1\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("getty@tty3.service.d")).unwrap();
        std::fs::write(
            dir.join("getty@tty3.service.d/instance.conf"),
            "[Service]\nEnvironment=INST=1\n",
        )
        .unwrap();

        let unit = parse_unit_from_path_as(&tpl, "getty@tty3.service").unwrap();
        let svc = unit.service.unwrap();
        assert!(svc.environment.contains(&"BASE=1".to_string()));
        assert!(svc.environment.contains(&"TPL=1".to_string()));
        assert!(svc.environment.contains(&"INST=1".to_string()));
    }

    // -----------------------------------------------------------------------
    // [Unit] section defaults
    // -----------------------------------------------------------------------

    #[test]
    fn test_allow_isolate_defaults_to_false() {
        let unit = parse_unit("iso.service", "[Unit]\nDescription=iso\n").unwrap();
        assert!(!unit.unit.allow_isolate);
    }

    #[test]
    fn test_allow_isolate_parsed() {
        let unit = parse_unit("iso.service", "[Unit]\nAllowIsolate=yes\n").unwrap();
        assert!(unit.unit.allow_isolate);
    }

    // -----------------------------------------------------------------------
    // Repeated append-only keys
    // -----------------------------------------------------------------------

    #[test]
    fn test_repeated_wants_are_appended_not_overwritten() {
        let content = "[Unit]\nWants=sockets.target timers.target\nWants=tmp.mount\n";
        let unit = parse_unit("basic.target", content).unwrap();
        assert!(unit.unit.wants.contains("sockets.target"));
        assert!(unit.unit.wants.contains("timers.target"));
        assert!(unit.unit.wants.contains("tmp.mount"));
    }

    #[test]
    fn test_repeated_after_are_appended() {
        let content = "[Unit]\nAfter=a.service\nAfter=b.service\nBefore=c.service\n";
        let unit = parse_unit("multi.service", content).unwrap();
        assert!(unit.unit.after.contains("a.service"));
        assert!(unit.unit.after.contains("b.service"));
        assert!(unit.unit.before.contains("c.service"));
    }

    #[test]
    fn test_repeated_install_keys_are_appended() {
        let content = "[Install]\nWantedBy=multi-user.target\nWantedBy=graphical.target\nAlso=a.service\nAlso=b.service\n";
        let unit = parse_unit("app.service", content).unwrap();
        assert!(unit.install.wanted_by.contains("multi-user.target"));
        assert!(unit.install.wanted_by.contains("graphical.target"));
        assert!(unit.install.also.contains("a.service"));
        assert!(unit.install.also.contains("b.service"));
    }

    #[test]
    fn test_scalar_keys_keep_last_win() {
        // Description= is a scalar: the last assignment must win, untouched
        // by the append-key merge.
        let content = "[Unit]\nDescription=first\nWants=a.service\nDescription=second\n";
        let unit = parse_unit("scalar.service", content).unwrap();
        assert_eq!(unit.unit.description, "second");
        assert!(unit.unit.wants.contains("a.service"));
    }

    #[test]
    fn test_repeated_condition_keys_are_appended() {
        let content =
            "[Unit]\nConditionPathExists=/a\nConditionPathExists=/b\nConditionVirtualization=kvm\n";
        let unit = parse_unit("cond.service", content).unwrap();
        assert_eq!(unit.unit.condition_path_exists, vec!["/a", "/b"]);
        assert_eq!(unit.unit.condition_virtualization, vec!["kvm"]);
    }

    // -----------------------------------------------------------------------
    // Sockets= (socket activation)
    // -----------------------------------------------------------------------

    #[test]
    fn test_sockets_parsed_from_service_section() {
        let content =
            "[Service]\nSockets=systemd-journald.socket systemd-journald-dev-log.socket\n";
        let unit = parse_unit("journald.service", content).unwrap();
        let svc = unit.service.unwrap();
        assert_eq!(svc.sockets.len(), 2);
        assert!(svc.sockets.contains(&"systemd-journald.socket".to_string()));
        assert!(svc
            .sockets
            .contains(&"systemd-journald-dev-log.socket".to_string()));
    }

    #[test]
    fn test_sockets_empty_when_not_set() {
        let content = "[Service]\nExecStart=/bin/echo hello\n";
        let unit = parse_unit("simple.service", content).unwrap();
        let svc = unit.service.unwrap();
        assert!(svc.sockets.is_empty());
    }
}
