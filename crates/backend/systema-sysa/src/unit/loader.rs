//! Unit file loader: discovers and loads unit files from standard search paths.
//!
//! This module handles:
//! - Loading a single unit file from the standard search paths
//! - Symlink alias resolution
//! - Default dependency injection

use std::path::Path;

use anyhow::Result;

use super::types::UnitFile;
use crate::state::AllocatorHandle;

/// systemd special target names (`SPECIAL_*` in special.h).
const TARGET_SYSINIT: &str = "sysinit.target";
const TARGET_BASIC: &str = "basic.target";
const TARGET_SHUTDOWN: &str = "shutdown.target";
const TARGET_TIMERS: &str = "timers.target";
const TARGET_SOCKETS: &str = "sockets.target";
const TARGET_PATHS: &str = "paths.target";
const TARGET_LOCAL_FS: &str = "local-fs.target";
const TARGET_LOCAL_FS_PRE: &str = "local-fs-pre.target";
const TARGET_REMOTE_FS: &str = "remote-fs.target";
const TARGET_REMOTE_FS_PRE: &str = "remote-fs-pre.target";
const TARGET_UMOUNT: &str = "umount.target";
const TARGET_SWAP: &str = "swap.target";
const TARGET_NETWORK: &str = "network.target";
const TARGET_NETWORK_ONLINE: &str = "network-online.target";
const TARGET_TIME_SYNC: &str = "time-sync.target";
const TARGET_TIME_SET: &str = "time-set.target";

/// Load a unit file from disk by name.
///
/// Parses `<search-path>/<name>` verbatim.  There is no template fallback: an
/// instance unit (`foo@bar.service`) is materialized from the Finder-reported
/// IR template by `unit::template::materialize`, so the template file is never
/// re-read here (see [`load_unit_flexible_in`]).
///
/// Returns an error if no exact file exists.
pub fn load_unit_flexible(name: &str) -> Result<UnitFile> {
    load_unit_flexible_in(&sysa::paths::instance().unit_search_paths, name)
}

/// [`load_unit_flexible`] over an explicit search-path list (testable
/// without touching the global path configuration).
///
/// Only an exact unit file is loaded.  There is deliberately no template
/// fallback: an instance (`foo@bar.service`) is a definition materialized
/// from the Finder-reported IR template, and reading the template file here
/// would bypass that definition's committed drop-ins.
fn load_unit_flexible_in(dirs: &[String], name: &str) -> Result<UnitFile> {
    // A bare template (e.g. `getty@.service`) is a definition, not a runnable
    // unit: loading it would let `systemctl start getty@.service` dispatch a
    // unit whose `%i`/`%I` specifiers expand to nothing (e.g. a getty with
    // `TTYPath=/dev/`).  Refuse the load so a template is never materialised
    // as a unit.  Instances are handled by `unit::template::materialize`.
    if sysa::unit_name::is_template(name) {
        anyhow::bail!(sysa::l10n::fmt(
            sysa::l10n::t_("Unit {name} is a template and cannot be loaded directly"),
            &[("name", name)],
        ));
    }
    for dir in dirs {
        let path = std::path::Path::new(dir).join(name);
        if path.is_file() {
            return load_unit_file(dirs, &path);
        }
    }

    anyhow::bail!(sysa::l10n::fmt(
        sysa::l10n::t_("Unit not found: {name}"),
        &[("name", name)],
    ))
}

/// Load a unit file from disk by name, resolving unit-file symlink aliases
/// to their canonical unit name.
///
/// systemd treats a symlink such as `/etc/systemd/system/display-manager.service
/// -> /usr/lib/systemd/system/lightdm.service` as an *alias* of the target
/// unit, not a separate unit.  Loading the symlink under its own basename
/// would create a duplicate unit that starts a second copy of the same
/// program.  This helper therefore parses the file under the resolved target's
/// basename and records the symlink's basename as an alias, exactly like the
/// full discovery path (`systema-sysf`'s loader).  Masked units (symlinks to
/// `/dev/null`) fall through to a plain parse, which fails harmlessly.
fn load_unit_file(dirs: &[String], path: &Path) -> Result<UnitFile> {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return parse_unit_at(dirs, path);
    };
    if !meta.file_type().is_symlink() {
        return parse_unit_at(dirs, path);
    }
    let Ok(target) = std::fs::canonicalize(path) else {
        return parse_unit_at(dirs, path);
    };
    let Some(canonical) = target.file_name().and_then(|n| n.to_str()) else {
        return parse_unit_at(dirs, path);
    };
    let alias = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();
    if canonical == alias {
        return parse_unit_at(dirs, path);
    }
    // An enablement symlink `foo@bar.service -> foo@.service` is an
    // *instance* of the template, not an alias: folding it into the
    // canonical template name would instantiate the template with an empty
    // `%i` (e.g. a getty with `TTYPath=/dev/`).  Parse it under the
    // symlink's own instance name instead, mirroring `systema-sysf`'s
    // `symlink_alias`.
    if sysa::unit_name::is_template(canonical)
        && sysa::unit_name::template_of(&alias).as_deref() == Some(canonical)
    {
        return parse_unit_at(dirs, path);
    }
    let mut unit = super::parser::parse_unit_from_path_as_in(dirs, path, canonical)?;
    if !unit.install.alias.iter().any(|a| a == &alias) {
        unit.install.alias.push(alias);
    }
    Ok(unit)
}

/// Parse `path` under its own file name, resolving its drop-ins from the unit
/// search paths.
fn parse_unit_at(dirs: &[String], path: &Path) -> Result<UnitFile> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");
    super::parser::parse_unit_from_path_as_in(dirs, path, name)
}

// --------------------------------------------------------------------------
// Default dependencies
// --------------------------------------------------------------------------

/// Inject systemd's default dependencies into the loaded unit set.
///
/// This mirrors systemd's `unit_add_default_dependencies()` per unit type
/// (service/timer/socket/path/slice/scope/mount/swap) plus the cross-unit
/// mount-point references of `unit_add_mounts_for()` /
/// `mount_add_mount_dependencies()` (`RequiresMountsFor=`/`WantsMountsFor=`
/// and the parent-directory/source-path mount chains).
///
/// Deviation from systemd: systemd loads the special targets while adding
/// the edges and *fails the unit load* if they are missing; systema only
/// wires an edge when the referenced unit is present in the loaded set, so
/// a partial unit set cannot break ordinary starts.
pub fn inject_default_dependencies(allocator: AllocatorHandle) {
    let names: Vec<String> = {
        let state = allocator.read();
        state.units.keys().cloned().collect()
    };

    // Pass 1: per-type defaults, gated on DefaultDependencies=.  Target
    // presence is probed against the name snapshot.
    let present: std::collections::HashSet<String> = names.iter().cloned().collect();
    {
        let mut state = allocator.write();
        for name in &names {
            let Some(unit) = state.units.get_mut(name) else {
                continue;
            };
            if !unit.unit.default_dependencies {
                continue;
            }
            add_type_default_dependencies(unit, &present);
        }
    }

    // Pass 2: mount-point cross references.  These are file-level
    // dependencies in systemd and are NOT gated on DefaultDependencies=.
    // Compute all edges from an immutable snapshot, then apply them.
    let mut edges: Vec<(String, String, MountDep)> = Vec::new();
    {
        let state = allocator.read();
        let units = &state.units;
        for name in &names {
            let Some(unit) = units.get(name) else {
                continue;
            };
            collect_mount_dependencies(unit, units, &mut edges);
        }
    }
    {
        let mut state = allocator.write();
        for (src, target, dep) in edges {
            if !state.units.contains_key(&target) {
                continue;
            }
            let Some(unit) = state.units.get_mut(&src) else {
                continue;
            };
            match dep {
                MountDep::Requires => {
                    unit.unit.requires.insert(target);
                }
                MountDep::Wants => {
                    unit.unit.wants.insert(target);
                }
                MountDep::After => {
                    unit.unit.after.insert(target);
                }
            }
        }
    }
}

/// `Requires=`/`Wants=`/`After=` edges created for mount-point references.
#[derive(Clone, Copy)]
enum MountDep {
    Requires,
    Wants,
    After,
}

/// Add a single default dependency, but only if the target unit exists in
/// the loaded set (see [`inject_default_dependencies`]).
fn add_default_dependency(unit: &mut UnitFile, field: Field, target: &str, present: bool) {
    if !present {
        return;
    }
    match field {
        Field::Requires => {
            unit.unit.requires.insert(target.to_string());
        }
        Field::Wants => {
            unit.unit.wants.insert(target.to_string());
        }
        Field::After => {
            unit.unit.after.insert(target.to_string());
        }
        Field::Before => {
            unit.unit.before.insert(target.to_string());
        }
        Field::Conflicts => {
            unit.unit.conflicts.insert(target.to_string());
        }
    }
}

/// Dependency fields a default edge can land in.
#[derive(Clone, Copy)]
enum Field {
    Requires,
    Wants,
    After,
    Before,
    Conflicts,
}

/// Add per-unit-type defaults (systemd `*_add_default_dependencies()`).
///
/// All targets are probed against the loaded unit-name snapshot; edges to
/// missing units are skipped.
fn add_type_default_dependencies(unit: &mut UnitFile, present: &std::collections::HashSet<String>) {
    let has = |target: &str| present.contains(target);
    match unit.kind {
        crate::unit::types::UnitKind::Service => {
            add_default_dependency(unit, Field::Requires, TARGET_SYSINIT, has(TARGET_SYSINIT));
            add_default_dependency(unit, Field::After, TARGET_SYSINIT, has(TARGET_SYSINIT));
            add_default_dependency(unit, Field::After, TARGET_BASIC, has(TARGET_BASIC));
            add_default_dependency(unit, Field::Before, TARGET_SHUTDOWN, has(TARGET_SHUTDOWN));
            add_default_dependency(
                unit,
                Field::Conflicts,
                TARGET_SHUTDOWN,
                has(TARGET_SHUTDOWN),
            );
        }
        crate::unit::types::UnitKind::Timer => {
            add_default_dependency(unit, Field::Before, TARGET_TIMERS, has(TARGET_TIMERS));
            add_default_dependency(unit, Field::Requires, TARGET_SYSINIT, has(TARGET_SYSINIT));
            add_default_dependency(unit, Field::After, TARGET_SYSINIT, has(TARGET_SYSINIT));
            add_default_dependency(unit, Field::Before, TARGET_SHUTDOWN, has(TARGET_SHUTDOWN));
            add_default_dependency(
                unit,
                Field::Conflicts,
                TARGET_SHUTDOWN,
                has(TARGET_SHUTDOWN),
            );
            if unit
                .timer
                .as_ref()
                .is_some_and(|t| !t.on_calendar.is_empty())
            {
                add_default_dependency(unit, Field::After, TARGET_TIME_SYNC, has(TARGET_TIME_SYNC));
                add_default_dependency(unit, Field::After, TARGET_TIME_SET, has(TARGET_TIME_SET));
            }
        }
        crate::unit::types::UnitKind::Socket => {
            add_default_dependency(unit, Field::Before, TARGET_SOCKETS, has(TARGET_SOCKETS));
            add_default_dependency(unit, Field::Requires, TARGET_SYSINIT, has(TARGET_SYSINIT));
            add_default_dependency(unit, Field::After, TARGET_SYSINIT, has(TARGET_SYSINIT));
            add_default_dependency(unit, Field::Before, TARGET_SHUTDOWN, has(TARGET_SHUTDOWN));
            add_default_dependency(
                unit,
                Field::Conflicts,
                TARGET_SHUTDOWN,
                has(TARGET_SHUTDOWN),
            );
        }
        crate::unit::types::UnitKind::Path => {
            add_default_dependency(unit, Field::Before, TARGET_PATHS, has(TARGET_PATHS));
            add_default_dependency(unit, Field::Requires, TARGET_SYSINIT, has(TARGET_SYSINIT));
            add_default_dependency(unit, Field::After, TARGET_SYSINIT, has(TARGET_SYSINIT));
            add_default_dependency(unit, Field::Before, TARGET_SHUTDOWN, has(TARGET_SHUTDOWN));
            add_default_dependency(
                unit,
                Field::Conflicts,
                TARGET_SHUTDOWN,
                has(TARGET_SHUTDOWN),
            );
        }
        crate::unit::types::UnitKind::Slice => {
            add_default_dependency(unit, Field::Before, TARGET_SHUTDOWN, has(TARGET_SHUTDOWN));
            add_default_dependency(
                unit,
                Field::Conflicts,
                TARGET_SHUTDOWN,
                has(TARGET_SHUTDOWN),
            );
        }
        crate::unit::types::UnitKind::Scope => {
            add_default_dependency(unit, Field::Before, TARGET_SHUTDOWN, has(TARGET_SHUTDOWN));
            add_default_dependency(
                unit,
                Field::Conflicts,
                TARGET_SHUTDOWN,
                has(TARGET_SHUTDOWN),
            );
        }
        crate::unit::types::UnitKind::Mount => {
            add_mount_default_dependencies(unit, has);
        }
        crate::unit::types::UnitKind::Swap => {
            add_swap_default_dependencies(unit, has);
        }
        _ => {}
    }
}

/// The `mount_add_default_dependencies()` equivalent: ordering against the
/// local/remote filesystem targets, umount.target, swap.target (tmpfs), and
/// the network targets for network filesystems.
///
/// This carries no mount-point knowledge of its own.  Whether a mount point
/// is worth managing is decided once, by System M while it discovers mounts
/// ([`sysa::mounts`]); whatever reaches System A as a `.mount` unit is by
/// definition one it should order.
fn add_mount_default_dependencies<F>(unit: &mut UnitFile, has: F)
where
    F: Fn(&str) -> bool,
{
    let Some(mnt) = unit.mount.clone() else {
        return;
    };

    let network = mount_is_network(&mnt);

    // mount_add_default_ordering_dependencies().
    let (after, before) = if network {
        (TARGET_REMOTE_FS_PRE, TARGET_REMOTE_FS)
    } else {
        (TARGET_LOCAL_FS_PRE, TARGET_LOCAL_FS)
    };
    if !mount_is_nofail(unit) {
        add_default_dependency(unit, Field::Before, before, has(before));
    }
    add_default_dependency(unit, Field::After, after, has(after));
    add_default_dependency(unit, Field::Before, TARGET_UMOUNT, has(TARGET_UMOUNT));
    add_default_dependency(unit, Field::Conflicts, TARGET_UMOUNT, has(TARGET_UMOUNT));
    if mnt.type_ == "tmpfs" {
        add_default_dependency(unit, Field::After, TARGET_SWAP, has(TARGET_SWAP));
    }

    // mount_add_default_network_dependencies().
    if network {
        add_default_dependency(unit, Field::After, TARGET_NETWORK, has(TARGET_NETWORK));
        add_default_dependency(
            unit,
            Field::Wants,
            TARGET_NETWORK_ONLINE,
            has(TARGET_NETWORK_ONLINE),
        );
        add_default_dependency(
            unit,
            Field::After,
            TARGET_NETWORK_ONLINE,
            has(TARGET_NETWORK_ONLINE),
        );
    }
}

/// The `swap_add_default_dependencies()` equivalent.
fn add_swap_default_dependencies<F>(unit: &mut UnitFile, has: F)
where
    F: Fn(&str) -> bool,
{
    let Some(swap) = &unit.swap else {
        return;
    };
    let netdev = fstab_test_option(&swap.options, "_netdev");
    if netdev {
        add_default_dependency(
            unit,
            Field::After,
            TARGET_REMOTE_FS_PRE,
            has(TARGET_REMOTE_FS_PRE),
        );
        add_default_dependency(unit, Field::Before, TARGET_REMOTE_FS, has(TARGET_REMOTE_FS));
        add_default_dependency(unit, Field::After, TARGET_NETWORK, has(TARGET_NETWORK));
        add_default_dependency(
            unit,
            Field::Wants,
            TARGET_NETWORK_ONLINE,
            has(TARGET_NETWORK_ONLINE),
        );
        add_default_dependency(
            unit,
            Field::After,
            TARGET_NETWORK_ONLINE,
            has(TARGET_NETWORK_ONLINE),
        );
    } else {
        add_default_dependency(unit, Field::Before, TARGET_SWAP, has(TARGET_SWAP));
    }
    add_default_dependency(unit, Field::Before, TARGET_UMOUNT, has(TARGET_UMOUNT));
    add_default_dependency(unit, Field::Conflicts, TARGET_UMOUNT, has(TARGET_UMOUNT));
}

/// Whether `options` contains the fstab option `want` (comma-separated),
/// mirroring `fstab_test_option()`.
fn fstab_test_option(options: &str, want: &str) -> bool {
    options.split(',').any(|o| o.trim() == want)
}

/// `mount_is_network()`: `_netdev` option or a network filesystem type.
fn mount_is_network(mnt: &crate::unit::types::MountSection) -> bool {
    if fstab_test_option(&mnt.options, "_netdev") {
        return true;
    }
    fstype_is_network(&mnt.type_)
}

/// `fstype_is_network()`: the `@network` set plus the extra types checked in
/// systemd's function.
fn fstype_is_network(fstype: &str) -> bool {
    let fstype = fstype.strip_prefix("fuse.").unwrap_or(fstype);
    matches!(
        fstype,
        "afs"
            | "ceph"
            | "cifs"
            | "gfs"
            | "gfs2"
            | "ncp"
            | "ncpfs"
            | "nfs"
            | "nfs4"
            | "ocfs2"
            | "orangefs"
            | "pvfs2"
            | "smb3"
            | "smbfs"
            | "davfs"
            | "glusterfs"
            | "lustre"
            | "sshfs"
    )
}

/// `mount_is_nofail()`: `nofail` beats `fail` in the mount options.
fn mount_is_nofail(unit: &UnitFile) -> bool {
    let Some(mnt) = &unit.mount else {
        return false;
    };
    let mut has_nofail = false;
    let mut has_fail = false;
    for opt in mnt.options.split(',') {
        match opt.trim() {
            "nofail" => has_nofail = true,
            "fail" => has_fail = true,
            _ => {}
        }
    }
    has_nofail && !has_fail
}

/// `mount_is_bind()`: `bind`/`rbind` option or a `bind`/`rbind` type.
fn mount_is_bind(mnt: &crate::unit::types::MountSection) -> bool {
    if fstab_test_option(&mnt.options, "bind") || fstab_test_option(&mnt.options, "rbind") {
        return true;
    }
    matches!(mnt.type_.as_str(), "bind" | "rbind")
}

/// `mount_is_loop()`: `loop` mount option.
fn mount_is_loop(mnt: &crate::unit::types::MountSection) -> bool {
    fstab_test_option(&mnt.options, "loop")
}

/// Escape a path into a unit name (`unit_name_path_escape()`), so
/// `/mnt/data` becomes `mnt-data` and `/` becomes `-`.
fn escape_unit_name_path(path: &str) -> String {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        return "-".to_string();
    }
    let mut out = String::with_capacity(trimmed.len());
    for (i, ch) in trimmed.chars().enumerate() {
        if ch == '/' {
            out.push('-');
        } else {
            let valid = ch.is_ascii_alphanumeric() || matches!(ch, ':' | '.' | '_');
            let escaped = (i == 0 && ch == '.') || matches!(ch, '-' | '\\') || !valid;
            if escaped {
                out.push('\\');
                out.push('x');
                out.push_str(&format!("{:02x}", ch as u32 & 0xff));
            } else {
                out.push(ch);
            }
        }
    }
    out
}

/// Collect the `RequiresMountsFor=`/`WantsMountsFor=` edges of `unit` and,
/// for mount units, the parent-directory and source-path mount chains
/// (`mount_add_mount_dependencies()`).
fn collect_mount_dependencies(
    unit: &UnitFile,
    units: &std::collections::HashMap<String, UnitFile>,
    edges: &mut Vec<(String, String, MountDep)>,
) {
    // RequiresMountsFor= / WantsMountsFor=: every mount unit covering the
    // path (path itself and all ancestors) is pulled in.
    for (paths, dep) in [
        (&unit.unit.requires_mounts_for, MountDep::Requires),
        (&unit.unit.wants_mounts_for, MountDep::Wants),
    ] {
        for path in paths {
            for (mount_name, mount_unit) in units {
                if mount_name == &unit.name {
                    continue;
                }
                if mount_unit.kind != crate::unit::types::UnitKind::Mount {
                    continue;
                }
                let Some(mnt) = &mount_unit.mount else {
                    continue;
                };
                if path_is_ancestor_or_self(&mnt.where_, path) {
                    edges.push((unit.name.clone(), mount_name.clone(), MountDep::After));
                    edges.push((unit.name.clone(), mount_name.clone(), dep));
                }
            }
        }
    }

    if unit.kind != crate::unit::types::UnitKind::Mount {
        return;
    }
    let Some(mnt) = &unit.mount else {
        return;
    };

    // Parent mount points (mount_add_mount_dependencies()).
    if mnt.where_ != "/" {
        if let Some(parent) = parent_dir(&mnt.where_) {
            add_mounts_for_path(unit, &parent, units, edges, MountDep::Requires);
        }
    }

    // Source path mount points, for bind/loop or non-network mounts.
    let what = mnt.what.trim_end_matches('/');
    if what.starts_with('/') && (mount_is_bind(mnt) || mount_is_loop(mnt) || !mount_is_network(mnt))
    {
        add_mounts_for_path(unit, what, units, edges, MountDep::Requires);
    }

    // Block-device dependency (mount_add_device_dependencies()): when the
    // mount source is a real device path, pull in and order after its
    // .device unit if one is loaded.  systema has no udev enumeration, so
    // `mount_is_bound_to_device()` (BindsTo=) is not implemented and the
    // StopPropagatedFrom= node dependency is omitted.
    if !mount_is_bind(mnt)
        && what.starts_with("/dev/")
        && !matches!(what, "/dev/root" | "/dev/nfs")
        && mnt.where_ != "/"
    {
        let device_name = format!("{}.device", escape_unit_name_path(what));
        if units.contains_key(&device_name) {
            edges.push((unit.name.clone(), device_name.clone(), MountDep::Requires));
            edges.push((unit.name.clone(), device_name, MountDep::After));
        }
    }
}

/// `unit_add_mounts_for(..., REQUIRES)` for a single path: Requires + After
/// on every mount unit whose mount point is `path` or an ancestor of it.
fn add_mounts_for_path(
    unit: &UnitFile,
    path: &str,
    units: &std::collections::HashMap<String, UnitFile>,
    edges: &mut Vec<(String, String, MountDep)>,
    dep: MountDep,
) {
    for (mount_name, mount_unit) in units {
        if mount_name == &unit.name {
            continue;
        }
        if mount_unit.kind != crate::unit::types::UnitKind::Mount {
            continue;
        }
        let Some(mnt) = &mount_unit.mount else {
            continue;
        };
        if path_is_ancestor_or_self(&mnt.where_, path) {
            edges.push((unit.name.clone(), mount_name.clone(), MountDep::After));
            edges.push((unit.name.clone(), mount_name.clone(), dep));
        }
    }
}

/// True if `path` is equal to or below `mount_point` (pathwise).
fn path_is_ancestor_or_self(mount_point: &str, path: &str) -> bool {
    let mp = mount_point.trim_end_matches('/');
    let p = path.trim_end_matches('/');
    if mp.is_empty() {
        return true;
    }
    p == mp || p.starts_with(&format!("{mp}/"))
}

/// Parent directory of a path (`path_extract_directory()`), or None for `/`.
fn parent_dir(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    let idx = trimmed.rfind('/')?;
    if idx == 0 {
        return Some("/".to_string());
    }
    Some(trimmed[..idx].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Allocator;
    use crate::unit::types::{MountSection, SwapSection, UnitKind};

    // =========================================================================
    // Default dependency injection
    // =========================================================================

    fn mount_unit(name: &str, where_: &str, what: &str, type_: &str, options: &str) -> UnitFile {
        let mut u = UnitFile::new(name);
        u.kind = UnitKind::Mount;
        u.mount = Some(MountSection {
            what: what.to_string(),
            where_: where_.to_string(),
            type_: type_.to_string(),
            options: options.to_string(),
            ..Default::default()
        });
        u
    }

    fn target(name: &str) -> UnitFile {
        let mut u = UnitFile::new(name);
        u.kind = UnitKind::Target;
        u
    }

    fn service(name: &str) -> UnitFile {
        UnitFile::new(name)
    }

    fn run_injection(units: Vec<UnitFile>) -> std::collections::HashMap<String, UnitFile> {
        let alloc = Allocator::handle();
        {
            let mut state = alloc.write();
            for u in units {
                state.units.insert(u.name.clone(), u);
            }
        }
        inject_default_dependencies(alloc.clone());
        let state = alloc.read();
        state.units.clone()
    }

    #[test]
    fn test_service_default_dependencies() {
        let units = run_injection(vec![
            service("sshd.service"),
            target("sysinit.target"),
            target("basic.target"),
            target("shutdown.target"),
        ]);
        let sshd = &units["sshd.service"];
        assert!(sshd.unit.requires.contains("sysinit.target"));
        assert!(sshd.unit.after.contains("sysinit.target"));
        assert!(sshd.unit.after.contains("basic.target"));
        assert!(sshd.unit.before.contains("shutdown.target"));
        assert!(sshd.unit.conflicts.contains("shutdown.target"));
    }

    #[test]
    fn test_service_default_dependencies_skip_missing_targets() {
        // No special targets loaded: no edges must be created.
        let units = run_injection(vec![service("sshd.service")]);
        let sshd = &units["sshd.service"];
        assert!(sshd.unit.requires.is_empty());
        assert!(sshd.unit.after.is_empty());
        assert!(sshd.unit.conflicts.is_empty());
    }

    #[test]
    fn test_default_dependencies_no_skips_everything() {
        let mut svc = service("sshd.service");
        svc.unit.default_dependencies = false;
        let units = run_injection(vec![
            svc,
            target("sysinit.target"),
            target("shutdown.target"),
        ]);
        let sshd = &units["sshd.service"];
        assert!(sshd.unit.requires.is_empty());
        assert!(sshd.unit.before.is_empty());
        assert!(sshd.unit.conflicts.is_empty());
    }

    #[test]
    fn test_mount_default_dependencies_local() {
        let units = run_injection(vec![
            mount_unit(
                "mnt-data.mount",
                "/mnt/data",
                "/dev/sda1",
                "ext4",
                "defaults",
            ),
            target("local-fs-pre.target"),
            target("local-fs.target"),
            target("umount.target"),
        ]);
        let mnt = &units["mnt-data.mount"];
        assert!(mnt.unit.after.contains("local-fs-pre.target"));
        assert!(mnt.unit.before.contains("local-fs.target"));
        assert!(mnt.unit.before.contains("umount.target"));
        assert!(mnt.unit.conflicts.contains("umount.target"));
        assert!(!mnt.unit.after.contains("remote-fs-pre.target"));
        assert!(!mnt.unit.requires.contains("swap.target"));
    }

    #[test]
    fn test_mount_default_dependencies_network() {
        let units = run_injection(vec![
            mount_unit(
                "mnt-nfs.mount",
                "/mnt/nfs",
                "server:/export",
                "nfs",
                "defaults",
            ),
            target("remote-fs-pre.target"),
            target("remote-fs.target"),
            target("network.target"),
            target("network-online.target"),
            target("umount.target"),
        ]);
        let mnt = &units["mnt-nfs.mount"];
        assert!(mnt.unit.after.contains("remote-fs-pre.target"));
        assert!(mnt.unit.before.contains("remote-fs.target"));
        assert!(mnt.unit.after.contains("network.target"));
        assert!(mnt.unit.wants.contains("network-online.target"));
        assert!(mnt.unit.after.contains("network-online.target"));
        assert!(!mnt.unit.before.contains("local-fs.target"));
    }

    #[test]
    fn test_mount_default_dependencies_nofail_skips_before() {
        let units = run_injection(vec![
            mount_unit("mnt-data.mount", "/mnt/data", "/dev/sda1", "ext4", "nofail"),
            target("local-fs-pre.target"),
            target("local-fs.target"),
            target("umount.target"),
        ]);
        let mnt = &units["mnt-data.mount"];
        assert!(!mnt.unit.before.contains("local-fs.target"));
        assert!(mnt.unit.after.contains("local-fs-pre.target"));
    }

    #[test]
    fn test_mount_default_dependencies_tmpfs_after_swap() {
        let units = run_injection(vec![
            mount_unit("run-test.mount", "/run/test", "tmpfs", "tmpfs", "defaults"),
            target("local-fs-pre.target"),
            target("local-fs.target"),
            target("umount.target"),
            target("swap.target"),
        ]);
        let mnt = &units["run-test.mount"];
        assert!(mnt.unit.after.contains("swap.target"));
    }

    /// System A must not special-case mount points.  System M drops API and
    /// extrinsic mount points during discovery, so a `.mount` unit that
    /// arrives here always gets the normal default dependencies — including
    /// the OS-base paths that used to be blacklisted inline.
    #[test]
    fn test_mount_default_dependencies_have_no_path_knowledge() {
        for where_ in ["/", "/usr", "/proc", "/sys", "/dev", "/run/initramfs/x"] {
            let name = format!("x-{}.mount", where_.replace('/', "-"));
            let units = run_injection(vec![
                mount_unit(&name, where_, "/dev/sda1", "ext4", "defaults"),
                target("local-fs-pre.target"),
                target("local-fs.target"),
                target("umount.target"),
            ]);
            let mnt = &units[&name];
            assert!(
                mnt.unit.before.contains("local-fs.target"),
                "where_={where_}"
            );
            assert!(
                mnt.unit.after.contains("local-fs-pre.target"),
                "where_={where_}"
            );
            assert!(mnt.unit.before.contains("umount.target"), "where_={where_}");
            assert!(
                mnt.unit.conflicts.contains("umount.target"),
                "where_={where_}"
            );
        }
    }

    #[test]
    fn test_mount_parent_chain_dependencies() {
        let units = run_injection(vec![
            mount_unit("data.mount", "/data", "/dev/sda1", "ext4", "defaults"),
            mount_unit(
                "data-sub.mount",
                "/data/sub",
                "/dev/sda2",
                "ext4",
                "defaults",
            ),
        ]);
        let sub = &units["data-sub.mount"];
        assert!(sub.unit.requires.contains("data.mount"));
        assert!(sub.unit.after.contains("data.mount"));
    }

    #[test]
    fn test_requires_mounts_for() {
        let mut svc = service("app.service");
        svc.unit.requires_mounts_for.push("/data".to_string());
        let units = run_injection(vec![
            svc,
            mount_unit("data.mount", "/data", "/dev/sda1", "ext4", "defaults"),
        ]);
        let app = &units["app.service"];
        assert!(app.unit.requires.contains("data.mount"));
        assert!(app.unit.after.contains("data.mount"));
    }

    #[test]
    fn test_wants_mounts_for() {
        let mut svc = service("app.service");
        svc.unit.wants_mounts_for.push("/data".to_string());
        let units = run_injection(vec![
            svc,
            mount_unit("data.mount", "/data", "/dev/sda1", "ext4", "defaults"),
        ]);
        let app = &units["app.service"];
        assert!(app.unit.wants.contains("data.mount"));
        assert!(app.unit.after.contains("data.mount"));
        assert!(!app.unit.requires.contains("data.mount"));
    }

    #[test]
    fn test_requires_mounts_for_descendant_path() {
        // /data/sub lies below /data.mount: the mount must still be pulled in.
        let mut svc = service("app.service");
        svc.unit.requires_mounts_for.push("/data/sub".to_string());
        let units = run_injection(vec![
            svc,
            mount_unit("data.mount", "/data", "/dev/sda1", "ext4", "defaults"),
        ]);
        let app = &units["app.service"];
        assert!(app.unit.requires.contains("data.mount"));
    }

    #[test]
    fn test_swap_default_dependencies() {
        let mut swp = UnitFile::new("dev-sda2.swap");
        swp.kind = UnitKind::Swap;
        swp.swap = Some(SwapSection::default());
        let units = run_injection(vec![swp, target("swap.target"), target("umount.target")]);
        let s = &units["dev-sda2.swap"];
        assert!(s.unit.before.contains("swap.target"));
        assert!(s.unit.before.contains("umount.target"));
        assert!(s.unit.conflicts.contains("umount.target"));
        assert!(!s.unit.after.contains("remote-fs-pre.target"));
    }

    #[test]
    fn test_swap_netdev_dependencies() {
        let mut swp = UnitFile::new("dev-sda2.swap");
        swp.kind = UnitKind::Swap;
        swp.swap = Some(SwapSection {
            options: "_netdev".to_string(),
            ..Default::default()
        });
        let units = run_injection(vec![
            swp,
            target("remote-fs-pre.target"),
            target("remote-fs.target"),
            target("network.target"),
            target("network-online.target"),
            target("umount.target"),
        ]);
        let s = &units["dev-sda2.swap"];
        assert!(s.unit.after.contains("remote-fs-pre.target"));
        assert!(s.unit.before.contains("remote-fs.target"));
        assert!(s.unit.after.contains("network.target"));
        assert!(s.unit.wants.contains("network-online.target"));
        assert!(!s.unit.before.contains("swap.target"));
    }

    #[test]
    fn test_mount_device_dependency() {
        let units = run_injection(vec![
            mount_unit(
                "mnt-data.mount",
                "/mnt/data",
                "/dev/sda1",
                "ext4",
                "defaults",
            ),
            {
                let mut d = UnitFile::new("dev-sda1.device");
                d.kind = UnitKind::Device;
                d
            },
        ]);
        let mnt = &units["mnt-data.mount"];
        assert!(mnt.unit.requires.contains("dev-sda1.device"));
        assert!(mnt.unit.after.contains("dev-sda1.device"));
    }

    #[test]
    fn test_escape_unit_name_path() {
        assert_eq!(escape_unit_name_path("/"), "-");
        assert_eq!(escape_unit_name_path("/mnt/data"), "mnt-data");
        assert_eq!(escape_unit_name_path("/foo-bar"), "foo\\x2dbar");
        assert_eq!(escape_unit_name_path("/dev/sda1"), "dev-sda1");
        assert_eq!(escape_unit_name_path("/a/b.c"), "a-b.c");
    }

    // =========================================================================
    // Template instantiation
    // =========================================================================

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "systema-sysa-loader-{}-{}-{}",
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
    fn load_flexible_never_reads_a_template() {
        let dir = temp_dir("tpl");
        std::fs::write(
            dir.join("getty@.service"),
            "[Unit]\nDescription=Getty on %I\n[Service]\nExecStart=/sbin/agetty %i\n",
        )
        .unwrap();

        // Instances are materialized from the Finder-reported IR template;
        // the template file itself is a definition, not a loadable unit.
        let dirs = vec![dir.to_string_lossy().into_owned()];
        let err = load_unit_flexible_in(&dirs, "getty@tty3.service").unwrap_err();
        assert!(err.to_string().contains("Unit not found"), "{err}");
    }

    #[test]
    fn load_flexible_exact_file_wins() {
        let dir = temp_dir("exact");
        std::fs::write(
            dir.join("getty@tty3.service"),
            "[Unit]\nDescription=Exact\n[Service]\nExecStart=/bin/true\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("getty@.service"),
            "[Unit]\nDescription=Template\n[Service]\nExecStart=/bin/false\n",
        )
        .unwrap();

        let dirs = vec![dir.to_string_lossy().into_owned()];
        let unit = load_unit_flexible_in(&dirs, "getty@tty3.service").unwrap();
        assert_eq!(unit.unit.description, "Exact");
        let svc = unit.service.unwrap();
        assert_eq!(svc.exec_start[0].program, "/bin/true");
    }

    #[test]
    fn load_flexible_missing_errors() {
        let dir = temp_dir("missing");
        let dirs = vec![dir.to_string_lossy().into_owned()];
        assert!(load_unit_flexible_in(&dirs, "nonexistent.service").is_err());
        assert!(load_unit_flexible_in(&dirs, "getty@tty9.service").is_err());
    }

    #[test]
    fn load_flexible_applies_dropins_from_every_search_path() {
        let root = temp_dir("dropin-paths");
        let etc = root.join("etc");
        let lib = root.join("lib");
        std::fs::create_dir_all(&etc).unwrap();
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(
            lib.join("hello.service"),
            "[Unit]\nDescription=Hello\n[Service]\nExecStart=/bin/true\n",
        )
        .unwrap();
        std::fs::create_dir_all(etc.join("hello.service.d")).unwrap();
        std::fs::write(
            etc.join("hello.service.d/override.conf"),
            "[Service]\nEnvironment=EXTRA=1\n",
        )
        .unwrap();

        // `hello.service` lives in `lib`; its drop-in lives in `etc`.
        let dirs = vec![
            etc.to_string_lossy().into_owned(),
            lib.to_string_lossy().into_owned(),
        ];
        let unit = load_unit_flexible_in(&dirs, "hello.service").unwrap();
        let svc = unit.service.unwrap();
        assert!(svc.environment.contains(&"EXTRA=1".to_string()));
    }

    #[test]
    fn load_flexible_resolves_symlink_alias_to_canonical_name() {
        let dir = temp_dir("flexsym");
        std::fs::write(
            dir.join("lightdm.service"),
            "[Unit]\nDescription=Display manager\n[Service]\nExecStart=/usr/sbin/lightdm\n",
        )
        .unwrap();
        std::os::unix::fs::symlink(
            dir.join("lightdm.service"),
            dir.join("display-manager.service"),
        )
        .unwrap();

        let dirs = vec![dir.to_string_lossy().into_owned()];
        let unit = load_unit_flexible_in(&dirs, "display-manager.service").unwrap();
        assert_eq!(unit.name, "lightdm.service");
        assert_eq!(unit.unit.description, "Display manager");
        assert!(unit
            .install
            .alias
            .iter()
            .any(|a| a == "display-manager.service"));
    }

    #[test]
    fn load_flexible_rejects_bare_template() {
        let dir = temp_dir("tplrej");
        std::fs::write(
            dir.join("getty@.service"),
            "[Unit]\nDescription=Getty on %I\n[Service]\nExecStart=/sbin/agetty %I\nTTYPath=/dev/%I\n",
        )
        .unwrap();

        let dirs = vec![dir.to_string_lossy().into_owned()];
        let err = load_unit_flexible_in(&dirs, "getty@.service").unwrap_err();
        assert!(
            err.to_string().contains("template"),
            "unexpected error: {err}"
        );
        // Instances are IR-only and never resolved from the template file.
        let err = load_unit_flexible_in(&dirs, "getty@tty3.service").unwrap_err();
        assert!(err.to_string().contains("Unit not found"), "{err}");
    }

    #[test]
    fn load_unit_file_instantiates_instance_symlink() {
        let dir = temp_dir("instsym");
        std::fs::write(
            dir.join("getty@.service"),
            "[Unit]\nDescription=Getty on %I\n[Service]\nExecStart=/sbin/agetty %I\n",
        )
        .unwrap();
        std::os::unix::fs::symlink(dir.join("getty@.service"), dir.join("getty@tty1.service"))
            .unwrap();

        // The enablement symlink is an *instance* of the template, not an
        // alias: it must be loaded under its own instance name with %I
        // expanded, never folded into the bare template.
        let dirs = vec![dir.to_string_lossy().into_owned()];
        let unit = load_unit_file(&dirs, &dir.join("getty@tty1.service")).unwrap();
        assert_eq!(unit.name, "getty@tty1.service");
        assert_eq!(unit.unit.description, "Getty on tty1");
        let svc = unit.service.unwrap();
        assert_eq!(svc.exec_start[0].program, "/sbin/agetty");
        assert_eq!(svc.exec_start[0].args, vec!["tty1"]);
        assert!(unit.install.alias.is_empty());
    }

    #[test]
    fn debug_load_guest_default_target() {
        let dirs = vec!["/mnt/lib/systemd/system".to_string()];
        let unit = load_unit_flexible_in(&dirs, "default.target");
        match &unit {
            Ok(u) => {
                eprintln!("OK name={} aliases={:?}", u.name, u.install.alias);
            }
            Err(e) => {
                eprintln!("ERR: {e:?}");
            }
        }
        let unit2 = load_unit_flexible_in(&dirs, "graphical.target");
        match &unit2 {
            Ok(u) => {
                eprintln!("OK2 name={} aliases={:?}", u.name, u.install.alias);
            }
            Err(e) => {
                eprintln!("ERR2: {e:?}");
            }
        }
    }
}
