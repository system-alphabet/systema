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

/// Parse a unit file from a filesystem path, applying the drop-ins found in
/// its own directory.
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
/// Drop-ins are resolved from the directory that holds the file; callers that
/// know the full unit search path must use [`parse_unit_from_path_as_in`] so
/// that drop-ins from every load path are applied too.
pub fn parse_unit_from_path_as(path: &Path, name: &str) -> Result<UnitFile> {
    let dirs: Vec<String> = path
        .parent()
        .map(|dir| dir.to_string_lossy().into_owned())
        .into_iter()
        .collect();
    parse_unit_from_path_as_in(&dirs, path, name)
}

/// Parse the unit file at `path` as `name`, applying the drop-ins that the
/// unit load path collects.
///
/// `dirs` lists the unit search paths in priority order (highest first), the
/// same shape as `sysa::paths::Paths::unit_search_paths`.  systemd collects
/// `<name>.d/*.conf` from *every* load path, not only from the directory the
/// unit file lives in, so a drop-in in `/etc/systemd/system` extends a unit
/// shipped in `/usr/lib/systemd/system`.  Directories are therefore applied
/// from the lowest priority upwards, letting a higher-priority directory
/// override a lower one.
///
/// An instance unit also inherits its template's drop-ins; within one search
/// directory they are applied before the instance's own, because the instance
/// is the more specific scope.
pub fn parse_unit_from_path_as_in(dirs: &[String], path: &Path, name: &str) -> Result<UnitFile> {
    let content = std::fs::read_to_string(path).with_context(|| {
        sysa::l10n::fmt(
            sysa::l10n::t_("Reading unit file {path} ..."),
            &[("path", &path.display().to_string())],
        )
    })?;
    let mut unit = parse_unit_content(name, &content)?;

    let template = sysa::unit_name::template_of(name);
    for dir in dirs.iter().rev() {
        // The template provides the shared scope; the instance refines it.
        if let Some(template) = &template {
            let tpl_dropin_dir = Path::new(dir).join(format!("{template}.d"));
            if tpl_dropin_dir.is_dir() {
                apply_dropin_dir(&tpl_dropin_dir, &mut unit)?;
            }
        }
        let dropin_dir = Path::new(dir).join(format!("{name}.d"));
        if dropin_dir.is_dir() {
            apply_dropin_dir(&dropin_dir, &mut unit)?;
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
    // Keep the PREPROCESSED text around: the [Unit] reconcile below folds
    // per assignment in file order (empty assignment = reset), so it must
    // read the raw repeated lines — the merged text has already collapsed
    // them, and the reset would be indistinguishable from a value-less
    // union.
    let preprocessed = preprocess_content(content);
    let processed = merge_append_keys(&preprocessed);
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
        // systemd drop-in semantics (systemd.unit(5), "Adding drop-in config
        // snippets"): list-typed dependency settings ACCUMULATE over the main
        // unit file and its drop-ins; an EMPTY assignment RESETS the
        // accumulated list; keys absent from the drop-in keep the base value.
        // `parse_unit_section` rewrites every [Unit] field from the drop-in's
        // config only, so a `[Unit] After=…`-only drop-in erased the base
        // file's Requires=/After= — on the deepin image the
        // user@.service.d/10-login-barrier.conf drop-in reduced user@<uid>
        // to after=[systemd-user-sessions.service] with NO Requires=
        // user-runtime-dir@%i.service edge, the user manager started before
        // its runtime-dir prerequisite and /run/user/<uid> was missing.
        // Snapshot the base section, parse, then reconcile per key below.
        let base = unit.unit.clone();
        parse_unit_section(&config, &mut unit.unit, &unit.name)?;

        let has_unit_key =
            |section: &str, key: &str| !collect_key_lines(&preprocessed, section, key).is_empty();
        for (key, base_set, parsed_set) in [
            ("requires", &base.requires, &mut unit.unit.requires),
            ("wants", &base.wants, &mut unit.unit.wants),
            ("conflicts", &base.conflicts, &mut unit.unit.conflicts),
            ("before", &base.before, &mut unit.unit.before),
            ("after", &base.after, &mut unit.unit.after),
            ("partof", &base.part_of, &mut unit.unit.part_of),
            ("bindsto", &base.binds_to, &mut unit.unit.binds_to),
            ("requisite", &base.requisite, &mut unit.unit.requisite),
            ("upholds", &base.upholds, &mut unit.unit.upholds),
            ("onsuccess", &base.on_success, &mut unit.unit.on_success),
            ("onfailure", &base.on_failure, &mut unit.unit.on_failure),
            (
                "propagatesreloadto",
                &base.propagates_reload_to,
                &mut unit.unit.propagates_reload_to,
            ),
        ] {
            let lines = collect_key_lines(&preprocessed, "unit", key);
            if lines.is_empty() {
                // Key absent from the drop-in: keep the base set.
                *parsed_set = base_set.clone();
                continue;
            }
            // Fold this drop-in's assignments in the file order systemd
            // applies them: empty value ⇒ reset the accumulated list;
            // non-empty ⇒ union its tokens in.  The raw (unmerged) lines
            // are used precisely so the reset stays observable here.
            let mut set = base_set.clone();
            for value in lines {
                if value.is_empty() {
                    set.clear();
                } else {
                    set.extend(split_list(&expand_specifiers(&value, &unit.name)));
                }
            }
            *parsed_set = set;
        }

        // Scalar settings: a drop-in overrides them only when it actually
        // provides the key; otherwise the base value survives (a re-parse
        // from the drop-in-only config would otherwise reset them to their
        // defaults).
        if !has_unit_key("unit", "description") {
            unit.unit.description = base.description.clone();
        }
        if !has_unit_key("unit", "slice") {
            unit.unit.slice = base.slice.clone();
        }
        if !has_unit_key("unit", "defaultdependencies") {
            unit.unit.default_dependencies = base.default_dependencies;
        }
        if !has_unit_key("unit", "allowisolate") {
            unit.unit.allow_isolate = base.allow_isolate;
        }
        if !has_unit_key("unit", "startlimitintervalsec")
            && !has_unit_key("service", "startlimitintervalsec")
        {
            unit.unit.start_limit_interval_sec = base.start_limit_interval_sec;
        }
        if !has_unit_key("unit", "startlimitburst") && !has_unit_key("service", "startlimitburst") {
            unit.unit.start_limit_burst = base.start_limit_burst;
        }
        if !has_unit_key("unit", "startlimitaction") && !has_unit_key("service", "startlimitaction")
        {
            unit.unit.start_limit_action = base.start_limit_action;
        }
        if !has_unit_key("unit", "successaction") {
            unit.unit.success_action = base.success_action;
        }
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

                // Directory directives: repeated directives accumulate and an
                // empty value resets the list (systemd semantics for the
                // directory family); a mode present in the drop-in overrides
                // the base value, one absent keeps it.
                for val in collect_key_lines(&processed, "service", "statedirectory") {
                    if val.is_empty() {
                        svc.state_directory.clear();
                    } else {
                        svc.state_directory
                            .extend(split_vec(&expand_specifiers(&val, &unit.name)));
                    }
                }
                if let Some(v) = config.get("service", "statedirectorymode") {
                    svc.state_directory_mode = parse_dir_mode(v);
                }
                for val in collect_key_lines(&processed, "service", "logsdirectory") {
                    if val.is_empty() {
                        svc.logs_directory.clear();
                    } else {
                        svc.logs_directory
                            .extend(split_vec(&expand_specifiers(&val, &unit.name)));
                    }
                }
                if let Some(v) = config.get("service", "logsdirectorymode") {
                    svc.logs_directory_mode = parse_dir_mode(v);
                }
                for val in collect_key_lines(&processed, "service", "runtimedirectory") {
                    if val.is_empty() {
                        svc.runtime_directory.clear();
                    } else {
                        svc.runtime_directory
                            .extend(split_vec(&expand_specifiers(&val, &unit.name)));
                    }
                }
                if let Some(v) = config.get("service", "runtimedirectorymode") {
                    svc.runtime_directory_mode = parse_dir_mode(v);
                }
                for val in collect_key_lines(&processed, "service", "configurationdirectory") {
                    if val.is_empty() {
                        svc.configuration_directory.clear();
                    } else {
                        svc.configuration_directory
                            .extend(split_vec(&expand_specifiers(&val, &unit.name)));
                    }
                }
                if let Some(v) = config.get("service", "configurationdirectorymode") {
                    svc.configuration_directory_mode = parse_dir_mode(v);
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
            if config.get_map_ref().contains_key("socket") {
                let mut sock = unit.socket.take().unwrap_or_default();
                let mut parsed = SocketSection::default();
                parse_socket_section(&config, &mut parsed, &unit.name)?;
                macro_rules! copy_socket {
                    ($key:literal, $field:ident) => {
                        if config.get("socket", $key).is_some() {
                            sock.$field = parsed.$field;
                        }
                    };
                }
                copy_socket!("accept", accept);
                copy_socket!("service", service);
                copy_socket!("socketuser", socket_user);
                copy_socket!("socketgroup", socket_group);
                copy_socket!("socketmode", socket_mode);
                copy_socket!("directorymode", directory_mode);
                copy_socket!("maxconnections", max_connections);
                copy_socket!("backlog", backlog);
                copy_socket!("bindipv6only", bind_ipv6_only);
                copy_socket!("freebind", free_bind);
                copy_socket!("transparent", transparent);
                copy_socket!("broadcast", broadcast);
                copy_socket!("passcredentials", pass_credentials);
                copy_socket!("passsecurity", pass_security);
                copy_socket!("timeoutsec", timeout_sec);
                apply_socket_list_values(&preprocessed, &mut sock, &unit.name);
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
            if config.get_map_ref().contains_key("path") {
                let mut path_sec = unit.path.take().unwrap_or_default();
                let mut parsed = PathSection::default();
                parse_path_section(&config, &mut parsed, &unit.name)?;
                macro_rules! copy_path {
                    ($key:literal, $field:ident) => {
                        if config.get("path", $key).is_some() {
                            path_sec.$field = parsed.$field;
                        }
                    };
                }
                copy_path!("unit", unit);
                copy_path!("makedirectory", make_directory);
                copy_path!("directorymode", directory_mode);
                copy_path!("triggerlimitintervalsec", trigger_limit_interval_sec);
                copy_path!("triggerlimitburst", trigger_limit_burst);
                apply_path_list_values(&preprocessed, &mut path_sec, &unit.name);
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
        // systemd fold semantics for append-only keys (systemd.unit(5)):
        // an EMPTY assignment RESETS the accumulated list, swallowing the
        // assignments before it; non-empty assignments union their tokens
        // in.  Encode the same result in the merged assignment: only the
        // values after the LAST empty one survive (`After=a\nAfter=\nAfter=b`
        // must collapse to `After=b`, `…After=` alone to an empty reset).
        let surviving: &[String] = match values.iter().rposition(|v| v.is_empty()) {
            Some(last_empty) => &values[last_empty + 1..],
            None => &values[..],
        };
        out[idx] = format!("{key} = {}", surviving.join(" "));
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
    let preprocessed = preprocess_content(content);
    let processed = merge_append_keys(&preprocessed);
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
            apply_socket_list_values(&preprocessed, &mut sock, name);
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
            apply_path_list_values(&preprocessed, &mut path_sec, name);
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
///
/// A template unit name (`getty@.service`) has no instance value, so `%i` and
/// `%I` are kept literal there: the consumer that materializes an instance
/// substitutes them.  `%%` also stays escaped in a template, so that consumer
/// can tell a literal `%` from a specifier it must expand.
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
    // A template definition has no instance value yet: only the consumer that
    // materializes an instance can expand `%i`/`%I`, so a template parse
    // keeps them literal.  Everything else about the name (`%n`, `%p`, ...)
    // is known and is expanded normally.
    let template = sysa::unit_name::is_template(name);

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
    let machine_id = || match sysa::firstboot::read_machine_id() {
        Some(id) => id,
        None => {
            tracing::debug!("expand_specifiers: no machine id file found for %m");
            String::new()
        }
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
            Some('i') if template => out.push_str("%i"),
            Some('i') => out.push_str(instance),
            Some('I') if template => out.push_str("%I"),
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
            Some('%') if template => out.push_str("%%"),
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

/// Parse a `Delegate=` value the way systemd does.
///
/// The directive takes a boolean, or a whitespace-separated controller
/// list: a list enables delegation *and* delegates exactly those
/// controllers (systemd's older `Delegate=<controllers>` spelling of
/// `Delegate=yes` plus `DelegateControllers=`).  An empty value means
/// "yes".  Returns `(delegate, controllers-carried-by-the-list)`; the
/// caller keeps an explicit `DelegateControllers=` over the list.
fn parse_delegate(value: Option<&str>) -> (bool, Option<Vec<String>>) {
    match value.map(str::trim) {
        None => (false, None),
        Some("") | Some("yes") | Some("true") | Some("1") => (true, None),
        Some("no") | Some("false") | Some("0") => (false, None),
        Some(list) => (true, Some(split_vec(list))),
    }
}

/// Parse a directory-mode directive value (`*DirectoryMode=`).
///
/// Octal like the unit file literal (`0700`); an empty/absent value yields
/// 0 (= unset — the worker applies its own 0755 default); anything
/// non-octal likewise collapses to 0 instead of failing the unit, matching
/// how the tolerant scalar getters behave for other keys.
fn parse_dir_mode(value: String) -> u32 {
    if value.is_empty() {
        0
    } else {
        u32::from_str_radix(value.trim(), 8).unwrap_or(0)
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

/// Parse a systemd time span into whole seconds.
///
/// `systemd.time(7)` accepts far more than `Xs`/`Xmin`/`Xh`: `m` is a synonym
/// for minutes, `d`/`w`/`M`/`y` cover days and beyond, and components may be
/// combined (`"1h 30min"`) or fractional (`"1.5h"`).  Only the long spellings
/// used to be recognized, so `OnBootSec=5m` / `OnActiveSec=10m` — the form
/// shipping in real unit files, e.g. `coredump-reporter.timer` — parsed to
/// `None` and left the whole `[Timer]` section trigger-less.
///
/// Sub-second values round up, because the callers store whole seconds and
/// `0` is their "unset, use the default" spelling.
fn parse_time_secs(s: &str) -> Option<u32> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed == "infinity" {
        return Some(u32::MAX);
    }

    let mut total = 0f64;
    for token in trimmed.split_whitespace() {
        total += parse_time_component(token)?;
    }
    let secs = total.ceil();
    if secs >= u32::MAX as f64 {
        return Some(u32::MAX);
    }
    Some(secs as u32)
}

/// One component of a time span (`"30min"`, `"1.5h"`, `"500ms"`, `"45"`), in
/// seconds.  Suffixes are matched longest-first so `min` wins over `m` and
/// `ms` over `m`.
fn parse_time_component(token: &str) -> Option<f64> {
    const UNITS: &[(&str, f64)] = &[
        ("usec", 1e-6),
        ("us", 1e-6),
        ("msec", 1e-3),
        ("ms", 1e-3),
        ("seconds", 1.0),
        ("second", 1.0),
        ("sec", 1.0),
        ("s", 1.0),
        ("minutes", 60.0),
        ("minute", 60.0),
        ("min", 60.0),
        ("m", 60.0),
        ("hours", 3600.0),
        ("hour", 3600.0),
        ("hr", 3600.0),
        ("h", 3600.0),
        ("days", 86_400.0),
        ("day", 86_400.0),
        ("d", 86_400.0),
        ("weeks", 604_800.0),
        ("week", 604_800.0),
        ("w", 604_800.0),
        ("months", 2_629_800.0),
        ("month", 2_629_800.0),
        ("M", 2_629_800.0),
        ("years", 31_557_600.0),
        ("year", 31_557_600.0),
        ("y", 31_557_600.0),
    ];
    for (suffix, factor) in UNITS {
        if let Some(number) = token.strip_suffix(suffix) {
            // Keep looking when the remainder is not a number: `"30minutes"`
            // also matches `s`, and only the long spelling parses.
            if let Ok(value) = number.trim().parse::<f64>() {
                return Some(value * factor);
            }
        }
    }
    // A bare number is seconds (systemd's default unit for these settings).
    token.parse::<f64>().ok()
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
    // Specifiers (%i, %n, ...) are expanded at parse time with the unit's
    // own name, so an instantiated template (user@1000.service parsed from
    // user@.service) yields the instance values (User=1000), like
    // systemd's manager_load_unit_from_path() + unit_full_printf().
    svc.user = expand_specifiers(&get_str(config, "service", "user"), name);
    svc.group = expand_specifiers(&get_str(config, "service", "group"), name);
    svc.pam_name = expand_specifiers(&get_str(config, "service", "pamname"), name);
    svc.pid_file = expand_specifiers(&get_str(config, "service", "pidfile"), name);
    svc.bus_name = expand_specifiers(&get_str(config, "service", "busname"), name);

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

    // Directory directives (systemd.exec(5)): whitespace-separated path
    // fragments below the per-type base (/var/lib, /var/log, /run, /etc);
    // repeated directives accumulate and an empty value resets the list,
    // matching the ExecStart= handling.  Modes stay 0 ("unset") so the
    // worker applies its own default (0755) when the unit does not set one.
    let statedir = get_str(config, "service", "statedirectory");
    if config.get("service", "statedirectory").is_some() {
        if statedir.is_empty() {
            svc.state_directory.clear();
        } else {
            svc.state_directory.extend(split_vec(&statedir));
        }
    }
    svc.state_directory_mode = parse_dir_mode(get_str(config, "service", "statedirectorymode"));
    let logdir = get_str(config, "service", "logsdirectory");
    if config.get("service", "logsdirectory").is_some() {
        if logdir.is_empty() {
            svc.logs_directory.clear();
        } else {
            svc.logs_directory.extend(split_vec(&logdir));
        }
    }
    svc.logs_directory_mode = parse_dir_mode(get_str(config, "service", "logsdirectorymode"));
    let rundir = get_str(config, "service", "runtimedirectory");
    if config.get("service", "runtimedirectory").is_some() {
        if rundir.is_empty() {
            svc.runtime_directory.clear();
        } else {
            svc.runtime_directory.extend(split_vec(&rundir));
        }
    }
    svc.runtime_directory_mode = parse_dir_mode(get_str(config, "service", "runtimedirectorymode"));
    let cfgdir = get_str(config, "service", "configurationdirectory");
    if config.get("service", "configurationdirectory").is_some() {
        if cfgdir.is_empty() {
            svc.configuration_directory.clear();
        } else {
            svc.configuration_directory.extend(split_vec(&cfgdir));
        }
    }
    svc.configuration_directory_mode =
        parse_dir_mode(get_str(config, "service", "configurationdirectorymode"));

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
    path_sec.unit = expand(&get_str(config, "path", "unit"));
    path_sec.make_directory = get_bool(config, "path", "makedirectory", false);
    path_sec.directory_mode = get_str(config, "path", "directorymode");
    path_sec.trigger_limit_interval_sec = get_u32(config, "path", "triggerlimitintervalsec", 2);
    path_sec.trigger_limit_burst = get_u32(config, "path", "triggerlimitburst", 200);
    Ok(())
}

fn apply_list_values(content: &str, section: &str, key: &str, dest: &mut Vec<String>, name: &str) {
    for value in collect_key_lines(content, section, key) {
        if value.is_empty() {
            dest.clear();
        } else {
            dest.push(expand_specifiers(&value, name));
        }
    }
}

fn apply_socket_list_values(content: &str, sock: &mut SocketSection, name: &str) {
    for (key, dest) in [
        ("listenstream", &mut sock.listen_stream),
        ("listendatagram", &mut sock.listen_datagram),
        ("listensequentialpacket", &mut sock.listen_sequential_packet),
        ("listenfifo", &mut sock.listen_fifo),
        ("listennetlink", &mut sock.listen_netlink),
        ("listenspecial", &mut sock.listen_special),
    ] {
        apply_list_values(content, "socket", key, dest, name);
    }
}

fn apply_path_list_values(content: &str, path: &mut PathSection, name: &str) {
    for (key, dest) in [
        ("pathexists", &mut path.path_exists),
        ("pathexistsglob", &mut path.path_exists_glob),
        ("pathchanged", &mut path.path_changed),
        ("pathmodified", &mut path.path_modified),
        ("directorynotempty", &mut path.directory_not_empty),
    ] {
        apply_list_values(content, "path", key, dest, name);
    }
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
    let (delegate, delegate_list) = parse_delegate(config.get(section, "delegate").as_deref());
    rc.delegate = delegate;
    rc.delegate_controllers = split_vec(&get_str(config, section, "delegatecontrollers"));
    if rc.delegate_controllers.is_empty() {
        if let Some(list) = delegate_list {
            rc.delegate_controllers = list;
        }
    }
    rc.delegate_subgroup = get_str(config, section, "delegatesubgroup");
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
    if let Some(v) = config.get(section, "delegate") {
        let (delegate, delegate_list) = parse_delegate(Some(&v));
        rc.delegate = delegate;
        if let Some(list) = delegate_list {
            // The list form carries the controller set too; an explicit
            // DelegateControllers= later in the same drop-in still wins.
            rc.delegate_controllers = list;
        }
    }
    if let Some(v) = config.get(section, "delegatecontrollers") {
        rc.delegate_controllers = split_vec(&v);
    }
    if let Some(v) = config.get(section, "delegatesubgroup") {
        rc.delegate_subgroup = v.clone();
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
        assert_eq!(
            StartLimitAction::Reboot.power_unit_name(),
            Some("reboot.power")
        );
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

    /// `m`/`d` suffixes are the short forms real unit files use.  They used to
    /// parse to `None`, which left the whole timer trigger-less and made
    /// System C refuse it (`coredump-reporter.timer`,
    /// `deepin-appstore-checkappupgrade.timer`).
    #[test]
    fn test_parse_timer_short_time_suffixes() {
        // /usr/lib/systemd/system/coredump-reporter.timer
        let coredump =
            "[Timer]\nUnit=coredump-reporter.service\nOnUnitActiveSec=15m\nOnBootSec=5m\n";
        let unit = parse_unit("coredump-reporter.timer", coredump).unwrap();
        let tmr = unit.timer.unwrap();
        assert_eq!(tmr.on_boot_sec, Some(300));
        assert_eq!(tmr.on_unit_active_sec, Some(900));
        assert_eq!(tmr.unit, "coredump-reporter.service");

        // /usr/lib/systemd/system/deepin-appstore-checkappupgrade.timer
        let appstore = "[Timer]\nOnActiveSec=10m\nOnUnitInactiveSec=1d\n\
                        Persistent=true\nRandomizedDelaySec=1h\n";
        let unit = parse_unit("deepin-appstore-checkappupgrade.timer", appstore).unwrap();
        let tmr = unit.timer.unwrap();
        assert_eq!(tmr.on_active_sec, Some(600));
        assert_eq!(tmr.on_unit_inactive_sec, Some(86_400));
        assert_eq!(tmr.randomized_delay_sec, 3600);
        assert!(tmr.persistent);
    }

    #[test]
    fn time_spans_follow_systemd_suffixes() {
        assert_eq!(parse_time_secs("5m"), Some(300));
        assert_eq!(parse_time_secs("90s"), Some(90));
        assert_eq!(parse_time_secs("1h"), Some(3600));
        assert_eq!(parse_time_secs("1d"), Some(86_400));
        assert_eq!(parse_time_secs("1w"), Some(604_800));
        assert_eq!(parse_time_secs("1h 30min"), Some(5_400));
        assert_eq!(parse_time_secs("1.5h"), Some(5_400));
        assert_eq!(parse_time_secs("30minutes"), Some(1_800));
        // Sub-second values round up: 0 means "unset" for the callers.
        assert_eq!(parse_time_secs("500ms"), Some(1));
        assert_eq!(parse_time_secs("45"), Some(45));
        assert_eq!(parse_time_secs("infinity"), Some(u32::MAX));
        assert_eq!(parse_time_secs("nonsense"), None);
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

    #[test]
    fn socket_and_path_list_assignments_preserve_values_and_dropin_resets() {
        let mut socket = parse_unit(
            "udev.socket",
            "[Socket]\nListenNetlink=kobject-uevent 1\nListenNetlink=route 2\nListenSequentialPacket=@/old\nSocketMode=0600\n",
        ).unwrap();
        assert_eq!(
            socket.socket.as_ref().unwrap().listen_netlink,
            ["kobject-uevent 1", "route 2"]
        );
        apply_dropin_content(
            &mut socket,
            "[Socket]\nListenSequentialPacket=\nListenSequentialPacket=@/new\n",
        )
        .unwrap();
        let socket = socket.socket.unwrap();
        assert_eq!(socket.listen_sequential_packet, ["@/new"]);
        assert_eq!(socket.socket_mode, "0600");
        assert_eq!(socket.listen_netlink, ["kobject-uevent 1", "route 2"]);

        let mut path = parse_unit(
            "acpid.path",
            "[Path]\nPathExists=/old\nUnit=acpid.service\n",
        )
        .unwrap();
        apply_dropin_content(
            &mut path,
            "[Path]\nPathExists=\nPathExists=/new\nPathChanged=/change\n",
        )
        .unwrap();
        let path = path.path.unwrap();
        assert_eq!(path.path_exists, ["/new"]);
        assert_eq!(path.path_changed, ["/change"]);
        assert_eq!(path.unit, "acpid.service");
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

    // Incident regression: the deepin image ships
    // user@.service.d/10-login-barrier.conf containing only
    // `After=systemd-user-sessions.service`.  With replacement semantics the
    // user@<uid> unit lost Requires=user-runtime-dir@%i.service and its other
    // After targets, started before /run/user/<uid> was mounted and the user
    // manager exited without READY=1 (black screen after login).  systemd's
    // drop-in semantics instead ACCUMULATE dependency lists.
    #[test]
    fn test_dropin_dep_lists_accumulate_over_base() {
        let base = concat!(
            "[Unit]\n",
            "After=user-runtime-dir@%i.service dbus.service systemd-oomd.service\n",
            "Requires=user-runtime-dir@%i.service\n",
            "[Service]\nExecStart=/usr/lib/systemd/systemd --user\n",
        );
        let mut unit = parse_unit("user@1000.service", base).unwrap();
        let dropin = "[Unit]\n# login barrier\nAfter=systemd-user-sessions.service\n";
        apply_dropin_content(&mut unit, dropin).unwrap();
        // Every accumulated edge survives:
        assert!(
            unit.unit.after.contains("user-runtime-dir@1000.service"),
            "after lost the base edge: {:?}",
            unit.unit.after
        );
        assert!(unit.unit.after.contains("dbus.service"));
        assert!(unit.unit.after.contains("systemd-oomd.service"));
        assert!(unit.unit.after.contains("systemd-user-sessions.service"));
        assert!(unit.unit.requires.contains("user-runtime-dir@1000.service"));
    }

    // systemd: an EMPTY dependency assignment resets the accumulated list,
    // with later assignments in the same file setting fresh values.
    #[test]
    fn test_dropin_empty_assignment_resets_list() {
        let base = "[Unit]\nAfter=a.service b.service\nRequires=a.service\n";
        let mut unit = parse_unit("reset.service", base).unwrap();
        let dropin = "[Unit]\nAfter=\nAfter=c.service\n";
        apply_dropin_content(&mut unit, dropin).unwrap();
        assert_eq!(unit.unit.after.len(), 1);
        assert!(unit.unit.after.contains("c.service"));
        // Requires untouched by the drop-in keeps its base value.
        assert!(unit.unit.requires.contains("a.service"));
    }

    // The same reset rule applies INSIDE one unit file: repeated
    // assignments accumulate, an empty assignment clears everything before
    // it (`merge_append_keys` must not lose the empty assignment).
    #[test]
    fn test_empty_assignment_within_one_file_resets() {
        // reset in the middle: only the tail survives.
        let unit = parse_unit(
            "m.service",
            "[Unit]\nAfter=a.service\nAfter=\nAfter=b.service\n",
        )
        .unwrap();
        assert_eq!(unit.unit.after.len(), 1);
        assert!(unit.unit.after.contains("b.service"));

        let unit = parse_unit(
            "m2.service",
            "[Unit]\nAfter=a.service\nAfter=b.service\nAfter=\n",
        )
        .unwrap();
        assert!(unit.unit.after.is_empty(), "trailing reset kept values");

        // No empty assignment: repeated values union (merge_append_keys).
        let unit = parse_unit("m3.service", "[Unit]\nAfter=a.service\nAfter=b.service\n").unwrap();
        assert!(unit.unit.after.contains("a.service"));
        assert!(unit.unit.after.contains("b.service"));
    }

    // A drop-in that first RESETS then re-adds must NOT keep the base
    // values: the empty assignment clears base ∪ drop-in, then rebuilds.
    #[test]
    fn test_dropin_reset_then_readd_discards_base() {
        let base = "[Unit]\nAfter=a.service b.service\n";
        let mut unit = parse_unit("rr.service", base).unwrap();
        let dropin = "[Unit]\nAfter=\nAfter=c.service\n";
        apply_dropin_content(&mut unit, dropin).unwrap();
        assert_eq!(unit.unit.after.len(), 1);
        assert!(unit.unit.after.contains("c.service"));
        assert!(!unit.unit.after.contains("a.service"));
    }

    // A [Service]-shaped drop-in must not clobber [Unit] data, and a [Unit]
    //-shaped drop-in must not reset scalars it does not mention.
    #[test]
    fn test_dropin_absent_dep_and_scalars_untouched() {
        let base = concat!(
            "[Unit]\n",
            "Description=User Manager\n",
            "Slice=user-%i.slice\n",
            "After=dbus.service\n",
            "[Service]\nExecStart=/usr/bin/stay\n",
        );
        let mut unit = parse_unit("u@1.service", base).unwrap();
        let deployin = "[Service]\nEnvironment=K=1\n";
        apply_dropin_content(&mut unit, deployin).unwrap();
        assert_eq!(unit.unit.description, "User Manager");
        assert_eq!(unit.unit.slice, "user-1.slice");
        assert!(unit.unit.after.contains("dbus.service"));

        let mut unit = parse_unit("u@2.service", base).unwrap();
        let unitdropin = "[Unit]\nAfter=logind.service\n";
        apply_dropin_content(&mut unit, unitdropin).unwrap();
        assert_eq!(unit.unit.description, "User Manager");
        assert_eq!(unit.unit.slice, "user-2.slice");
        assert!(unit.unit.after.contains("logind.service"));
        assert!(unit.unit.after.contains("dbus.service"));
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
    // Directory directives (systemd.exec(5))
    // -----------------------------------------------------------------------

    #[test]
    fn test_service_directory_directives_parse() {
        let content = "[Service]\n\
                       ExecStart=/usr/bin/foo\n\
                       StateDirectory=foo bar/baz\n\
                       StateDirectoryMode=0700\n\
                       LogsDirectory=deepin\n\
                       LogsDirectoryMode=0750\n";
        let unit = parse_unit("dird.service", content).unwrap();
        let svc = unit.service.unwrap();
        // Whitespace-split fragments; repeated values accumulate.
        assert_eq!(svc.state_directory, vec!["foo", "bar/baz"]);
        assert_eq!(svc.state_directory_mode, 0o700);
        assert_eq!(svc.logs_directory, vec!["deepin"]);
        assert_eq!(svc.logs_directory_mode, 0o750);
        // Untouched families stay empty with mode 0 (= worker default 0755).
        assert!(svc.runtime_directory.is_empty());
        assert_eq!(svc.runtime_directory_mode, 0);
        assert!(svc.configuration_directory.is_empty());
        assert_eq!(svc.configuration_directory_mode, 0);
    }

    #[test]
    fn test_dropin_directory_directives_merge() {
        // The real regression shape: dde-dconfig-daemon declares
        // LogsDirectory=deepin + StateDirectory=dde-dconfig-daemon (Mode=0700)
        // and stock systemd synthesizes $LOGS_DIRECTORY/$STATE_DIRECTORY from
        // them.
        let base = "[Service]\n\
                    ExecStart=/usr/bin/dde-dconfig-daemon\n\
                    LogsDirectory=deepin\n\
                    StateDirectory=dde-dconfig-daemon\n\
                    StateDirectoryMode=0700\n";
        let mut unit = parse_unit("dde-dconfig-daemon.service", base).unwrap();

        // Empty value in a drop-in resets the accumulated list.
        apply_dropin_content(&mut unit, "[Service]\nStateDirectory=\n").unwrap();
        {
            let svc = unit.service.as_ref().unwrap();
            assert!(svc.state_directory.is_empty());
            // Untouched keys keep the base values through the drop-in.
            assert_eq!(svc.logs_directory, vec!["deepin"]);
        }

        // A non-empty drop-in value unions its fragments in.
        apply_dropin_content(&mut unit, "[Service]\nStateDirectory=qux\n").unwrap();
        let svc = unit.service.as_ref().unwrap();
        assert_eq!(svc.state_directory, vec!["qux"]);
        assert_eq!(svc.logs_directory, vec!["deepin"]);
    }

    // -----------------------------------------------------------------------
    // Delegate= (resource-control delegation)
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_delegate_service() {
        let content = "[Service]\nExecStart=/usr/bin/foo\nDelegate=yes\nDelegateControllers=cpu memory pids\nDelegateSubgroup=init.scope\n";
        let unit = parse_unit("deleg.service", content).unwrap();
        let rc = &unit.service.unwrap().rc;
        assert!(rc.delegate);
        assert_eq!(rc.delegate_controllers, vec!["cpu", "memory", "pids"]);
        assert_eq!(rc.delegate_subgroup, "init.scope");
    }

    #[test]
    fn test_delegate_defaults_off() {
        let content = "[Service]\nExecStart=/usr/bin/foo\n";
        let unit = parse_unit("plain.service", content).unwrap();
        let rc = &unit.service.unwrap().rc;
        assert!(!rc.delegate);
        assert!(rc.delegate_controllers.is_empty());
        assert_eq!(rc.delegate_subgroup, "");
    }

    #[test]
    fn test_delegate_controller_list_form() {
        // user@.service's exact shape: a controller list means "delegate
        // with exactly these controllers" — not "false".
        let content = "[Service]\nExecStart=/usr/lib/systemd/systemd --user\nDelegate=pids memory cpu\nDelegateSubgroup=init.scope\n";
        let unit = parse_unit("user@.service", content).unwrap();
        let rc = &unit.service.unwrap().rc;
        assert!(rc.delegate);
        assert_eq!(rc.delegate_controllers, vec!["pids", "memory", "cpu"]);
        assert_eq!(rc.delegate_subgroup, "init.scope");
    }

    #[test]
    fn test_delegate_empty_value_is_yes() {
        let content = "[Service]\nExecStart=/usr/bin/foo\nDelegate=\n";
        let unit = parse_unit("foo.service", content).unwrap();
        let rc = &unit.service.unwrap().rc;
        assert!(rc.delegate);
        assert!(rc.delegate_controllers.is_empty());
    }

    #[test]
    fn test_delegate_list_yields_to_explicit_controllers_key() {
        let content =
            "[Service]\nExecStart=/usr/bin/foo\nDelegate=pids memory\nDelegateControllers=cpu\n";
        let unit = parse_unit("foo.service", content).unwrap();
        let rc = &unit.service.unwrap().rc;
        assert!(rc.delegate);
        assert_eq!(rc.delegate_controllers, vec!["cpu"]);
    }

    #[test]
    fn test_dropin_delegate_overrides() {
        let base = "[Service]\nExecStart=/usr/bin/foo\nDelegate=no\n";
        let mut unit = parse_unit("foo.service", base).unwrap();
        let dropin = "[Service]\nDelegate=yes\nDelegateSubgroup=init.scope\n";
        apply_dropin_content(&mut unit, dropin).unwrap();
        let rc = &unit.service.unwrap().rc;
        assert!(rc.delegate);
        assert_eq!(rc.delegate_subgroup, "init.scope");
    }

    #[test]
    fn test_dropin_delegate_list_form() {
        let base = "[Service]\nExecStart=/usr/bin/foo\nDelegate=no\n";
        let mut unit = parse_unit("foo.service", base).unwrap();
        let dropin = "[Service]\nDelegate=pids memory\n";
        apply_dropin_content(&mut unit, dropin).unwrap();
        let rc = &unit.service.unwrap().rc;
        assert!(rc.delegate);
        assert_eq!(rc.delegate_controllers, vec!["pids", "memory"]);
    }

    #[test]
    fn test_dropin_delegate_absent_keeps_base() {
        let base = "[Service]\nExecStart=/usr/bin/foo\nDelegate=yes\nDelegateControllers=cpu\n";
        let mut unit = parse_unit("foo.service", base).unwrap();
        let dropin = "[Service]\nCPUWeight=200\n";
        apply_dropin_content(&mut unit, dropin).unwrap();
        let rc = &unit.service.unwrap().rc;
        assert!(rc.delegate);
        assert_eq!(rc.delegate_controllers, vec!["cpu"]);
    }

    #[test]
    fn test_specifier_expands_in_user_group() {
        // The systemd user manager instance: %i must resolve per instance,
        // otherwise User=%i arrives at the nss lookup verbatim and the unit
        // can never start.
        let content =
            "[Service]\nExecStart=/usr/lib/systemd/systemd --user\nUser=%i\nGroup=%i\nPIDFile=/run/%n.pid\n";
        let unit = parse_unit("user@1000.service", content).unwrap();
        let svc = unit.service.unwrap();
        assert_eq!(svc.user, "1000");
        assert_eq!(svc.group, "1000");
        assert_eq!(svc.pid_file, "/run/user@1000.service.pid");
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
            "[Unit]\nDescription=Getty %I (100%%)\n[Service]\nExecStart=/sbin/agetty %i\n",
        )
        .unwrap();

        // Parsing the template verbatim keeps %i/%I literal: only a consumer
        // that materializes an instance knows the parameter, so the template
        // stays instantiable instead of silently losing the instance.  A
        // literal `%%` also stays escaped for that later pass.
        let unit = parse_unit_from_path(&tpl).unwrap();
        assert_eq!(unit.name, "getty@.service");
        assert_eq!(unit.unit.description, "Getty %I (100%%)");
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
