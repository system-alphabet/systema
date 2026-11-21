//! `[Install]`-driven unit-file operations.

use std::collections::{HashSet, VecDeque};
use std::os::unix::fs::symlink;
use std::path::Path;

use super::parse::{expand_specifiers, replace_instance};
use super::preset::{self, PresetAction, PresetMode};
use super::{
    instance_of, is_template, is_valid_unit_name, normalize, resolve, resolve_relative,
    template_of, unit_exists, Changes, LookupPaths, ResolvedUnit,
};

/// Flags shared by the operations.
#[derive(Debug, Clone)]
pub struct Options {
    /// Operate on the runtime configuration directory.
    pub runtime: bool,
    /// Replace conflicting links/files (`--force`).
    pub force: bool,
    /// Preset execution skips vendor-provided links and dependency masks.
    pub applying_preset: bool,
    /// Preset execution ignores failures of `Also=` units.
    pub ignore_auxiliary_failure: bool,
}

impl Options {
    pub fn new(runtime: bool, force: bool) -> Self {
        Self {
            runtime,
            force,
            applying_preset: false,
            ignore_auxiliary_failure: false,
        }
    }

    fn config<'a>(&self, lp: &'a LookupPaths) -> &'a Path {
        lp.config_path(self.runtime)
    }
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn file_name_text(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn unit_suffix(name: &str) -> &str {
    name.rfind('.').map(|dot| &name[dot..]).unwrap_or("")
}

fn invalid_name(name: &str) -> String {
    sysa::l10n::fmt(
        sysa::l10n::t_("Invalid unit name {name}."),
        &[("name", name)],
    )
}

fn io_error(template: String, path: &Path, error: &std::io::Error) -> String {
    sysa::l10n::fmt(
        &template,
        &[("path", &path.to_string_lossy()), ("e", &error.to_string())],
    )
}

// ---------------------------------------------------------------------------
// Symlink primitives
// ---------------------------------------------------------------------------

/// Create `source → dest`, mirroring systemd's `create_symlink()`.
fn add_symlink(
    dest: &Path,
    source: &Path,
    force: bool,
    changes: &mut Changes,
) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| io_error(sysa::l10n::t_("Failed to create {path}: {e}."), parent, &e))?;
    }
    match std::fs::symlink_metadata(dest) {
        Ok(meta) => {
            if !meta.file_type().is_symlink() {
                return Err(sysa::l10n::fmt(
                    sysa::l10n::t_("File {path} already exists."),
                    &[("path", &path_text(dest))],
                ));
            }
            let existing = std::fs::read_link(dest)
                .map_err(|e| io_error(sysa::l10n::t_("Failed to read {path}: {e}."), dest, &e))?;
            let resolved = resolve_relative(dest, &existing);
            if resolved == normalize(source) {
                return Ok(());
            }
            if !force {
                return Err(sysa::l10n::fmt(
                    sysa::l10n::t_("File {path} already exists."),
                    &[("path", &path_text(dest))],
                ));
            }
            std::fs::remove_file(dest)
                .map_err(|e| io_error(sysa::l10n::t_("Failed to remove {path}: {e}."), dest, &e))?;
            changes.unlink(path_text(dest));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(io_error(
                sysa::l10n::t_("Failed to inspect {path}: {e}."),
                dest,
                &e,
            ))
        }
    }
    symlink(source, dest)
        .map_err(|e| io_error(sysa::l10n::t_("Failed to create {path}: {e}."), dest, &e))?;
    changes.symlink(path_text(dest), path_text(source));
    Ok(())
}

/// Whether an entry is a mask (`/dev/null` symlink or an empty file).
fn is_mask_entry(path: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if meta.file_type().is_symlink() {
        return std::fs::read_link(path)
            .map(|target| resolve_relative(path, &target) == Path::new("/dev/null"))
            .unwrap_or(false);
    }
    meta.is_file() && meta.len() == 0
}

// ---------------------------------------------------------------------------
// Enable
// ---------------------------------------------------------------------------

/// Enable the given units (paths or names).  Returns whether any unit carried
/// install information.
pub fn enable(lp: &LookupPaths, names: &[String], opts: &Options) -> (bool, Changes) {
    let config = opts.config(lp).to_path_buf();
    let mut changes = Changes::default();
    let mut carries = false;
    let mut queue: VecDeque<(String, bool)> =
        names.iter().map(|name| (name.clone(), false)).collect();
    let mut seen = HashSet::new();
    let mut processed = Vec::new();

    while let Some((name, auxiliary)) = queue.pop_front() {
        if !seen.insert(name.clone()) {
            continue;
        }
        let resolved = match resolve(&name, lp) {
            Ok(resolved) => resolved,
            Err(message) => {
                if auxiliary {
                    changes.record("auxiliary unit failed", name, "");
                    continue;
                }
                changes.fail(message);
                return (carries, changes);
            }
        };
        if resolved.masked {
            changes.record("masked", path_text(&resolved.mask_path), "");
            // Assume something could have been enabled here, like systemd,
            // so the "no install information" notice is suppressed.
            carries = true;
            continue;
        }
        match apply_install_links(&resolved, lp, &config, opts, &mut changes) {
            Ok(applied) => carries |= applied,
            Err(message) => {
                changes.fail(message);
                return (carries, changes);
            }
        }
        for also in &resolved.info.also {
            let expanded = expand_specifiers(
                also,
                &resolved.name,
                resolved.info.default_instance.as_deref(),
            );
            queue.push_back((expanded, true));
        }
        processed.push(resolved);
    }

    for unit in &processed {
        unmask_dependencies(&config, unit, &mut changes);
    }

    (carries, changes)
}

fn apply_install_links(
    info: &ResolvedUnit,
    lp: &LookupPaths,
    config: &Path,
    opts: &Options,
    changes: &mut Changes,
) -> Result<bool, String> {
    let mut carries = false;
    carries |= symlink_wants(
        info,
        &info.info.wanted_by,
        ".wants/",
        lp,
        config,
        opts,
        changes,
    )?;
    carries |= symlink_wants(
        info,
        &info.info.required_by,
        ".requires/",
        lp,
        config,
        opts,
        changes,
    )?;
    carries |= symlink_wants(
        info,
        &info.info.upheld_by,
        ".upholds/",
        lp,
        config,
        opts,
        changes,
    )?;
    carries |= symlink_aliases(info, config, opts.force, changes)?;
    Ok(carries)
}

fn symlink_wants(
    info: &ResolvedUnit,
    targets: &[String],
    suffix: &str,
    lp: &LookupPaths,
    config: &Path,
    opts: &Options,
    changes: &mut Changes,
) -> Result<bool, String> {
    if targets.is_empty() {
        return Ok(false);
    }
    let link_name = if !is_template(&info.name) {
        info.name.clone()
    } else if let Some(default) = &info.info.default_instance {
        replace_instance(&info.name, default).ok_or_else(|| invalid_name(&info.name))?
    } else {
        return Err(sysa::l10n::fmt(
            sysa::l10n::t_("Cannot enable template unit {name} without an instance."),
            &[("name", &info.name)],
        ));
    };

    for target in targets {
        let destination =
            expand_specifiers(target, &info.name, info.info.default_instance.as_deref());
        if !is_valid_unit_name(&destination) {
            if opts.ignore_auxiliary_failure {
                continue;
            }
            return Err(invalid_name(&destination));
        }
        let rel = format!("{destination}{suffix}{link_name}");
        if opts.applying_preset && provided_below(lp, config, &rel, &info.path) {
            continue;
        }
        let path = config.join(&rel);
        add_symlink(&path, &info.path, true, changes)?;
        if !unit_exists(lp, &destination) {
            changes.record(
                "destination not present",
                destination,
                path_text(&info.path),
            );
        }
    }
    Ok(true)
}

fn symlink_aliases(
    info: &ResolvedUnit,
    config: &Path,
    force: bool,
    changes: &mut Changes,
) -> Result<bool, String> {
    let mut carries = false;
    for alias in &info.info.aliases {
        let destination =
            expand_specifiers(alias, &info.name, info.info.default_instance.as_deref());
        if destination.contains('/') {
            // Legacy `Alias=target.wants/unit.service` form.
            let Some((dir, entry)) = destination.split_once('/') else {
                return Err(invalid_name(&destination));
            };
            if !(dir.ends_with(".wants") || dir.ends_with(".requires")) {
                return Err(sysa::l10n::fmt(
                    sysa::l10n::t_("Invalid alias path {path}."),
                    &[("path", &destination)],
                ));
            }
            if !is_valid_unit_name(entry) || unit_suffix(entry) != unit_suffix(&info.name) {
                return Err(invalid_name(entry));
            }
            let path = config.join(&destination);
            add_symlink(&path, &info.path, force, changes)?;
            carries = true;
            continue;
        }

        let alias_name = if is_template(&destination) {
            match instance_of(&info.name) {
                Some(instance) => {
                    replace_instance(&destination, &instance).unwrap_or_else(|| destination.clone())
                }
                None => destination.clone(),
            }
        } else {
            destination.clone()
        };
        if !is_valid_unit_name(&alias_name) {
            return Err(invalid_name(&alias_name));
        }
        if unit_suffix(&alias_name) != unit_suffix(&info.name) {
            return Err(sysa::l10n::fmt(
                sysa::l10n::t_("Invalid alias {alias} for unit {name}."),
                &[("alias", &alias_name), ("name", &info.name)],
            ));
        }
        if alias_name == info.name {
            // A self-alias is silently ignored, like systemd.
            continue;
        }
        let path = config.join(&alias_name);
        add_symlink(&path, &info.path, force, changes)?;
        carries = true;
    }
    Ok(carries)
}

/// Whether a lower-priority search path already provides `rel` (preset mode
/// avoids duplicating vendor links).
fn provided_below(lp: &LookupPaths, config: &Path, rel: &str, source: &Path) -> bool {
    let Some(index) = lp.search_paths.iter().position(|dir| dir == config) else {
        return false;
    };
    for dir in lp.search_paths.iter().skip(index + 1) {
        let path = dir.join(rel);
        if let Ok(target) = std::fs::read_link(&path) {
            if resolve_relative(&path, &target) == normalize(source) {
                return true;
            }
        }
    }
    false
}

/// Remove dependency masks that an earlier disable created for this unit.
fn unmask_dependencies(config: &Path, info: &ResolvedUnit, changes: &mut Changes) {
    let mut names = vec![info.name.clone()];
    for alias in &info.info.aliases {
        names.push(expand_specifiers(
            alias,
            &info.name,
            info.info.default_instance.as_deref(),
        ));
    }
    let mut stack = vec![config.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let is_dependency_dir = dir != config
            && dir
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(dependency_suffix)
                .is_some();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                if dependency_suffix(name).is_some() {
                    stack.push(path);
                }
                continue;
            }
            if !is_dependency_dir || !is_mask_entry(&path) {
                continue;
            }
            let entry_name = file_name_text(&path);
            let template = template_of(&entry_name);
            let matched = names
                .iter()
                .any(|n| *n == entry_name || template.as_deref() == Some(n.as_str()));
            if matched && std::fs::remove_file(&path).is_ok() {
                changes.record("unmask-dependency", path_text(&path), "");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Disable
// ---------------------------------------------------------------------------

/// Disable the given units.  Returns whether any unit carried install
/// information (or a symlink was removed).
pub fn disable(lp: &LookupPaths, names: &[String], opts: &Options) -> (bool, Changes) {
    let config = opts.config(lp).to_path_buf();
    let mut changes = Changes::default();
    let mut marked: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = names.iter().cloned().collect();
    let mut seen = HashSet::new();
    let mut infos = Vec::new();
    let mut has_install_info = false;

    while let Some(name) = queue.pop_front() {
        if !seen.insert(name.clone()) {
            continue;
        }
        if !is_valid_unit_name(&name) {
            changes.fail(invalid_name(&name));
            return (false, changes);
        }
        marked.insert(name.clone());
        // Missing units still have their leftover links removed.
        if let Ok(info) = resolve(&name, lp) {
            has_install_info |= info.info.has_install_info();
            // Following an alias lands on the canonical unit; its links must
            // go too.
            marked.insert(info.name.clone());
            for alias in &info.info.aliases {
                let expanded =
                    expand_specifiers(alias, &info.name, info.info.default_instance.as_deref());
                if let Some((_, entry)) = expanded.split_once('/') {
                    marked.insert(entry.to_string());
                } else {
                    marked.insert(expanded);
                }
            }
            for also in &info.info.also {
                queue.push_back(expand_specifiers(
                    also,
                    &info.name,
                    info.info.default_instance.as_deref(),
                ));
            }
            infos.push(info);
        }
    }

    remove_marked_symlinks(&config, &mut marked, &mut changes);
    if !opts.applying_preset {
        for info in &infos {
            mask_dependencies(lp, &config, info, &mut changes);
        }
    }

    (has_install_info || changes.has_modifications(), changes)
}

/// Remove every symlink below `config` that points at (or is named after) a
/// marked unit, repeating until no further links match.
fn remove_marked_symlinks(config: &Path, marked: &mut HashSet<String>, changes: &mut Changes) {
    loop {
        let mut removed = false;
        let mut stack = vec![config.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(meta) = std::fs::symlink_metadata(&path) else {
                    continue;
                };
                if meta.file_type().is_dir() {
                    stack.push(path);
                    continue;
                }
                if !meta.file_type().is_symlink() || is_mask_entry(&path) {
                    continue;
                }
                let entry_name = file_name_text(&path);
                if !is_valid_unit_name(&entry_name) {
                    continue;
                }
                let entry_template = template_of(&entry_name);
                let mut matched = marked.contains(&entry_name)
                    || entry_template
                        .as_ref()
                        .map(|t| marked.contains(t))
                        .unwrap_or(false);
                if !matched {
                    if let Ok(target) = std::fs::read_link(&path) {
                        let resolved = resolve_relative(&path, &target);
                        let target_name = file_name_text(&resolved);
                        let target_template = template_of(&target_name);
                        matched = marked.contains(&target_name)
                            || target_template
                                .as_ref()
                                .map(|t| marked.contains(t))
                                .unwrap_or(false);
                    }
                }
                if !matched {
                    continue;
                }
                if std::fs::remove_file(&path).is_ok() {
                    changes.unlink(path_text(&path));
                    marked.insert(entry_name);
                    removed = true;
                }
            }
        }
        if !removed {
            break;
        }
    }
    prune_empty_dirs(config);
}

fn prune_empty_dirs(config: &Path) {
    let mut dirs = Vec::new();
    let mut stack = vec![config.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                stack.push(entry.path());
                dirs.push(entry.path());
            }
        }
    }
    // Deepest first.
    dirs.sort_by_key(|dir| std::cmp::Reverse(dir.components().count()));
    for dir in dirs {
        if dir != config {
            let _ = std::fs::remove_dir(&dir);
        }
    }
}

fn dependency_suffix(dir_name: &str) -> Option<&'static str> {
    [".wants", ".requires", ".upholds"]
        .into_iter()
        .find(|suffix| dir_name.ends_with(suffix))
}

fn dependency_declared(info: &ResolvedUnit, suffix: &str, dir_unit: &str) -> bool {
    let lists: [(&Vec<String>, &str); 3] = [
        (&info.info.wanted_by, ".wants"),
        (&info.info.required_by, ".requires"),
        (&info.info.upheld_by, ".upholds"),
    ];
    for (list, expected) in lists {
        if expected != suffix {
            continue;
        }
        for target in list {
            let expanded =
                expand_specifiers(target, &info.name, info.info.default_instance.as_deref());
            if expanded == dir_unit {
                return true;
            }
        }
    }
    false
}

/// Create `/dev/null` masks in the configuration directory for vendor
/// dependency links that `disable` cannot remove.
fn mask_dependencies(lp: &LookupPaths, config: &Path, info: &ResolvedUnit, changes: &mut Changes) {
    if info.masked {
        return;
    }
    let mut names = vec![info.name.clone()];
    for alias in &info.info.aliases {
        names.push(expand_specifiers(
            alias,
            &info.name,
            info.info.default_instance.as_deref(),
        ));
    }

    for vendor in lp.vendor_paths() {
        let Ok(entries) = std::fs::read_dir(vendor) else {
            continue;
        };
        for entry in entries.flatten() {
            let dir_name = file_name_text(&entry.path());
            let Some(suffix) = dependency_suffix(&dir_name) else {
                continue;
            };
            let dir_unit = &dir_name[..dir_name.len() - suffix.len()];
            if !dependency_declared(info, suffix, dir_unit) {
                continue;
            }
            let Ok(links) = std::fs::read_dir(entry.path()) else {
                continue;
            };
            for link in links.flatten() {
                let link_path = link.path();
                let link_name = file_name_text(&link_path);
                let link_template = template_of(&link_name);
                let matched = names.iter().any(|name| {
                    *name == link_name || link_template.as_deref() == Some(name.as_str())
                });
                if !matched {
                    continue;
                }
                let mask = config.join(&dir_name).join(&link_name);
                if std::fs::symlink_metadata(&mask).is_ok() {
                    continue;
                }
                if let Some(parent) = mask.parent() {
                    if std::fs::create_dir_all(parent).is_err() {
                        continue;
                    }
                }
                if symlink("/dev/null", &mask).is_ok() {
                    changes.record("mask-dependency", path_text(&mask), path_text(&link_path));
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Reenable
// ---------------------------------------------------------------------------

pub fn reenable(lp: &LookupPaths, names: &[String], opts: &Options) -> (bool, Changes) {
    let mut base_names = Vec::new();
    let mut files = Vec::new();
    for name in names {
        match resolve(name, lp) {
            Ok(info) if !info.masked => {
                base_names.push(info.name.clone());
                files.push(path_text(&info.path));
            }
            _ => {
                base_names.push(name.clone());
                files.push(name.clone());
            }
        }
    }
    let (_, mut changes) = disable(lp, &base_names, opts);
    if changes.error.is_some() {
        return (false, changes);
    }
    let (carries, enable_changes) = enable(lp, &files, opts);
    changes.merge(enable_changes);
    (carries, changes)
}

// ---------------------------------------------------------------------------
// Mask / unmask
// ---------------------------------------------------------------------------

pub fn mask(lp: &LookupPaths, names: &[String], opts: &Options) -> Changes {
    let config = opts.config(lp).to_path_buf();
    let mut changes = Changes::default();
    for name in names {
        if !is_valid_unit_name(name) {
            changes.fail(invalid_name(name));
            continue;
        }
        let path = config.join(name);
        if let Err(message) = add_symlink(&path, Path::new("/dev/null"), opts.force, &mut changes) {
            changes.fail(message);
            return changes;
        }
    }
    changes
}

pub fn unmask(lp: &LookupPaths, names: &[String], opts: &Options) -> Changes {
    let config = opts.config(lp).to_path_buf();
    let mut changes = Changes::default();
    for name in names {
        if !is_valid_unit_name(name) {
            changes.fail(invalid_name(name));
            return changes;
        }
        let path = config.join(name);
        if !is_mask_entry(&path) {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {
                changes.unlink(path_text(&path));
                let mut marked = HashSet::from([name.clone()]);
                remove_marked_symlinks(&config, &mut marked, &mut changes);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                changes.fail(io_error(
                    sysa::l10n::t_("Failed to remove {path}: {e}."),
                    &path,
                    &e,
                ));
                return changes;
            }
        }
    }
    changes
}

// ---------------------------------------------------------------------------
// Link / revert
// ---------------------------------------------------------------------------

pub fn link(lp: &LookupPaths, files: &[String], opts: &Options) -> Changes {
    let config = opts.config(lp).to_path_buf();
    let mut changes = Changes::default();
    for file in files {
        if !file.starts_with('/') {
            changes.fail(invalid_name(file));
            continue;
        }
        let source = normalize(Path::new(file));
        let name = file_name_text(&source);
        if !is_valid_unit_name(&name) {
            changes.fail(invalid_name(&name));
            return changes;
        }
        match std::fs::symlink_metadata(&source) {
            Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => {}
            _ => {
                changes.fail(sysa::l10n::fmt(
                    sysa::l10n::t_("Unit file {path} is not a regular file."),
                    &[("path", &path_text(&source))],
                ));
                return changes;
            }
        }
        // A file already inside the search path is a silent no-op.
        if lp.search_paths.iter().any(|dir| source.starts_with(dir)) {
            continue;
        }
        let dest = config.join(&name);
        if let Err(message) = add_symlink(&dest, &source, opts.force, &mut changes) {
            changes.fail(message);
            return changes;
        }
    }
    changes
}

pub fn revert(lp: &LookupPaths, names: &[String]) -> Changes {
    let mut changes = Changes::default();
    let mut marked: HashSet<String> = HashSet::new();
    for name in names {
        if !is_valid_unit_name(name) {
            changes.fail(invalid_name(name));
            return changes;
        }
        marked.insert(name.clone());
        let has_vendor = lp.vendor_paths().iter().any(|dir| dir.join(name).is_file());
        for config in [&lp.persistent_config, &lp.runtime_config] {
            let dropin = config.join(format!("{name}.d"));
            if dropin.is_dir() {
                remove_dir_recording(&dropin, &mut changes);
            }
            if has_vendor {
                let unit_path = config.join(name);
                if let Ok(meta) = std::fs::symlink_metadata(&unit_path) {
                    if meta.is_file() || meta.file_type().is_symlink() {
                        if std::fs::remove_file(&unit_path).is_ok() {
                            changes.unlink(path_text(&unit_path));
                        }
                    }
                }
            }
        }
    }
    remove_marked_symlinks(&lp.persistent_config, &mut marked, &mut changes);
    remove_marked_symlinks(&lp.runtime_config, &mut marked, &mut changes);
    changes
}

fn remove_dir_recording(dir: &Path, changes: &mut Changes) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            remove_dir_recording(&path, changes);
        } else if std::fs::remove_file(&path).is_ok() {
            changes.unlink(path_text(&path));
        }
    }
    if std::fs::remove_dir(dir).is_ok() {
        changes.unlink(path_text(dir));
    }
}

// ---------------------------------------------------------------------------
// Default target
// ---------------------------------------------------------------------------

pub fn set_default(lp: &LookupPaths, name: &str, force: bool) -> Changes {
    let mut changes = Changes::default();
    if !super::is_target(name) || name == "default.target" {
        changes.fail(invalid_name(name));
        return changes;
    }
    let info = match resolve(name, lp) {
        Ok(info) => info,
        Err(message) => {
            changes.fail(message);
            return changes;
        }
    };
    let dest = lp.persistent_config.join("default.target");
    if let Err(message) = add_symlink(&dest, &info.path, force, &mut changes) {
        changes.fail(message);
    }
    changes
}

pub fn get_default(lp: &LookupPaths) -> Result<String, String> {
    for dir in &lp.search_paths {
        let path = dir.join("default.target");
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.file_type().is_symlink() {
            if meta.is_file() {
                return Ok("default.target".to_string());
            }
            continue;
        }
        let mut current = path;
        for _ in 0..64 {
            let target = std::fs::read_link(&current).map_err(|e| {
                io_error(sysa::l10n::t_("Failed to read {path}: {e}."), &current, &e)
            })?;
            let resolved = resolve_relative(&current, &target);
            if resolved == Path::new("/dev/null") {
                return Err(sysa::l10n::t_("Unit file is masked."));
            }
            match std::fs::symlink_metadata(&resolved) {
                Ok(next) if next.file_type().is_symlink() => current = resolved,
                _ => return Ok(file_name_text(&resolved)),
            }
        }
        return Err(sysa::l10n::t_(
            "Too many levels of symbolic links while resolving default.target.",
        ));
    }
    Ok("default.target".to_string())
}

// ---------------------------------------------------------------------------
// Preset
// ---------------------------------------------------------------------------

pub fn preset(
    lp: &LookupPaths,
    names: &[String],
    opts: &Options,
    mode: PresetMode,
) -> (bool, Changes) {
    let rules = preset::read_system_presets();
    preset_names(lp, names, opts, mode, &rules)
}

pub fn preset_all(lp: &LookupPaths, opts: &Options, mode: PresetMode) -> (bool, Changes) {
    let rules = preset::read_system_presets();
    let mut names = Vec::new();
    for dir in &lp.search_paths {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = file_name_text(&entry.path());
            if !is_valid_unit_name(&name) {
                continue;
            }
            if entry
                .file_type()
                .map(|t| t.is_file() || t.is_symlink())
                .unwrap_or(false)
            {
                names.push(name);
            }
        }
    }
    preset_names(lp, &names, opts, mode, &rules)
}

fn preset_names(
    lp: &LookupPaths,
    names: &[String],
    opts: &Options,
    mode: PresetMode,
    rules: &[preset::PresetRule],
) -> (bool, Changes) {
    let mut plus = Vec::new();
    let mut minus = Vec::new();
    for name in names {
        // Aliases are skipped: the canonical unit gets the policy.
        if let Ok(info) = resolve(name, lp) {
            if info.name != *name {
                continue;
            }
        }
        let (action, instances) = preset::query(rules, name);
        match action {
            PresetAction::Enable => match instances {
                Some(list) => plus.extend(list),
                None => plus.push(name.clone()),
            },
            PresetAction::Disable => minus.push(name.clone()),
            PresetAction::Ignore => {}
        }
    }

    let mut changes = Changes::default();
    let mut carries = false;
    let mut preset_opts = opts.clone();
    preset_opts.applying_preset = true;
    preset_opts.ignore_auxiliary_failure = true;

    if mode != PresetMode::EnableOnly {
        let (_, disable_changes) = disable(lp, &minus, &preset_opts);
        changes.merge(disable_changes);
        if changes.error.is_some() {
            return (carries, changes);
        }
    }
    if mode != PresetMode::DisableOnly {
        let (enabled, enable_changes) = enable(lp, &plus, &preset_opts);
        carries = enabled;
        changes.merge(enable_changes);
    }
    (carries, changes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "systema-sysw-install-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(dir.join("etc")).unwrap();
        fs::create_dir_all(dir.join("run")).unwrap();
        fs::create_dir_all(dir.join("usr")).unwrap();
        dir
    }

    fn paths(root: &Path) -> LookupPaths {
        let etc = root.join("etc");
        let run = root.join("run");
        let usr = root.join("usr");
        LookupPaths {
            persistent_config: etc.clone(),
            runtime_config: run.clone(),
            search_paths: vec![etc, run, usr.clone()],
            vendor_dirs: vec![usr],
        }
    }

    #[test]
    fn enable_creates_wants_and_alias_links() {
        let root = temp_dir("enable");
        let lp = paths(&root);
        let unit = root.join("usr/foo.service");
        fs::write(
            &unit,
            "[Install]\nWantedBy=multi-user.target\nAlias=bar.service\n",
        )
        .unwrap();

        let (carries, changes) = enable(
            &lp,
            &["foo.service".to_string()],
            &Options::new(false, false),
        );
        assert!(carries);
        assert!(changes.error.is_none(), "{:?}", changes.error);
        let link = lp
            .persistent_config
            .join("multi-user.target.wants/foo.service");
        assert_eq!(fs::read_link(&link).unwrap(), unit);
        assert_eq!(
            fs::read_link(lp.persistent_config.join("bar.service")).unwrap(),
            unit
        );
    }

    #[test]
    fn disable_removes_links_but_keeps_masks() {
        let root = temp_dir("disable");
        let lp = paths(&root);
        let unit = root.join("usr/foo.service");
        fs::write(&unit, "[Install]\nWantedBy=multi-user.target\n").unwrap();
        enable(
            &lp,
            &["foo.service".to_string()],
            &Options::new(false, false),
        );
        let link = lp
            .persistent_config
            .join("multi-user.target.wants/foo.service");
        assert!(link.exists());

        let (carries, changes) = disable(
            &lp,
            &["foo.service".to_string()],
            &Options::new(false, false),
        );
        assert!(carries);
        assert!(changes.error.is_none(), "{:?}", changes.error);
        assert!(!link.exists());
    }

    #[test]
    fn mask_and_unmask() {
        let root = temp_dir("mask");
        let lp = paths(&root);
        fs::write(root.join("usr/foo.service"), "[Install]\n").unwrap();

        let changes = mask(
            &lp,
            &["foo.service".to_string()],
            &Options::new(false, false),
        );
        assert!(changes.error.is_none(), "{:?}", changes.error);
        let masked = lp.persistent_config.join("foo.service");
        assert_eq!(fs::read_link(&masked).unwrap(), PathBuf::from("/dev/null"));

        let changes = unmask(
            &lp,
            &["foo.service".to_string()],
            &Options::new(false, false),
        );
        assert!(changes.error.is_none(), "{:?}", changes.error);
        assert!(!masked.exists());
    }

    #[test]
    fn template_uses_default_instance() {
        let root = temp_dir("template");
        let lp = paths(&root);
        let unit = root.join("usr/getty@.service");
        fs::write(
            &unit,
            "[Install]\nWantedBy=getty.target\nDefaultInstance=tty1\n",
        )
        .unwrap();

        let (carries, changes) = enable(
            &lp,
            &["getty@.service".to_string()],
            &Options::new(false, false),
        );
        assert!(carries);
        assert!(changes.error.is_none(), "{:?}", changes.error);
        let link = lp
            .persistent_config
            .join("getty.target.wants/getty@tty1.service");
        assert_eq!(fs::read_link(link).unwrap(), unit);
    }

    #[test]
    fn instance_symlink_follows_template_info() {
        let root = temp_dir("instance");
        let lp = paths(&root);
        let unit = root.join("usr/foo@.service");
        fs::write(&unit, "[Install]\nWantedBy=multi-user.target\n").unwrap();

        let (carries, changes) = enable(
            &lp,
            &["foo@bar.service".to_string()],
            &Options::new(false, false),
        );
        assert!(carries);
        assert!(changes.error.is_none(), "{:?}", changes.error);
        let link = lp
            .persistent_config
            .join("multi-user.target.wants/foo@bar.service");
        assert_eq!(fs::read_link(link).unwrap(), unit);
    }

    #[test]
    fn disabling_one_instance_keeps_the_other() {
        let root = temp_dir("instances");
        let lp = paths(&root);
        let unit = root.join("usr/foo@.service");
        fs::write(&unit, "[Install]\nWantedBy=multi-user.target\n").unwrap();
        enable(
            &lp,
            &["foo@one.service".to_string(), "foo@two.service".to_string()],
            &Options::new(false, false),
        );
        let wants = lp.persistent_config.join("multi-user.target.wants");
        assert!(wants.join("foo@one.service").exists());
        assert!(wants.join("foo@two.service").exists());

        let (_, changes) = disable(
            &lp,
            &["foo@one.service".to_string()],
            &Options::new(false, false),
        );
        assert!(changes.error.is_none(), "{:?}", changes.error);
        assert!(!wants.join("foo@one.service").exists());
        assert!(wants.join("foo@two.service").exists());
    }

    #[test]
    fn disabling_one_instance_masks_only_that_instance() {
        let root = temp_dir("instance-vendor");
        let lp = paths(&root);
        let unit = root.join("usr/foo@.service");
        fs::write(&unit, "[Install]\nWantedBy=multi-user.target\n").unwrap();
        let vendor_wants = root.join("usr/multi-user.target.wants");
        fs::create_dir_all(&vendor_wants).unwrap();
        symlink(&unit, vendor_wants.join("foo@one.service")).unwrap();
        symlink(&unit, vendor_wants.join("foo@two.service")).unwrap();

        let changes = disable(
            &lp,
            &["foo@one.service".to_string()],
            &Options::new(false, false),
        )
        .1;
        assert!(changes.error.is_none(), "{:?}", changes.error);
        let masks = lp.persistent_config.join("multi-user.target.wants");
        assert_eq!(
            fs::read_link(masks.join("foo@one.service")).unwrap(),
            PathBuf::from("/dev/null")
        );
        assert!(!masks.join("foo@two.service").exists());
    }

    #[test]
    fn disabling_vendor_link_masks_it() {
        let root = temp_dir("vendor");
        let lp = paths(&root);
        let unit = root.join("usr/foo.service");
        fs::write(&unit, "[Install]\nWantedBy=multi-user.target\n").unwrap();
        let vendor_wants = root.join("usr/multi-user.target.wants");
        fs::create_dir_all(&vendor_wants).unwrap();
        symlink(&unit, vendor_wants.join("foo.service")).unwrap();

        let changes = disable(
            &lp,
            &["foo.service".to_string()],
            &Options::new(false, false),
        )
        .1;
        assert!(changes.error.is_none(), "{:?}", changes.error);
        let mask = lp
            .persistent_config
            .join("multi-user.target.wants/foo.service");
        assert_eq!(fs::read_link(mask).unwrap(), PathBuf::from("/dev/null"));
    }

    #[test]
    fn revert_removes_configuration_links() {
        let root = temp_dir("revert");
        let lp = paths(&root);
        let unit = root.join("usr/foo.service");
        fs::write(&unit, "[Install]\nWantedBy=multi-user.target\n").unwrap();
        enable(
            &lp,
            &["foo.service".to_string()],
            &Options::new(false, false),
        );

        let changes = revert(&lp, &["foo.service".to_string()]);
        assert!(changes.error.is_none(), "{:?}", changes.error);
        assert!(!lp
            .persistent_config
            .join("multi-user.target.wants/foo.service")
            .exists());
    }

    #[test]
    fn set_and_get_default() {
        let root = temp_dir("default");
        let lp = paths(&root);
        let target = root.join("usr/graphical.target");
        fs::write(&target, "[Unit]\n").unwrap();

        let changes = set_default(&lp, "graphical.target", false);
        assert!(changes.error.is_none(), "{:?}", changes.error);
        assert_eq!(get_default(&lp).unwrap(), "graphical.target");
    }
}
