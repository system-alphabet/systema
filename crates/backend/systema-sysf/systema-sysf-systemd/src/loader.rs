use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use super::parser::{parse_unit_from_path, parse_unit_from_path_as};
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
/// instance name so `%i`/`%I` are expanded (see [`parse_unit_from_path`]).
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

/// Discover and parse all systemd unit files from standard search paths.
pub fn discover_all() -> Result<Vec<UnitFile>> {
    let mut units = Vec::new();
    // Units pulled in implicitly by `<unit>.wants/` / `<unit>.requires/`
    // directories, keyed by the name of the unit the directory belongs to
    // (e.g. `sockets.target.wants/dbus.socket` => sockets.target wants dbus.socket).
    let mut implicit: HashMap<String, (HashSet<String>, HashSet<String>)> = HashMap::new();
    for dir in sysa::paths::instance().unit_search_paths.iter() {
        let path = Path::new(dir);
        if path.exists() {
            load_units_from_dir_recursive(path, &mut units, &mut implicit, None).with_context(
                || {
                    sysa::l10n::fmt(
                        sysa::l10n::t_("Scanning {dir} ..."),
                        &[("dir", &dir.to_string())],
                    )
                },
            )?;
        }
    }
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
                match parse_unit_from_path_as(&path, name) {
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
                    let mut unit = parse_unit_from_path_as(&path, &canonical)?;
                    if !unit.install.alias.iter().any(|a| a == name) {
                        unit.install.alias.push(name.to_string());
                    }
                    Ok(unit)
                }
                None => parse_unit_from_path(&path),
            };
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

/// Fold the implicit dependency directories discovered during the scan into
/// the owning units' `Wants=`/`Requires=` sets.
fn apply_implicit_deps(
    units: &mut [UnitFile],
    implicit: &HashMap<String, (HashSet<String>, HashSet<String>)>,
) {
    for unit in units {
        if let Some((wants, requires)) = implicit.get(&unit.name) {
            unit.unit.wants.extend(wants.iter().cloned());
            unit.unit.requires.extend(requires.iter().cloned());
        }
    }
}

fn load_units_from_dir_recursive(
    dir: &Path,
    units: &mut Vec<UnitFile>,
    implicit: &mut HashMap<String, (HashSet<String>, HashSet<String>)>,
    ctx: Option<(String, DirDepKind)>,
) -> Result<usize> {
    let mut count = 0usize;
    count += load_units_from_dir(dir, units, implicit, ctx.as_ref())?;

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
                count += load_units_from_dir_recursive(&path, units, implicit, child_ctx)?;
            }
        }
    }

    Ok(count)
}

fn load_units_from_dir(
    dir: &Path,
    units: &mut Vec<UnitFile>,
    implicit: &mut HashMap<String, (HashSet<String>, HashSet<String>)>,
    ctx: Option<&(String, DirDepKind)>,
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
        // A template unit (e.g. `getty@.service`) is a definition, not a
        // runnable unit: only its instances (`getty@tty1.service`) are real
        // units.  Treating the bare template as a unit would let it be
        // started with unexpanded `%i`/`%I` specifiers (e.g. a getty with
        // `TTYPath=/dev/`).  It is never emitted to the committed set.
        if sysa::unit_name::is_template(&name) {
            debug!("Skipping template unit {}", name);
            continue;
        }
        // A unit-file symlink aliases its resolved target: the canonical unit
        // name is the target's basename and the symlink's basename becomes an
        // alias of it.  This mirrors systemd, where `display-manager.service`
        // -> `lightdm.service` is not a second unit but another name for the
        // same one.  Masking (-> /dev/null) is already skipped by `is_file()`.
        let unit = match symlink_alias(&path) {
            Some((canonical, alias)) => {
                let mut unit = match parse_unit_from_path_as(&path, &canonical) {
                    Ok(u) => u,
                    Err(e) => {
                        debug!("Skipping {}: {}", path.display(), e);
                        continue;
                    }
                };
                if !unit.install.alias.iter().any(|a| a == &alias) {
                    unit.install.alias.push(alias);
                }
                Ok(unit)
            }
            None => parse_unit_from_path(&path),
        };
        match unit {
            Ok(unit) => {
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
        load_units_from_dir_recursive(&dir, &mut units, &mut implicit, None).unwrap();
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
        load_units_from_dir_recursive(&unit_dir, &mut units, &mut implicit, None).unwrap();
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
        load_units_from_dir_recursive(&unit_dir, &mut units, &mut implicit, None).unwrap();
        apply_implicit_deps(&mut units, &implicit);

        let target = units.iter().find(|u| u.name == "target.service").unwrap();
        assert!(target.unit.requires.contains("dep.service"));
        assert!(!target.unit.wants.contains("dep.service"));
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
        load_units_from_dir_recursive(&dir, &mut units, &mut implicit, None).unwrap();

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
        load_units_from_dir_recursive(&unit_dir, &mut units, &mut implicit, None).unwrap();
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
        load_units_from_dir_recursive(&dir, &mut units, &mut implicit, None).unwrap();

        let names: Vec<String> = units.iter().map(|u| u.name.clone()).collect();
        assert_eq!(names, vec!["iodined.service".to_string()]);
    }

    #[test]
    fn bare_template_unit_is_skipped_in_discovery() {
        let dir = temp_dir("tplskip");
        fs::write(
            dir.join("getty@.service"),
            "[Unit]\nDescription=Getty on %I\n[Service]\nExecStart=/sbin/agetty %I\nTTYPath=/dev/%I\n",
        )
        .unwrap();

        let mut units = Vec::new();
        let mut implicit = HashMap::new();
        load_units_from_dir_recursive(&dir, &mut units, &mut implicit, None).unwrap();

        assert!(units.iter().all(|u| u.name != "getty@.service"));
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
        load_units_from_dir_recursive(&unit_dir, &mut units, &mut implicit, None).unwrap();
        apply_implicit_deps(&mut units, &implicit);

        // The bare template is never a unit.
        assert!(units.iter().all(|u| u.name != "getty@.service"));
        // The enablement symlink is an instance unit, with %I expanded.
        let getty = units
            .iter()
            .find(|u| u.name == "getty@tty1.service")
            .unwrap();
        assert_eq!(getty.unit.description, "Getty on tty1");
        assert_eq!(getty.service.as_ref().unwrap().tty_path, "/dev/tty1");
        // The wants edge references the instance, not the template.
        let target = units.iter().find(|u| u.name == "getty.target").unwrap();
        assert!(target.unit.wants.contains("getty@tty1.service"));
        assert!(!target.unit.wants.contains("getty@.service"));
    }
}
