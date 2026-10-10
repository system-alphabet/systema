//! Enablement detection — which units are "enabled".
//!
//! A unit is enabled when the system config says it should be started at
//! boot.  systemd expresses this with symlinks in the unit search paths:
//!
//! - `*.target.wants/` and `*.target.requires/` directories containing a
//!   link named after the unit (the common case: `systemctl enable`
//!   creates `multi-user.target.wants/foo.service`);
//! - a primary enablement link directly in a search directory whose
//!   basename equals the resolved target's basename but lives in a
//!   different directory (e.g. `/etc/systemd/system/foo.service` →
//!   `../usr/lib/systemd/system/foo.service`).  Aliases (a link whose
//!   basename differs from its target) are *not* enablement.
//!
//! The scan never follows into `.d/` drop-in directories or hidden
//! directories, mirroring the unit loader.

use std::collections::HashSet;
use std::path::Path;

/// Unit-file extensions that can carry enablement links.
fn is_known_extension(name: &str) -> bool {
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
    )
}

/// Return the set of enabled unit names found in the given search paths.
///
/// The union over all directories is taken: an enablement link in any
/// search directory enables the unit.
pub fn scan_enabled_units(dirs: &[String]) -> HashSet<String> {
    let mut enabled = HashSet::new();
    for dir in dirs {
        let dir = Path::new(dir);
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            let name = match entry.file_name().into_string() {
                Ok(n) => n,
                Err(_) => continue,
            };
            let path = entry.path();

            // Wants/requires directories: every entry is an enabled unit.
            if file_type.is_dir() && is_wants_requires_dir(&name) {
                let Ok(sub_entries) = std::fs::read_dir(&path) else {
                    continue;
                };
                for sub in sub_entries.flatten() {
                    let sub_name = match sub.file_name().into_string() {
                        Ok(n) => n,
                        Err(_) => continue,
                    };
                    if is_enablement_name(&sub_name) {
                        enabled.insert(sub_name);
                    }
                }
                continue;
            }

            // Primary enablement link: a symlink whose basename equals its
            // resolved target's basename, in a different directory.
            if file_type.is_symlink() && is_known_extension(&name) && !is_template(&name) {
                if let Ok(target) = std::fs::read_link(&path) {
                    let resolved = if target.is_absolute() {
                        target
                    } else {
                        dir.join(&target)
                    };
                    if let (Some(link_base), Some(target_base)) =
                        (Path::new(&name).file_name(), resolved.file_name())
                    {
                        let link_dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
                        let target_dir = resolved
                            .parent()
                            .and_then(|p| p.canonicalize().ok())
                            .unwrap_or_default();
                        if link_base == target_base && link_dir != target_dir {
                            enabled.insert(name);
                        }
                    }
                }
            }
        }
    }
    enabled
}

/// True for `<something>.target.wants` / `<something>.target.requires`
/// directories.
fn is_wants_requires_dir(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".target.wants") || lower.ends_with(".target.requires")
}

/// An enablement link name: a concrete unit name (not a template), not a
/// hidden file.
fn is_enablement_name(name: &str) -> bool {
    !name.starts_with('.') && is_known_extension(name) && !is_template(name)
}

/// True for template units (`foo@.service`).  Template links enable every
/// instance and cannot be started directly by name.
fn is_template(name: &str) -> bool {
    match name.rsplit_once('@') {
        Some((_, rest)) => rest.split('.').next().unwrap_or("").is_empty(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_link(dir: &std::path::Path, name: &str, target: &str) {
        std::os::unix::fs::symlink(target, dir.join(name)).unwrap();
    }

    fn write_file(dir: &std::path::Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn wants_directory_marks_units_enabled() {
        let tmp = std::env::temp_dir().join(format!("sysa-enable-wants-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("multi-user.target.wants")).unwrap();
        write_link(
            &tmp.join("multi-user.target.wants"),
            "foo.service",
            "../../usr/lib/systemd/system/foo.service",
        );
        write_link(
            &tmp.join("multi-user.target.wants"),
            "bar.timer",
            "/usr/lib/systemd/system/bar.timer",
        );

        let enabled = scan_enabled_units(&[tmp.to_string_lossy().to_string()]);
        assert!(enabled.contains("foo.service"));
        assert!(enabled.contains("bar.timer"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn requires_directory_marks_units_enabled() {
        let tmp = std::env::temp_dir().join(format!("sysa-enable-req-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("sysinit.target.requires")).unwrap();
        write_link(
            &tmp.join("sysinit.target.requires"),
            "foo.service",
            "/usr/lib/systemd/system/foo.service",
        );

        let enabled = scan_enabled_units(&[tmp.to_string_lossy().to_string()]);
        assert!(enabled.contains("foo.service"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn primary_link_in_other_dir_marks_enabled() {
        let tmp = std::env::temp_dir().join(format!("sysa-enable-primary-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("etc")).unwrap();
        std::fs::create_dir_all(tmp.join("lib")).unwrap();
        write_file(&tmp.join("lib"), "foo.service", "[Unit]\n[Service]\n");
        // /etc/systemd/system/foo.service → /usr/lib/systemd/system/foo.service
        write_link(&tmp.join("etc"), "foo.service", "../lib/foo.service");

        let enabled = scan_enabled_units(&[
            tmp.join("etc").to_string_lossy().to_string(),
            tmp.join("lib").to_string_lossy().to_string(),
        ]);
        assert!(enabled.contains("foo.service"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn alias_link_is_not_enablement() {
        let tmp = std::env::temp_dir().join(format!("sysa-enable-alias-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("lib")).unwrap();
        write_file(&tmp.join("lib"), "lightdm.service", "[Unit]\n[Service]\n");
        // display-manager.service → lightdm.service: an alias, not enablement.
        write_link(
            &tmp.join("lib"),
            "display-manager.service",
            "lightdm.service",
        );

        let enabled = scan_enabled_units(&[tmp.join("lib").to_string_lossy().to_string()]);
        assert!(!enabled.contains("display-manager.service"));
        assert!(!enabled.contains("lightdm.service"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn template_links_are_ignored() {
        let tmp = std::env::temp_dir().join(format!("sysa-enable-tmpl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("getty.target.wants")).unwrap();
        write_link(
            &tmp.join("getty.target.wants"),
            "getty@.service",
            "/usr/lib/systemd/system/getty@.service",
        );

        let enabled = scan_enabled_units(&[tmp.to_string_lossy().to_string()]);
        assert!(!enabled.contains("getty@.service"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn missing_directories_are_skipped() {
        let enabled = scan_enabled_units(&["/nonexistent/dir".to_string()]);
        assert!(enabled.is_empty());
    }
}
