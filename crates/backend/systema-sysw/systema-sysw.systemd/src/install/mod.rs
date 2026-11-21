//! Client-side unit-file installation (the `systemctl enable/disable/...`
//! semantics), used when `systemctl` is unavailable or the fallback is forced.
//!
//! This mirrors the observable behavior of systemd's `src/shared/install.c`:
//! persistent configuration lives in `/etc/systemd/system`, runtime
//! configuration in `$runstatedir/systemd/system`, and unit files are searched
//! in [`sysa::paths::Paths::unit_search_paths`] order (which now includes the
//! runtime directory).  The `[Install]` section of a unit file (plus its
//! drop-ins) is the recipe that gets materialised as `.wants/`, `.requires/`,
//! `.upholds/` and alias symlinks.

pub mod ops;
pub mod parse;
pub mod preset;

use std::path::{Path, PathBuf};

use sysa::unit_name;

pub use ops::Options;
pub use ops::{
    disable, enable, get_default, link, mask, preset, preset_all, reenable, revert, set_default,
    unmask,
};
pub use parse::InstallSection;

/// One `(change type, path, source)` triple as exposed by the D-Bus API.
pub type ChangeTuple = (String, String, String);

/// Collected installation changes.
///
/// Warnings such as `masked` or `destination not present` are recorded as
/// successful changes (exactly like systemd serializes them); the first
/// failure is kept separately and turns the whole call into an error.
#[derive(Debug, Default)]
pub struct Changes {
    pub records: Vec<ChangeTuple>,
    pub error: Option<String>,
}

impl Changes {
    pub fn record(&mut self, kind: &str, path: impl Into<String>, source: impl Into<String>) {
        self.records
            .push((kind.to_string(), path.into(), source.into()));
    }

    pub fn unlink(&mut self, path: impl Into<String>) {
        self.record("unlink", path, "");
    }

    pub fn symlink(&mut self, path: impl Into<String>, source: impl Into<String>) {
        self.record("symlink", path, source);
    }

    pub fn fail(&mut self, message: String) {
        if self.error.is_none() {
            self.error = Some(message);
        }
    }

    pub fn merge(&mut self, other: Changes) {
        self.records.extend(other.records);
        if self.error.is_none() {
            self.error = other.error;
        }
    }

    pub fn has_modifications(&self) -> bool {
        !self.records.is_empty()
    }
}

/// The directory layout the install operations work on.
#[derive(Debug, Clone)]
pub struct LookupPaths {
    /// Persistent configuration directory (`/etc/systemd/system`).
    pub persistent_config: PathBuf,
    /// Runtime configuration directory (`$runstatedir/systemd/system`).
    pub runtime_config: PathBuf,
    /// Unit search path, highest priority first.
    pub search_paths: Vec<PathBuf>,
    /// Search-path directories owned by the distribution (`/usr`, `/lib`).
    pub vendor_dirs: Vec<PathBuf>,
}

impl LookupPaths {
    /// The host's effective layout.
    pub fn system() -> Self {
        let paths = sysa::paths::instance();
        let runtime_config = PathBuf::from(format!("{}/systemd/system", paths.runstatedir));
        let persistent_config = PathBuf::from("/etc/systemd/system");
        let mut search_paths: Vec<PathBuf> =
            paths.unit_search_paths.iter().map(PathBuf::from).collect();
        // systemd's lookup path always contains the runtime configuration
        // directory, right after the persistent one.
        if !search_paths.iter().any(|dir| dir == &runtime_config) {
            match search_paths
                .iter()
                .position(|dir| dir == &persistent_config)
            {
                Some(index) => search_paths.insert(index + 1, runtime_config.clone()),
                None => search_paths.insert(0, runtime_config.clone()),
            }
        }
        Self {
            persistent_config,
            runtime_config,
            vendor_dirs: search_paths
                .iter()
                .filter(|dir| is_vendor_dir(dir))
                .cloned()
                .collect(),
            search_paths,
        }
    }

    /// Configuration directory for the requested scope.
    pub fn config_path(&self, runtime: bool) -> &Path {
        if runtime {
            &self.runtime_config
        } else {
            &self.persistent_config
        }
    }

    /// Directories owned by the distribution (not configuration).
    pub fn vendor_paths(&self) -> Vec<&Path> {
        self.vendor_dirs.iter().map(PathBuf::as_path).collect()
    }
}

/// Whether `dir` is a vendor unit directory (`/usr/...` or `/lib/...`).
pub fn is_vendor_dir(dir: &Path) -> bool {
    let text = dir.to_string_lossy();
    text.starts_with("/usr") || text == "/lib" || text.starts_with("/lib/")
}

const UNIT_SUFFIXES: &[&str] = &[
    "service",
    "socket",
    "device",
    "mount",
    "automount",
    "swap",
    "target",
    "path",
    "timer",
    "slice",
    "scope",
];

/// systemd's `unit_name_is_valid(name, UNIT_NAME_ANY)`.
pub fn is_valid_unit_name(name: &str) -> bool {
    if !unit_name::looks_like_unit_name(name) {
        return false;
    }
    match name.rfind('.') {
        Some(dot) => UNIT_SUFFIXES.contains(&&name[dot + 1..]),
        None => false,
    }
}

/// Whether `name` is a `.target` unit.
pub fn is_target(name: &str) -> bool {
    name.ends_with(".target") && is_valid_unit_name(name)
}

pub fn is_template(name: &str) -> bool {
    unit_name::is_template(name)
}

pub fn template_of(name: &str) -> Option<String> {
    unit_name::template_of(name)
}

/// systemd's `unit_name_template()`: the template name of a template or
/// instance unit, `None` for plain names.
pub fn name_template(name: &str) -> Option<String> {
    if is_template(name) {
        Some(name.to_string())
    } else {
        template_of(name)
    }
}

pub fn instance_of(name: &str) -> Option<String> {
    unit_name::instance_of(name)
}

pub fn replace_instance(template: &str, instance: &str) -> Option<String> {
    parse::replace_instance(template, instance)
}

/// A unit resolved to its file plus parsed `[Install]` section.
#[derive(Debug, Clone)]
pub struct ResolvedUnit {
    /// Canonical unit name used to expand specifiers.
    pub name: String,
    /// Unit file the symlinks point at.
    pub path: PathBuf,
    /// Masked units have no install information.
    pub masked: bool,
    /// The mask file (equals `path` for masked units).
    pub mask_path: PathBuf,
    pub info: InstallSection,
}

/// Resolve `name_or_path` and load its `[Install]` section.
///
/// `Err(message)` carries a localized, user-visible message.
pub fn resolve(name_or_path: &str, lp: &LookupPaths) -> Result<ResolvedUnit, String> {
    resolve_inner(name_or_path, lp, 0)
}

fn resolve_inner(
    name_or_path: &str,
    lp: &LookupPaths,
    depth: usize,
) -> Result<ResolvedUnit, String> {
    if depth > 64 {
        return Err(sysa::l10n::fmt(
            sysa::l10n::t_("Too many levels of symbolic links while resolving {name}."),
            &[("name", name_or_path)],
        ));
    }
    let explicit_path = name_or_path.starts_with('/');
    let name = if explicit_path {
        Path::new(name_or_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(name_or_path)
            .to_string()
    } else {
        name_or_path.to_string()
    };
    if !is_valid_unit_name(&name) {
        return Err(invalid_name(&name));
    }

    let found = if explicit_path {
        classify(Path::new(name_or_path))?
    } else {
        locate(&name, lp)?
    };

    match found {
        Found::Missing => Err(not_found(&name)),
        Found::Mask(path) => Ok(ResolvedUnit {
            name,
            path: path.clone(),
            masked: true,
            mask_path: path,
            info: InstallSection::default(),
        }),
        Found::Regular(path) => load(path, name, lp),
        Found::Symlink { target } => {
            let target_name = target
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string();
            if target_name == name {
                if !target_exists(&target) {
                    return Err(not_found(&name));
                }
                return load(target, name, lp);
            }
            if let Some(instance) = instance_of(&name) {
                if is_template(&target_name) {
                    if let Some(followed) = replace_instance(&target_name, &instance) {
                        if followed == name {
                            if !target_exists(&target) {
                                return Err(not_found(&name));
                            }
                            return load(target, name, lp);
                        }
                        return resolve_inner(&followed, lp, depth + 1);
                    }
                }
            }
            if is_valid_unit_name(&target_name) && target.is_file() {
                return load(target, target_name, lp);
            }
            // The symlink may point at a vendor file through a relative name
            // that only exists in the search path; resolve by name.
            resolve_inner(&target_name, lp, depth + 1)
        }
    }
}

enum Found {
    Missing,
    Mask(PathBuf),
    Regular(PathBuf),
    Symlink { target: PathBuf },
}

/// Look `name` up in the search path, falling back to the template when an
/// instance file does not exist.
fn locate(name: &str, lp: &LookupPaths) -> Result<Found, String> {
    for dir in &lp.search_paths {
        let found = classify(&dir.join(name))?;
        if !matches!(found, Found::Missing) {
            return Ok(found);
        }
    }
    if let Some(template) = template_of(name) {
        for dir in &lp.search_paths {
            let found = classify(&dir.join(&template))?;
            if !matches!(found, Found::Missing) {
                // The template file carries the install information for the
                // requested instance; keep the requested name so that
                // specifiers expand with the instance.
                return Ok(match found {
                    Found::Regular(path) => Found::Regular(path),
                    other => other,
                });
            }
        }
    }
    Ok(Found::Missing)
}

/// Classify one path (one symlink step only).
fn classify(path: &Path) -> Result<Found, String> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Found::Missing),
        Err(e) => {
            return Err(io_error(
                sysa::l10n::t_("Failed to inspect {path}: {e}."),
                path,
                &e,
            ));
        }
    };
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(path)
            .map_err(|e| io_error(sysa::l10n::t_("Failed to read {path}: {e}."), path, &e))?;
        let target = resolve_relative(path, &target);
        if target == Path::new("/dev/null") {
            return Ok(Found::Mask(path.to_path_buf()));
        }
        return Ok(Found::Symlink { target });
    }
    if meta.is_file() {
        if meta.len() == 0 {
            return Ok(Found::Mask(path.to_path_buf()));
        }
        return Ok(Found::Regular(path.to_path_buf()));
    }
    Ok(Found::Missing)
}

fn load(path: PathBuf, name: String, lp: &LookupPaths) -> Result<ResolvedUnit, String> {
    let dropins = dropin_files(&name, lp);
    let info = parse::parse_install(&path, &dropins)
        .map_err(|e| io_error(sysa::l10n::t_("Failed to read {path}: {e}."), &path, &e))?;
    Ok(ResolvedUnit {
        name,
        path,
        masked: false,
        mask_path: PathBuf::new(),
        info,
    })
}

/// Drop-in files that apply to `name` (main and template directories),
/// highest priority first, one file per basename.
fn dropin_files(name: &str, lp: &LookupPaths) -> Vec<PathBuf> {
    let mut dirs = vec![format!("{name}.d")];
    if let Some(template) = template_of(name) {
        dirs.push(format!("{template}.d"));
    }
    let mut seen = std::collections::HashSet::new();
    let mut files = Vec::new();
    for dir_name in dirs {
        for dir in &lp.search_paths {
            let dropin_dir = dir.join(&dir_name);
            let Ok(entries) = std::fs::read_dir(&dropin_dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let file_name = entry.file_name();
                let file_name = file_name.to_string_lossy().into_owned();
                if !file_name.ends_with(".conf") {
                    continue;
                }
                if seen.insert(file_name.clone()) {
                    files.push(entry.path());
                }
            }
        }
    }
    files
}

/// Whether a unit with this name exists (a mask counts as existing).
pub fn unit_exists(lp: &LookupPaths, name: &str) -> bool {
    !matches!(locate(name, lp), Ok(Found::Missing) | Err(_))
}

fn target_exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// Resolve a symlink target relative to the link's directory.
pub fn resolve_relative(link: &Path, target: &Path) -> PathBuf {
    if target.is_absolute() {
        return normalize(target);
    }
    match link.parent() {
        Some(parent) => normalize(&parent.join(target)),
        None => normalize(target),
    }
}

/// Lexically normalize a path (no filesystem access).
pub fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn invalid_name(name: &str) -> String {
    sysa::l10n::fmt(
        sysa::l10n::t_("Invalid unit name {name}."),
        &[("name", name)],
    )
}

fn not_found(name: &str) -> String {
    sysa::l10n::fmt(
        sysa::l10n::t_("Unit file {name} not found."),
        &[("name", name)],
    )
}

fn io_error(template: String, path: &Path, error: &std::io::Error) -> String {
    sysa::l10n::fmt(
        &template,
        &[("path", &path.to_string_lossy()), ("e", &error.to_string())],
    )
}
