use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use super::parser::parse_unit_from_path_as_in;
use super::types::UnitFile;
pub fn is_known_extension(name: &str) -> bool {
    matches!(
        name.rsplit('.').next().unwrap_or(""),
        "service"
            | "target"
            | "mount"
            | "automount"
            | "timer"
            | "socket"
            | "slice"
            | "scope"
            | "swap"
            | "path"
            | "device"
            | "power"
    )
}

/// If `path` is a symlink to a regular unit file, resolve the canonical unit
/// name (the basename of the resolved target) and the alias name (the
/// symlink's own basename).
///
/// Returns `None` when `path` is not a symlink, when the symlink is broken or
/// masks to a non-file (`/dev/null` masking is skipped by `is_file()` before
/// this is called), or when the resolved target is not a unit file.  A symlink
/// whose basename already equals the target's basename (e.g. a `.wants/`
/// enablement link pointing at the same-named unit) is not an alias, and
/// neither is an enablement link that *instantiates* a template
/// (`foo@bar.service` -> `foo@.service`): such a link is an instance of the
/// template, not another name for it, and must be loaded under its own
/// instance name so `%i`/`%I` are expanded.
fn symlink_alias(path: &Path) -> Option<(String, String)> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.file_type().is_symlink() {
        return None;
    }
    let target = std::fs::canonicalize(path).ok()?;
    let canonical_name = target.file_name().and_then(|n| n.to_str())?.to_string();
    let alias_name = path.file_name().and_then(|n| n.to_str())?.to_string();
    if canonical_name == alias_name || !is_known_extension(&canonical_name) {
        return None;
    }
    if sysa::unit_name::is_template(&canonical_name)
        && sysa::unit_name::template_of(&alias_name).as_deref() == Some(canonical_name.as_str())
    {
        return None;
    }
    Some((canonical_name, alias_name))
}

/// True when `path` is a *primary enablement link*: a symlink whose basename
/// equals the resolved target's basename but that lives in a different
/// directory, e.g. `/etc/systemd/system/foo.service` →
/// `../usr/lib/systemd/system/foo.service`.
///
/// That is how `systemctl enable` makes a unit start at boot without pulling
/// it into a `<target>.wants/` directory, so it is enablement evidence.  A
/// link whose basename differs from its target's (an alias, see
/// [`symlink_alias`]) is not enablement, and neither is a link that
/// instantiates a template.  Masking (`-> /dev/null`) fails to resolve and is
/// not enablement either.
fn is_primary_enablement_link(path: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !meta.file_type().is_symlink() {
        return false;
    }
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if !is_known_extension(name) || sysa::unit_name::is_template(name) {
        return false;
    }
    let Ok(target) = std::fs::canonicalize(path) else {
        return false;
    };
    if target.file_name().and_then(|n| n.to_str()) != Some(name) {
        return false;
    }
    let Some(link_dir) = path.parent().and_then(|dir| dir.canonicalize().ok()) else {
        return false;
    };
    // `canonicalize` resolved the link, so the target's parent is canonical.
    target.parent() != Some(link_dir.as_path())
}

/// Whether `dir` is a runtime (non-persistent) unit directory.
///
/// systemd distinguishes `enabled` from `enabled-runtime` (and `linked` from
/// `linked-runtime`) by whether the enablement symlink lives under `/run`, so
/// the loader tracks that bit while it walks each search path.
fn is_runtime_dir(dir: &str) -> bool {
    dir == "/run" || dir.starts_with("/run/")
}

/// systemd's `UnitFileState` for one discovered unit file.
///
/// Mirrors `unit_file_lookup_state()`'s order: a unit-file link ("linked")
/// describes the definition itself and wins over enablement evidence; then a
/// `<unit>.wants/`/`.requires/` link makes the unit enabled; without any link
/// the state follows the `[Install]` section (`disabled` when it names
/// something `enable` could act on, `indirect` when it only carries `Also=`,
/// `static` otherwise).
fn classify_file_state(
    path: &Path,
    in_dep_dir: bool,
    runtime: bool,
    unit: &UnitFile,
) -> &'static str {
    // Membership links come first: a `<target>.wants/` link points at the
    // same-named file, which `is_primary_enablement_link()` would also call a
    // link, yet systemd reports enablement for it, not "linked".
    if in_dep_dir {
        return if runtime {
            "enabled-runtime"
        } else {
            "enabled"
        };
    }
    if is_primary_enablement_link(path) {
        return if runtime { "linked-runtime" } else { "linked" };
    }
    let installable = !unit.install.wanted_by.is_empty()
        || !unit.install.required_by.is_empty()
        || !unit.install.alias.is_empty();
    if installable {
        "disabled"
    } else if !unit.install.also.is_empty() {
        "indirect"
    } else {
        "static"
    }
}

/// Discover and parse all systemd unit files from standard search paths.
///
/// Every returned unit carries the loader's enablement verdict: a unit pulled
/// in by a `<target>.wants/`/`.requires/` link or reached through a primary
/// enablement link is [`UnitFile::enabled`], everything else is not.  The
/// same unit may be returned more than once (definition and enablement link);
/// merging the copies is the caller's job.
pub fn discover_all() -> Result<Vec<UnitFile>> {
    let mut units = Vec::new();
    let dirs = sysa::paths::instance().unit_search_paths.clone();
    // Units pulled in implicitly by `<unit>.wants/` / `<unit>.requires/`
    // directories, keyed by the name of the unit the directory belongs to
    // (e.g. `sockets.target.wants/dbus.socket` => sockets.target wants dbus.socket).
    let mut implicit: HashMap<String, (HashSet<String>, HashSet<String>)> = HashMap::new();
    for dir in &dirs {
        let path = Path::new(dir);
        if path.exists() {
            load_units_from_dir_recursive(
                path,
                &dirs,
                &mut units,
                &mut implicit,
                None,
                is_runtime_dir(dir),
            )
            .with_context(|| {
                sysa::l10n::fmt(
                    sysa::l10n::t_("Scanning {dir} ..."),
                    &[("dir", &dir.to_string())],
                )
            })?;
        }
    }
    // Instance drop-in directories carry the per-instance specialization that
    // cannot be derived from the template alone.
    discover_instance_specializations(&mut units);
    apply_implicit_deps(&mut units, &implicit);
    info!("Systemd finder discovered {} unit(s)", units.len());
    Ok(units)
}

/// Find and parse a single named unit file from standard search paths.
///
/// If an exact file with `name` does not exist but `name` is an instance
/// unit (`foo@bar.service`), the corresponding template file
/// (`foo@.service`) is loaded instead and instantiated with the requested
/// name (so `%i`/`%p`/`%n` specifiers are expanded with the instance).
pub fn discover_one(name: &str) -> Result<Option<UnitFile>> {
    discover_one_in(&sysa::paths::instance().unit_search_paths, name)
}

/// [`discover_one`] over an explicit search-path list (testable without
/// touching the global path configuration).
fn discover_one_in(dirs: &[String], name: &str) -> Result<Option<UnitFile>> {
    // A bare template (e.g. `getty@.service`) is a definition, not a unit
    // that can be looked up or started; refusing it here keeps a reference
    // to `foo@.service` from ever materialising as a unit.
    if sysa::unit_name::is_template(name) {
        return Ok(None);
    }
    if let Some(unit) = find_exact_in(dirs, name)? {
        return Ok(Some(unit));
    }

    // Template fallback for instance units.
    if let Some(template) = sysa::unit_name::template_of(name) {
        for dir in dirs {
            let path = Path::new(dir).join(&template);
            if path.is_file() {
                match parse_unit_from_path_as_in(dirs, &path, name) {
                    Ok(unit) => return Ok(Some(unit)),
                    Err(e) => {
                        warn!(
                            "Failed to parse template {} for {}: {}",
                            path.display(),
                            name,
                            e
                        );
                    }
                }
            }
        }
    }

    Ok(None)
}

/// Find and parse a unit file that exists verbatim under `name`.
///
/// When `name` is an alias (a symlink pointing at another unit file), the
/// file is parsed under its canonical name and the alias is recorded, so a
/// lookup of `display-manager.service` yields the `lightdm.service` unit.
fn find_exact_in(dirs: &[String], name: &str) -> Result<Option<UnitFile>> {
    for dir in dirs {
        let path = Path::new(dir).join(name);
        if path.exists() {
            let parsed = match symlink_alias(&path) {
                Some((canonical, _)) => {
                    let mut unit = parse_unit_from_path_as_in(dirs, &path, &canonical)?;
                    if !unit.install.alias.iter().any(|a| a == name) {
                        unit.install.alias.push(name.to_string());
                    }
                    unit.alias_links.push(name.to_string());
                    Ok(unit)
                }
                None => parse_unit_from_path_as_in(dirs, &path, name),
            }
            // A single-unit lookup sees the file itself, so it can only
            // recognise a primary enablement link here.  Enablement through a
            // `.wants/`/`.requires/` directory is discovered by
            // [`discover_all`], which walks those directories.
            .map(|mut unit| {
                unit.enabled |= is_primary_enablement_link(&path);
                unit.file_state =
                    Some(classify_file_state(&path, false, is_runtime_dir(dir), &unit).to_string());
                unit
            });
            match parsed {
                Ok(unit) => return Ok(Some(unit)),
                Err(e) => {
                    warn!("Failed to parse {}: {}", path.display(), e);
                }
            }
        }
    }
    Ok(None)
}

/// Discover per-instance drop-in directories (`getty@tty3.service.d/*.conf`)
/// and report the instance they specialize.
///
/// The instance usually has no file of its own — it exists only because a
/// template does.  Parsing the template under the instance name applies the
/// template's drop-ins (`getty@.service.d`) *and* the instance's own
/// (`getty@tty3.service.d`), which is exactly the specialization a consumer
/// cannot reconstruct from the template IR alone.  Instances that discovery
/// already found (a unit file or an enabling symlink) keep their entry.
fn discover_instance_specializations(units: &mut Vec<UnitFile>) {
    let dirs = sysa::paths::instance().unit_search_paths.clone();
    discover_instance_specializations_in(&dirs, units);
}

/// [`discover_instance_specializations`] over an explicit search-path list
/// (testable without touching the global path configuration).
fn discover_instance_specializations_in(dirs: &[String], units: &mut Vec<UnitFile>) {
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let Some(dir_name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let Some(name) = dir_name.strip_suffix(".d") else {
                continue;
            };
            if !is_known_extension(name) || !sysa::unit_name::is_instance(name) {
                continue;
            }
            // A real instance file is authoritative: it was parsed from its
            // own path, so both drop-in scopes were already applied.
            if dirs
                .iter()
                .any(|search| is_regular_file(&Path::new(search).join(name)))
            {
                continue;
            }
            let Some(template) = sysa::unit_name::template_of(name) else {
                continue;
            };
            let path = Path::new(dir).join(&template);
            if !path.is_file() {
                continue;
            }
            match parse_unit_from_path_as_in(dirs, &path, name) {
                Ok(mut unit) => match units.iter().position(|u| u.name == name) {
                    Some(index) => {
                        // Discovered through an enabling symlink in a
                        // `.wants/` directory: that path carried no drop-in
                        // scope, so fold the instance's own drop-ins in here.
                        // The re-parse saw the template, not the enablement
                        // link, so carry the enablement evidence over.
                        debug!("Folding instance drop-ins into {}", name);
                        unit.enabled |= units[index].enabled;
                        units[index] = unit;
                    }
                    None => {
                        debug!("Discovered instance specialization {}", name);
                        units.push(unit);
                    }
                },
                Err(e) => debug!("Skipping instance drop-ins for {}: {}", name, e),
            }
        }
    }
}

/// True for a regular file (not a symlink) at `path`.
fn is_regular_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|meta| meta.file_type().is_file())
        .unwrap_or(false)
}

/// The kind of dependency implied by a `<unit>.wants/` / `<unit>.requires/`
/// directory: every unit inside is wanted/required by the owning unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirDepKind {
    Wants,
    Requires,
}

/// Split a directory name like `sockets.target.wants` into the owning unit
/// name (`sockets.target`) and the dependency kind, if it is a dependency
/// directory.
fn dep_dir_target(name: &str) -> Option<(String, DirDepKind)> {
    name.strip_suffix(".wants")
        .map(|base| (base.to_string(), DirDepKind::Wants))
        .or_else(|| {
            name.strip_suffix(".requires")
                .map(|base| (base.to_string(), DirDepKind::Requires))
        })
}

/// Map an enablement directory's owner name to the canonical unit that should
/// receive its edges.
///
/// A `<unit>.wants/` or `<unit>.requires/` directory may be named after an
/// alias.  With `default.target -> graphical.target`, for example,
/// `default.target.wants/ddm.service` belongs to `graphical.target`; the
/// symlink is parsed under its canonical name, so the directory key no longer
/// matches any unit.  Only materialised symlink aliases (`alias_links`) are
/// used here: a bare `[Install] Alias=` declaration can be contested and is
/// resolved by the Finder, while a symlink is filesystem evidence.
fn implicit_owner_aliases(units: &[UnitFile]) -> HashMap<String, String> {
    let names: HashSet<&str> = units.iter().map(|unit| unit.name.as_str()).collect();
    let mut aliases = HashMap::new();
    for unit in units {
        for alias in &unit.alias_links {
            if names.contains(alias.as_str()) {
                continue;
            }
            aliases
                .entry(alias.clone())
                .or_insert_with(|| unit.name.clone());
        }
    }
    aliases
}

/// Fold the implicit dependency directories discovered during the scan into
/// the owning units' `Wants=`/`Requires=` sets, resolving alias-named owners
/// to their canonical unit.
fn apply_implicit_deps(
    units: &mut [UnitFile],
    implicit: &HashMap<String, (HashSet<String>, HashSet<String>)>,
) {
    let aliases = implicit_owner_aliases(units);
    for unit in units {
        let name = unit.name.clone();
        let mut owners = vec![name.as_str()];
        for (alias, canonical) in &aliases {
            if canonical == &name {
                owners.push(alias.as_str());
            }
        }
        for owner in owners {
            if let Some((wants, requires)) = implicit.get(owner) {
                unit.unit.wants.extend(wants.iter().cloned());
                unit.unit.requires.extend(requires.iter().cloned());
            }
        }
    }
}

fn load_units_from_dir_recursive(
    dir: &Path,
    dirs: &[String],
    units: &mut Vec<UnitFile>,
    implicit: &mut HashMap<String, (HashSet<String>, HashSet<String>)>,
    ctx: Option<(String, DirDepKind)>,
    runtime: bool,
) -> Result<usize> {
    let mut count = 0usize;
    count += load_units_from_dir(dir, dirs, units, implicit, ctx.as_ref(), runtime)?;

    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let dir_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if dir_name.starts_with('.') || dir_name.ends_with(".d") {
                    continue;
                }
                // `<unit>.wants/` / `<unit>.requires/` directories imply a
                // dependency edge from `unit` to everything inside them;
                // nested subdirectories inherit that implication.
                let child_ctx = dep_dir_target(dir_name).or_else(|| ctx.clone());
                count += load_units_from_dir_recursive(
                    &path, dirs, units, implicit, child_ctx, runtime,
                )?;
            }
        }
    }

    Ok(count)
}

fn load_units_from_dir(
    dir: &Path,
    dirs: &[String],
    units: &mut Vec<UnitFile>,
    implicit: &mut HashMap<String, (HashSet<String>, HashSet<String>)>,
    ctx: Option<&(String, DirDepKind)>,
    runtime: bool,
) -> Result<usize> {
    let mut count = 0usize;

    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(0);
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        if !is_known_extension(&name) {
            continue;
        }
        // A template unit (e.g. `getty@.service`) is reported as a *definition*,
        // not as a runnable unit: the parser keeps its `%i`/`%I` specifiers
        // literal, and the IR marks it `template.definition`, so a consumer
        // can materialize any instance from it without touching the disk.
        // Dropping templates here would leave instance names unresolvable.
        // A unit-file symlink aliases its resolved target: the canonical unit
        // name is the target's basename and the symlink's basename becomes an
        // alias of it.  This mirrors systemd, where `display-manager.service`
        // -> `lightdm.service` is not a second unit but another name for the
        // same one.  Masking (-> /dev/null) is already skipped by `is_file()`.
        // Drop-ins are resolved from the unit search paths, never from the
        // directory this entry happens to live in: a `.wants/` link is still
        // the unit it names, so its template and instance drop-ins sit beside
        // the template, not inside the `.wants/` directory.
        let unit = match symlink_alias(&path) {
            Some((canonical, alias)) => {
                let mut unit = match parse_unit_from_path_as_in(dirs, &path, &canonical) {
                    Ok(u) => u,
                    Err(e) => {
                        debug!("Skipping {}: {}", path.display(), e);
                        continue;
                    }
                };
                if !unit.install.alias.iter().any(|a| a == &alias) {
                    unit.install.alias.push(alias.clone());
                }
                unit.alias_links.push(alias);
                Ok(unit)
            }
            None => parse_unit_from_path_as_in(dirs, &path, &name),
        };
        match unit {
            Ok(mut unit) => {
                // Enablement evidence, in the two shapes systemd uses: a link
                // inside a `<unit>.wants/`/`<unit>.requires/` directory (the
                // loader recurses with that context), or a primary enablement
                // link in the search path itself.  Templates are definitions,
                // never boot-set members, so a template link enables nothing.
                if (ctx.is_some() || is_primary_enablement_link(&path))
                    && !sysa::unit_name::is_template(&unit.name)
                {
                    unit.enabled = true;
                }
                unit.file_state =
                    Some(classify_file_state(&path, ctx.is_some(), runtime, &unit).to_string());
                if let Some((target, kind)) = ctx {
                    let slot = implicit.entry(target.clone()).or_default();
                    match kind {
                        DirDepKind::Wants => {
                            slot.0.insert(unit.name.clone());
                        }
                        DirDepKind::Requires => {
                            slot.1.insert(unit.name.clone());
                        }
                    }
                }
                units.push(unit);
                count += 1;
            }
            Err(e) => {
                debug!("Skipping {}: {}", path.display(), e);
            }
        }
    }

    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Create a unique temporary directory for one test.
    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "systema-sysf-loader-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn discover_one_instantiates_template() {
        let dir = temp_dir("tpl");
        let tpl = dir.join("getty@.service");
        fs::write(
            &tpl,
            "[Unit]\nDescription=Getty on %I\n[Service]\nExecStart=/sbin/agetty -o '-p -- \\\\u' %i\n",
        )
        .unwrap();

        let dirs = vec![dir.to_string_lossy().into_owned()];
        let unit = discover_one_in(&dirs, "getty@tty3.service")
            .unwrap()
            .unwrap();

        assert_eq!(unit.name, "getty@tty3.service");
        assert_eq!(unit.unit.description, "Getty on tty3");
        let svc = unit.service.unwrap();
        assert!(svc.exec_start[0].raw.contains("tty3"));
        assert!(svc.exec_start[0].args.contains(&"tty3".to_string()));
    }

    #[test]
    fn discover_one_marks_a_symlink_alias_as_materialised() {
        let dir = temp_dir("alias-link");
        fs::write(
            dir.join("lightdm.service"),
            "[Unit]\nDescription=LightDM\n[Service]\nExecStart=/usr/bin/lightdm\n",
        )
        .unwrap();
        std::os::unix::fs::symlink("lightdm.service", dir.join("display-manager.service")).unwrap();

        let dirs = vec![dir.to_string_lossy().into_owned()];
        let unit = discover_one_in(&dirs, "display-manager.service")
            .unwrap()
            .unwrap();

        assert_eq!(unit.name, "lightdm.service");
        assert_eq!(
            unit.install.alias,
            vec!["display-manager.service".to_string()]
        );
        // The name exists on disk: the finder may treat it as authoritative.
        assert_eq!(
            unit.alias_links,
            vec!["display-manager.service".to_string()]
        );
    }

    #[test]
    fn install_alias_is_not_marked_as_materialised() {
        let dir = temp_dir("alias-hint");
        fs::write(
            dir.join("lightdm.service"),
            "[Unit]\nDescription=LightDM\n[Install]\nAlias=display-manager.service\n\
             [Service]\nExecStart=/usr/bin/lightdm\n",
        )
        .unwrap();

        let dirs = vec![dir.to_string_lossy().into_owned()];
        let unit = discover_one_in(&dirs, "lightdm.service").unwrap().unwrap();

        assert!(unit
            .install
            .alias
            .contains(&"display-manager.service".to_string()));
        assert!(unit.alias_links.is_empty());
    }

    #[test]
    fn discover_one_exact_file_takes_precedence() {
        let dir = temp_dir("exact");
        fs::write(
            dir.join("getty@tty3.service"),
            "[Unit]\nDescription=Exact instance\n[Service]\nExecStart=/bin/true\n",
        )
        .unwrap();

        let dirs = vec![dir.to_string_lossy().into_owned()];
        let unit = discover_one_in(&dirs, "getty@tty3.service")
            .unwrap()
            .unwrap();

        assert_eq!(unit.name, "getty@tty3.service");
        assert_eq!(unit.unit.description, "Exact instance");
    }

    #[test]
    fn discover_one_missing_returns_none() {
        let dir = temp_dir("missing");
        let dirs = vec![dir.to_string_lossy().into_owned()];
        assert!(discover_one_in(&dirs, "nonexistent.service")
            .unwrap()
            .is_none());
        assert!(discover_one_in(&dirs, "getty@tty9.service")
            .unwrap()
            .is_none());
    }

    #[test]
    fn discover_one_plain_unit_no_fallback() {
        let dir = temp_dir("plain");
        // A plain (non-instance) name must never fall back to anything.
        let dirs = vec![dir.to_string_lossy().into_owned()];
        assert!(discover_one_in(&dirs, "sshd.service").unwrap().is_none());
    }

    #[test]
    fn power_units_are_discovered() {
        let dir = temp_dir("power");
        fs::write(
            dir.join("poweroff.power"),
            "[Unit]\nDescription=Power off\n",
        )
        .unwrap();

        // `discover_all` scans whole directories, so `.power` must pass the
        // known-extension filter (the actual regression `list-units` hit).
        assert!(is_known_extension("poweroff.power"));
        let mut units = Vec::new();
        let mut implicit = HashMap::new();
        load_units_from_dir_recursive(
            &dir,
            &[dir.to_string_lossy().into_owned()],
            &mut units,
            &mut implicit,
            None,
            false,
        )
        .unwrap();
        assert!(units.iter().any(|u| u.name == "poweroff.power"));

        // A single-unit lookup works too.
        let dirs = vec![dir.to_string_lossy().into_owned()];
        let unit = discover_one_in(&dirs, "poweroff.power").unwrap().unwrap();
        assert_eq!(unit.name, "poweroff.power");
        assert_eq!(unit.unit.description, "Power off");
        assert_eq!(
            crate::types::UnitKind::from_extension(&unit.name),
            crate::types::UnitKind::Power
        );
    }

    #[test]
    fn wants_dir_synthesizes_dependency_edges() {
        let dir = temp_dir("wants");
        let unit_dir = dir.join("system");
        fs::create_dir_all(&unit_dir).unwrap();
        fs::write(
            unit_dir.join("sockets.target"),
            "[Unit]\nDescription=Socket target\n",
        )
        .unwrap();
        fs::write(
            unit_dir.join("dbus.socket"),
            "[Socket]\nListenStream=/run/dbus/system_bus_socket\n",
        )
        .unwrap();
        // A unit pulled in only via the .wants directory.
        fs::write(
            unit_dir.join("other.service"),
            "[Service]\nExecStart=/bin/true\n",
        )
        .unwrap();
        fs::create_dir_all(unit_dir.join("sockets.target.wants")).unwrap();
        fs::write(
            unit_dir.join("sockets.target.wants").join("dbus.socket"),
            "[Socket]\nListenStream=/run/dbus/system_bus_socket\n",
        )
        .unwrap();
        fs::write(
            unit_dir.join("sockets.target.wants").join("other.service"),
            "[Service]\nExecStart=/bin/true\n",
        )
        .unwrap();

        let mut units = Vec::new();
        let mut implicit = HashMap::new();
        load_units_from_dir_recursive(
            &unit_dir,
            &[unit_dir.to_string_lossy().into_owned()],
            &mut units,
            &mut implicit,
            None,
            false,
        )
        .unwrap();
        apply_implicit_deps(&mut units, &implicit);

        let sockets = units.iter().find(|u| u.name == "sockets.target").unwrap();
        assert!(sockets.unit.wants.contains("dbus.socket"));
        assert!(sockets.unit.wants.contains("other.service"));
        assert!(!sockets.unit.requires.contains("dbus.socket"));
        assert!(!sockets.unit.requires.contains("other.service"));
    }

    #[test]
    fn requires_dir_synthesizes_dependency_edges() {
        let dir = temp_dir("requires");
        let unit_dir = dir.join("system");
        fs::create_dir_all(&unit_dir).unwrap();
        fs::write(
            unit_dir.join("target.service"),
            "[Unit]\nDescription=Target\n[Service]\nExecStart=/bin/true\n",
        )
        .unwrap();
        fs::create_dir_all(unit_dir.join("target.service.requires")).unwrap();
        fs::write(
            unit_dir.join("target.service.requires").join("dep.service"),
            "[Unit]\nDescription=Dep\n[Service]\nExecStart=/bin/true\n",
        )
        .unwrap();

        let mut units = Vec::new();
        let mut implicit = HashMap::new();
        load_units_from_dir_recursive(
            &unit_dir,
            &[unit_dir.to_string_lossy().into_owned()],
            &mut units,
            &mut implicit,
            None,
            false,
        )
        .unwrap();
        apply_implicit_deps(&mut units, &implicit);

        let target = units.iter().find(|u| u.name == "target.service").unwrap();
        assert!(target.unit.requires.contains("dep.service"));
        assert!(!target.unit.wants.contains("dep.service"));
    }

    #[test]
    fn alias_named_wants_and_requires_dirs_fold_into_canonical_target() {
        let dir = temp_dir("wantsaliasowner");
        let unit_dir = dir.join("system");
        fs::create_dir_all(&unit_dir).unwrap();
        fs::write(
            unit_dir.join("graphical.target"),
            "[Unit]\nDescription=Graphical target\n",
        )
        .unwrap();
        fs::write(
            unit_dir.join("ddm.service"),
            "[Unit]\nDescription=Display manager\n[Service]\nExecStart=/usr/bin/ddm\n",
        )
        .unwrap();
        fs::write(
            unit_dir.join("required.service"),
            "[Unit]\nDescription=Required service\n[Service]\nExecStart=/bin/true\n",
        )
        .unwrap();
        // The standard `systemctl set-default` layout: default.target is a
        // symlink alias, and units enabled with WantedBy=default.target live
        // in default.target.wants/.
        std::os::unix::fs::symlink("graphical.target", unit_dir.join("default.target")).unwrap();
        fs::create_dir_all(unit_dir.join("default.target.wants")).unwrap();
        std::os::unix::fs::symlink(
            unit_dir.join("ddm.service"),
            unit_dir.join("default.target.wants/ddm.service"),
        )
        .unwrap();
        fs::create_dir_all(unit_dir.join("default.target.requires")).unwrap();
        std::os::unix::fs::symlink(
            unit_dir.join("required.service"),
            unit_dir.join("default.target.requires/required.service"),
        )
        .unwrap();

        let dirs = vec![unit_dir.to_string_lossy().into_owned()];
        let mut units = Vec::new();
        let mut implicit = HashMap::new();
        load_units_from_dir_recursive(&unit_dir, &dirs, &mut units, &mut implicit, None, false)
            .unwrap();
        apply_implicit_deps(&mut units, &implicit);

        // The symlink is parsed as graphical.target; default.target is only
        // an alias.  Its dependency directories must still land on the
        // canonical unit.
        assert!(!units.iter().any(|u| u.name == "default.target"));
        let graphical = units
            .iter()
            .find(|u| u.name == "graphical.target")
            .expect("graphical.target discovered");
        assert!(graphical.unit.wants.contains("ddm.service"));
        assert!(graphical.unit.requires.contains("required.service"));
    }

    #[test]
    fn symlink_unit_folds_into_canonical_name() {
        let dir = temp_dir("alias");
        fs::write(
            dir.join("lightdm.service"),
            "[Unit]\nDescription=Display manager\n[Service]\nExecStart=/usr/sbin/lightdm\n",
        )
        .unwrap();
        std::os::unix::fs::symlink(
            dir.join("lightdm.service"),
            dir.join("display-manager.service"),
        )
        .unwrap();

        let mut units = Vec::new();
        let mut implicit = HashMap::new();
        load_units_from_dir_recursive(
            &dir,
            &[dir.to_string_lossy().into_owned()],
            &mut units,
            &mut implicit,
            None,
            false,
        )
        .unwrap();

        // The symlink is never its own unit: every entry that came from it
        // carries the canonical name `lightdm.service` plus the alias.
        assert!(!units.iter().any(|u| u.name == "display-manager.service"));
        assert!(units.iter().any(|u| u.name == "lightdm.service"
            && u.unit.description == "Display manager"
            && u.install
                .alias
                .iter()
                .any(|a| a == "display-manager.service")));
        // The symlink basename does not appear as a unit name anywhere.
        assert!(units.iter().all(|u| u.name != "display-manager.service"));
    }

    #[test]
    fn wants_dir_symlink_alias_edges_reference_canonical_name() {
        let dir = temp_dir("wantsalias");
        let unit_dir = dir.join("system");
        fs::create_dir_all(&unit_dir).unwrap();
        fs::write(
            unit_dir.join("graphical.target"),
            "[Unit]\nDescription=Graphical target\n",
        )
        .unwrap();
        fs::write(
            unit_dir.join("lightdm.service"),
            "[Unit]\nDescription=Display manager\n[Service]\nExecStart=/usr/sbin/lightdm\n",
        )
        .unwrap();
        // The enablement link uses the alias name; the edge must reference
        // the canonical unit so the scheduler sees exactly one lightdm.
        fs::create_dir_all(unit_dir.join("graphical.target.wants")).unwrap();
        std::os::unix::fs::symlink(
            unit_dir.join("lightdm.service"),
            unit_dir
                .join("graphical.target.wants")
                .join("display-manager.service"),
        )
        .unwrap();

        let mut units = Vec::new();
        let mut implicit = HashMap::new();
        load_units_from_dir_recursive(
            &unit_dir,
            &[unit_dir.to_string_lossy().into_owned()],
            &mut units,
            &mut implicit,
            None,
            false,
        )
        .unwrap();
        apply_implicit_deps(&mut units, &implicit);

        let graphical = units.iter().find(|u| u.name == "graphical.target").unwrap();
        assert!(graphical.unit.wants.contains("lightdm.service"));
        assert!(!graphical.unit.wants.contains("display-manager.service"));
    }

    #[test]
    fn masked_symlink_to_dev_null_is_skipped() {
        let dir = temp_dir("mask");
        fs::write(
            dir.join("iodined.service"),
            "[Unit]\nDescription=Real service\n[Service]\nExecStart=/usr/sbin/iodined\n",
        )
        .unwrap();
        // Masking a unit means symlinking it to /dev/null.
        std::os::unix::fs::symlink("/dev/null", dir.join("some-other.service")).unwrap();

        let mut units = Vec::new();
        let mut implicit = HashMap::new();
        load_units_from_dir_recursive(
            &dir,
            &[dir.to_string_lossy().into_owned()],
            &mut units,
            &mut implicit,
            None,
            false,
        )
        .unwrap();

        let names: Vec<String> = units.iter().map(|u| u.name.clone()).collect();
        assert_eq!(names, vec!["iodined.service".to_string()]);
    }

    #[test]
    fn bare_template_unit_is_discovered_as_a_definition() {
        let dir = temp_dir("tplskip");
        fs::write(
            dir.join("getty@.service"),
            "[Unit]\nDescription=Getty on %I\n[Service]\nExecStart=/sbin/agetty %I\nTTYPath=/dev/%I\n",
        )
        .unwrap();

        let mut units = Vec::new();
        let mut implicit = HashMap::new();
        load_units_from_dir_recursive(
            &dir,
            &[dir.to_string_lossy().into_owned()],
            &mut units,
            &mut implicit,
            None,
            false,
        )
        .unwrap();

        // The template is reported as a definition; its instance specifiers
        // are preserved for the consumer that materializes an instance.
        let tpl = units
            .iter()
            .find(|u| u.name == "getty@.service")
            .expect("template discovered");
        assert_eq!(tpl.unit.description, "Getty on %I");
        assert_eq!(tpl.service.as_ref().unwrap().tty_path, "/dev/%I");
        assert_eq!(tpl.service.as_ref().unwrap().exec_start[0].args, ["%I"]);
    }

    #[test]
    fn instance_drop_ins_are_discovered_as_a_specialization() {
        let dir = temp_dir("tpldropin");
        fs::write(
            dir.join("getty@.service"),
            "[Unit]\nDescription=Getty on %I\n[Service]\nExecStart=/sbin/agetty %I\n",
        )
        .unwrap();
        fs::create_dir_all(dir.join("getty@tty3.service.d")).unwrap();
        fs::write(
            dir.join("getty@tty3.service.d/override.conf"),
            "[Service]\nEnvironment=GETTY_MODE=special\n",
        )
        .unwrap();

        let mut units = Vec::new();
        let mut implicit = HashMap::new();
        let dirs = vec![dir.to_string_lossy().into_owned()];
        load_units_from_dir_recursive(&dir, &dirs, &mut units, &mut implicit, None, false).unwrap();
        discover_instance_specializations_in(&dirs, &mut units);

        let instance = units
            .iter()
            .find(|u| u.name == "getty@tty3.service")
            .expect("instance specialization discovered");
        assert_eq!(instance.unit.description, "Getty on tty3");
        assert_eq!(
            instance.service.as_ref().unwrap().environment,
            ["GETTY_MODE=special"]
        );
    }

    #[test]
    fn enabling_symlink_instance_gets_its_own_drop_ins() {
        let dir = temp_dir("tpldropin-wants");
        let unit_dir = dir.join("system");
        fs::create_dir_all(&unit_dir).unwrap();
        fs::write(
            unit_dir.join("getty@.service"),
            "[Unit]\nDescription=Getty on %I\n[Service]\nExecStart=/sbin/agetty %I\n",
        )
        .unwrap();
        fs::write(unit_dir.join("getty.target"), "[Unit]\nDescription=Getty\n").unwrap();
        // The enablement link is parsed from a `.wants/` path, where no
        // drop-in scope exists; the instance's own directory lives beside
        // the template and must still be folded in.
        fs::create_dir_all(unit_dir.join("getty.target.wants")).unwrap();
        std::os::unix::fs::symlink(
            unit_dir.join("getty@.service"),
            unit_dir.join("getty.target.wants/getty@tty1.service"),
        )
        .unwrap();
        fs::create_dir_all(unit_dir.join("getty@tty1.service.d")).unwrap();
        fs::write(
            unit_dir.join("getty@tty1.service.d/override.conf"),
            "[Service]\nEnvironment=GETTY_MODE=special\n",
        )
        .unwrap();

        let mut units = Vec::new();
        let mut implicit = HashMap::new();
        let dirs = vec![unit_dir.to_string_lossy().into_owned()];
        load_units_from_dir_recursive(&unit_dir, &dirs, &mut units, &mut implicit, None, false)
            .unwrap();
        discover_instance_specializations_in(&dirs, &mut units);

        let instance = units
            .iter()
            .find(|u| u.name == "getty@tty1.service")
            .expect("instance unit from enablement link");
        assert_eq!(instance.unit.description, "Getty on tty1");
        assert_eq!(
            instance.service.as_ref().unwrap().environment,
            ["GETTY_MODE=special"]
        );
        // The instance was re-parsed from the template to fold its drop-ins
        // in; the enablement evidence from the `.wants/` link must survive.
        assert!(instance.enabled);
    }

    #[test]
    fn discover_one_bare_template_returns_none() {
        let dir = temp_dir("tplone");
        fs::write(
            dir.join("getty@.service"),
            "[Unit]\nDescription=Getty on %I\n[Service]\nExecStart=/sbin/agetty %I\n",
        )
        .unwrap();

        let dirs = vec![dir.to_string_lossy().into_owned()];
        assert!(discover_one_in(&dirs, "getty@.service").unwrap().is_none());
        // Instances are still resolved through the template.
        let unit = discover_one_in(&dirs, "getty@tty3.service")
            .unwrap()
            .unwrap();
        assert_eq!(unit.name, "getty@tty3.service");
    }

    #[test]
    fn instance_enablement_symlink_becomes_instance_unit() {
        let dir = temp_dir("tplwant");
        let unit_dir = dir.join("system");
        fs::create_dir_all(&unit_dir).unwrap();
        fs::write(
            unit_dir.join("getty@.service"),
            "[Unit]\nDescription=Getty on %I\n[Service]\nExecStart=/sbin/agetty %I\nTTYPath=/dev/%I\n",
        )
        .unwrap();
        fs::write(unit_dir.join("getty.target"), "[Unit]\nDescription=Getty\n").unwrap();
        // The enablement link instantiates the template: the symlink basename
        // is `getty@tty1.service`, NOT an alias of `getty@.service`.
        fs::create_dir_all(unit_dir.join("getty.target.wants")).unwrap();
        std::os::unix::fs::symlink(
            unit_dir.join("getty@.service"),
            unit_dir
                .join("getty.target.wants")
                .join("getty@tty1.service"),
        )
        .unwrap();

        let mut units = Vec::new();
        let mut implicit = HashMap::new();
        load_units_from_dir_recursive(
            &unit_dir,
            &[unit_dir.to_string_lossy().into_owned()],
            &mut units,
            &mut implicit,
            None,
            false,
        )
        .unwrap();
        apply_implicit_deps(&mut units, &implicit);

        // The bare template is reported as a definition (with %I preserved),
        // never as a runnable instance.
        let template = units
            .iter()
            .find(|u| u.name == "getty@.service")
            .expect("template definition reported");
        assert_eq!(template.unit.description, "Getty on %I");
        // The enablement symlink is an instance unit, with %I expanded.
        let getty = units
            .iter()
            .find(|u| u.name == "getty@tty1.service")
            .unwrap();
        assert_eq!(getty.unit.description, "Getty on tty1");
        assert_eq!(getty.service.as_ref().unwrap().tty_path, "/dev/tty1");
        // The enablement link makes the instance a boot-set member; the bare
        // template stays a definition.
        assert!(getty.enabled);
        assert!(!template.enabled);
        // The wants edge references the instance, not the template.
        let target = units.iter().find(|u| u.name == "getty.target").unwrap();
        assert!(target.unit.wants.contains("getty@tty1.service"));
        assert!(!target.unit.wants.contains("getty@.service"));
    }

    // Enablement is part of the discovery result: the Finder is the component
    // that sees the unit search paths, so it decides enablement once instead
    // of letting every consumer re-scan the filesystem.
    #[test]
    fn wants_directory_marks_the_unit_enabled() {
        let dir = temp_dir("enabled-wants");
        let unit_dir = dir.join("system");
        fs::create_dir_all(&unit_dir).unwrap();
        fs::write(unit_dir.join("multi-user.target"), "[Unit]\n").unwrap();
        fs::write(
            unit_dir.join("foo.service"),
            "[Unit]\nDescription=Foo\n[Service]\nExecStart=/bin/true\n",
        )
        .unwrap();
        fs::create_dir_all(unit_dir.join("multi-user.target.wants")).unwrap();
        std::os::unix::fs::symlink(
            unit_dir.join("foo.service"),
            unit_dir.join("multi-user.target.wants/foo.service"),
        )
        .unwrap();

        let dirs = vec![unit_dir.to_string_lossy().into_owned()];
        let mut units = Vec::new();
        let mut implicit = HashMap::new();
        load_units_from_dir_recursive(&unit_dir, &dirs, &mut units, &mut implicit, None, false)
            .unwrap();

        // The plain definition in the search directory is not enablement ...
        assert!(units.iter().any(|u| u.name == "foo.service" && !u.enabled));
        // ... the link inside the `.wants/` directory is; the finder merges
        // the two copies into one enabled unit.
        assert!(units.iter().any(|u| u.name == "foo.service" && u.enabled));
        assert!(
            !units
                .iter()
                .find(|u| u.name == "multi-user.target")
                .unwrap()
                .enabled
        );
    }

    #[test]
    fn primary_enablement_link_marks_the_unit_enabled() {
        let root = temp_dir("enabled-primary");
        let etc = root.join("etc");
        let lib = root.join("lib");
        fs::create_dir_all(&etc).unwrap();
        fs::create_dir_all(&lib).unwrap();
        fs::write(
            lib.join("foo.service"),
            "[Unit]\nDescription=Foo\n[Service]\nExecStart=/bin/true\n",
        )
        .unwrap();
        // `/etc/systemd/system/foo.service` -> `/usr/lib/systemd/system/foo.service`
        std::os::unix::fs::symlink("../lib/foo.service", etc.join("foo.service")).unwrap();

        let dirs = vec![
            etc.to_string_lossy().into_owned(),
            lib.to_string_lossy().into_owned(),
        ];
        let mut units = Vec::new();
        let mut implicit = HashMap::new();
        for dir in &dirs {
            load_units_from_dir_recursive(
                Path::new(dir),
                &dirs,
                &mut units,
                &mut implicit,
                None,
                false,
            )
            .unwrap();
        }

        assert_eq!(units.len(), 2);
        assert!(units.iter().any(|u| u.name == "foo.service" && u.enabled));
        assert!(units.iter().any(|u| u.name == "foo.service" && !u.enabled));
    }

    #[test]
    fn alias_link_is_not_enablement() {
        let dir = temp_dir("enabled-alias");
        fs::write(
            dir.join("lightdm.service"),
            "[Unit]\nDescription=Display manager\n[Service]\nExecStart=/usr/sbin/lightdm\n",
        )
        .unwrap();
        // A second name for the same unit, not a boot-set membership.
        std::os::unix::fs::symlink("lightdm.service", dir.join("display-manager.service")).unwrap();

        let dirs = vec![dir.to_string_lossy().into_owned()];
        let mut units = Vec::new();
        let mut implicit = HashMap::new();
        load_units_from_dir_recursive(&dir, &dirs, &mut units, &mut implicit, None, false).unwrap();

        assert!(units.iter().all(|u| !u.enabled));
        assert!(units.iter().any(|u| u.name == "lightdm.service"));
    }

    #[test]
    fn template_enablement_link_enables_no_unit() {
        let dir = temp_dir("enabled-template");
        let unit_dir = dir.join("system");
        fs::create_dir_all(&unit_dir).unwrap();
        fs::write(
            unit_dir.join("getty@.service"),
            "[Unit]\nDescription=Getty on %I\n[Service]\nExecStart=/sbin/agetty %I\n",
        )
        .unwrap();
        fs::write(unit_dir.join("getty.target"), "[Unit]\n").unwrap();
        fs::create_dir_all(unit_dir.join("getty.target.wants")).unwrap();
        std::os::unix::fs::symlink(
            unit_dir.join("getty@.service"),
            unit_dir.join("getty.target.wants/getty@.service"),
        )
        .unwrap();

        let dirs = vec![unit_dir.to_string_lossy().into_owned()];
        let mut units = Vec::new();
        let mut implicit = HashMap::new();
        load_units_from_dir_recursive(&unit_dir, &dirs, &mut units, &mut implicit, None, false)
            .unwrap();

        // A template is a definition, never a boot-set member.
        assert!(units
            .iter()
            .filter(|u| u.name == "getty@.service")
            .all(|u| !u.enabled));
    }

    // The loader also reports the full `systemctl is-enabled` state, not just
    // the coarse enablement flag: install information alone decides
    // static/indirect/disabled, and the directory holding the link decides
    // enabled vs enabled-runtime.
    fn scan(dir: &Path, runtime: bool) -> Vec<UnitFile> {
        let dirs = vec![dir.to_string_lossy().into_owned()];
        let mut units = Vec::new();
        let mut implicit = HashMap::new();
        load_units_from_dir_recursive(dir, &dirs, &mut units, &mut implicit, None, runtime)
            .unwrap();
        units
    }

    fn has_state(units: &[UnitFile], name: &str, state: &str) -> bool {
        units
            .iter()
            .any(|u| u.name == name && u.file_state.as_deref() == Some(state))
    }

    #[test]
    fn file_state_follows_install_information() {
        let dir = temp_dir("file-state-install");
        fs::write(
            dir.join("disabled.service"),
            "[Unit]\nDescription=D\n[Install]\nWantedBy=multi-user.target\n",
        )
        .unwrap();
        fs::write(dir.join("static.service"), "[Unit]\nDescription=S\n").unwrap();
        fs::write(
            dir.join("indirect.service"),
            "[Unit]\nDescription=I\n[Install]\nAlso=static.service\n",
        )
        .unwrap();

        let units = scan(&dir, false);

        assert!(has_state(&units, "disabled.service", "disabled"));
        assert!(has_state(&units, "static.service", "static"));
        assert!(has_state(&units, "indirect.service", "indirect"));
    }

    #[test]
    fn file_state_distinguishes_runtime_enablement() {
        let dir = temp_dir("file-state-runtime");
        fs::write(dir.join("foo.target"), "[Unit]\n").unwrap();
        fs::write(
            dir.join("bar.service"),
            "[Unit]\nDescription=Bar\n[Service]\nExecStart=/bin/true\n",
        )
        .unwrap();
        fs::create_dir_all(dir.join("foo.target.wants")).unwrap();
        std::os::unix::fs::symlink(
            dir.join("bar.service"),
            dir.join("foo.target.wants/bar.service"),
        )
        .unwrap();

        assert!(has_state(&scan(&dir, false), "bar.service", "enabled"));
        assert!(has_state(
            &scan(&dir, true),
            "bar.service",
            "enabled-runtime"
        ));
    }

    #[test]
    fn file_state_reports_unit_file_links_as_linked() {
        let root = temp_dir("file-state-linked");
        let etc = root.join("etc");
        let lib = root.join("lib");
        fs::create_dir_all(&etc).unwrap();
        fs::create_dir_all(&lib).unwrap();
        fs::write(
            lib.join("foo.service"),
            "[Unit]\nDescription=Foo\n[Service]\nExecStart=/bin/true\n",
        )
        .unwrap();
        std::os::unix::fs::symlink("../lib/foo.service", etc.join("foo.service")).unwrap();

        assert!(has_state(&scan(&etc, false), "foo.service", "linked"));
        assert!(has_state(
            &scan(&etc, true),
            "foo.service",
            "linked-runtime"
        ));
    }

    // Incident regression: an instance pulled in only by an enablement link is
    // parsed from a `.wants/` path, where neither drop-in scope exists.  Its
    // template drop-ins live beside the template and must still be applied,
    // or the instance loses the Requires=/After= edges they carry (the
    // deepin user@<uid> + login-barrier case).
    #[test]
    fn wants_instance_inherits_template_dropin_edges() {
        let dir = temp_dir("wants-tpl-edges");
        let unit_dir = dir.join("system");
        fs::create_dir_all(&unit_dir).unwrap();
        fs::write(
            unit_dir.join("user@.service"),
            "[Unit]\nDescription=User manager %I\nAfter=base.service\n\
             [Service]\nExecStart=/usr/lib/systemd/systemd --user\n",
        )
        .unwrap();
        fs::write(unit_dir.join("multi-user.target"), "[Unit]\n").unwrap();
        fs::create_dir_all(unit_dir.join("multi-user.target.wants")).unwrap();
        std::os::unix::fs::symlink(
            unit_dir.join("user@.service"),
            unit_dir.join("multi-user.target.wants/user@1000.service"),
        )
        .unwrap();
        // A separate template drop-in adds the prerequisite edge.
        fs::create_dir_all(unit_dir.join("user@.service.d")).unwrap();
        fs::write(
            unit_dir.join("user@.service.d/10-login-barrier.conf"),
            "[Unit]\nAfter=systemd-user-sessions.service\nRequires=user-runtime-dir@%i.service\n",
        )
        .unwrap();

        let dirs = vec![unit_dir.to_string_lossy().into_owned()];
        let mut units = Vec::new();
        let mut implicit = HashMap::new();
        load_units_from_dir_recursive(&unit_dir, &dirs, &mut units, &mut implicit, None, false)
            .unwrap();

        let instance = units
            .iter()
            .find(|u| u.name == "user@1000.service")
            .expect("enablement link instance discovered");
        assert!(instance.unit.after.contains("base.service"));
        assert!(
            instance
                .unit
                .after
                .contains("systemd-user-sessions.service"),
            "template drop-in edges lost: {:?}",
            instance.unit.after
        );
        assert!(
            instance
                .unit
                .requires
                .contains("user-runtime-dir@1000.service"),
            "template drop-in requirement lost: {:?}",
            instance.unit.requires
        );
    }

    // systemd collects drop-ins from every unit load path, not only from the
    // directory that holds the unit file: an /etc drop-in extends a unit
    // shipped in /usr/lib.  Within one directory the instance's own drop-ins
    // are the more specific scope and override the template's.
    #[test]
    fn dropins_are_collected_from_every_search_path() {
        let root = temp_dir("cross-path");
        let etc = root.join("etc");
        let lib = root.join("lib");
        fs::create_dir_all(&etc).unwrap();
        fs::create_dir_all(&lib).unwrap();
        fs::write(
            lib.join("user@.service"),
            "[Unit]\nDescription=User manager %I\n\
             [Service]\nExecStart=/usr/lib/systemd/systemd --user\n",
        )
        .unwrap();
        fs::create_dir_all(lib.join("user@.service.d")).unwrap();
        fs::write(
            lib.join("user@.service.d/10-runtime-dir.conf"),
            "[Unit]\nRequires=user-runtime-dir@%i.service\nAfter=user-runtime-dir@%i.service\n",
        )
        .unwrap();
        fs::create_dir_all(etc.join("user@.service.d")).unwrap();
        fs::write(
            etc.join("user@.service.d/20-login.conf"),
            "[Unit]\nAfter=systemd-user-sessions.service\n",
        )
        .unwrap();
        fs::create_dir_all(etc.join("user@1000.service.d")).unwrap();
        fs::write(
            etc.join("user@1000.service.d/30-local.conf"),
            "[Unit]\nDescription=Local user manager\n",
        )
        .unwrap();

        let dirs = vec![
            etc.to_string_lossy().into_owned(),
            lib.to_string_lossy().into_owned(),
        ];
        let unit = discover_one_in(&dirs, "user@1000.service")
            .unwrap()
            .unwrap();

        assert!(unit.unit.requires.contains("user-runtime-dir@1000.service"));
        assert!(unit.unit.after.contains("user-runtime-dir@1000.service"));
        assert!(unit.unit.after.contains("systemd-user-sessions.service"));
        // The instance's drop-in is applied after the template's, so the more
        // specific scope wins for scalar settings too.
        assert_eq!(unit.unit.description, "Local user manager");
    }
}
