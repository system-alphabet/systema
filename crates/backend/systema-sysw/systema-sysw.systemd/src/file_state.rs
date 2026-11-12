//! Disk-only `UnitFileState` for unit names System A has not loaded.
//!
//! `systemctl is-enabled` must answer for names that are not part of the
//! runtime model, most importantly masked units (the Finder skips a
//! `-> /dev/null` link) and aliases (System A stores those as name mappings,
//! not units).  Everything that can be decided from the filesystem layout
//! alone is decided here, mirroring the shapes `unit_file_get_state()` reads;
//! install information (`static` / `indirect` / `disabled`) stays with the
//! Finder, which is the component that parses unit files.

use std::path::{Component, Path, PathBuf};

/// Directories systemd generators write their units into.
pub(crate) const GENERATOR_OUTPUT_DIRS: &[&str] = &[
    "/run/systemd/generator",
    "/run/systemd/generator.early",
    "/run/systemd/generator.late",
];

/// Classify `name` from the search paths and generator output directories.
///
/// Returns `None` when no file (or mask) of that name exists anywhere.
pub(crate) fn detect(name: &str, search_paths: &[String]) -> Option<&'static str> {
    detect_in(name, search_paths, GENERATOR_OUTPUT_DIRS)
}

/// [`detect`] with an explicit generator-output list, for tests.
fn detect_in(name: &str, search_paths: &[String], generator_dirs: &[&str]) -> Option<&'static str> {
    for dir in search_paths {
        let path = Path::new(dir).join(name);
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let runtime = is_runtime_dir(dir);
        if meta.file_type().is_symlink() {
            let Ok(target) = std::fs::read_link(&path) else {
                continue;
            };
            let resolved = resolve_relative(&path, &target);
            if resolved == Path::new("/dev/null") {
                return Some(if runtime { "masked-runtime" } else { "masked" });
            }
            if resolved.file_name() != path.file_name() {
                return Some("alias");
            }
            return Some(if runtime { "linked-runtime" } else { "linked" });
        }
        if meta.file_type().is_file() {
            // The file exists but System A never loaded it: the Finder could
            // not parse it, so there is no install information to report.
            return Some("static");
        }
    }
    for dir in generator_dirs {
        if Path::new(dir).join(name).is_file() {
            return Some("generated");
        }
    }
    None
}

/// Whether `dir` is a runtime (non-persistent) unit directory.
fn is_runtime_dir(dir: &str) -> bool {
    dir == "/run" || dir.starts_with("/run/")
}

/// Resolve a symlink target relative to the link's directory.
fn resolve_relative(link: &Path, target: &Path) -> PathBuf {
    if target.is_absolute() {
        return target.to_path_buf();
    }
    match link.parent() {
        Some(parent) => normalize(&parent.join(target)),
        None => target.to_path_buf(),
    }
}

/// Collapse `.` and `..` components lexically (no filesystem access).
fn normalize(path: &Path) -> PathBuf {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "systema-sysw-filestate-{}-{}-{}",
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
    fn mask_links_report_masked() {
        let dir = temp_dir("mask");
        symlink("/dev/null", dir.join("foo.service")).unwrap();

        assert_eq!(
            detect_in("foo.service", &[dir.to_string_lossy().into_owned()], &[]),
            Some("masked")
        );
    }

    #[test]
    fn runtime_predicate_covers_run_directories() {
        // `/run/...` cannot be created by an unprivileged test, so the
        // `masked-runtime` / `linked-runtime` branch is exercised through the
        // predicate that guards it.
        assert!(is_runtime_dir("/run/systemd/system"));
        assert!(is_runtime_dir("/run/systema"));
        assert!(!is_runtime_dir("/etc/systemd/system"));
    }

    #[test]
    fn alias_links_report_alias() {
        let dir = temp_dir("alias");
        fs::write(dir.join("ddm.service"), "[Unit]\n").unwrap();
        symlink("ddm.service", dir.join("display-manager.service")).unwrap();

        assert_eq!(
            detect_in(
                "display-manager.service",
                &[dir.to_string_lossy().into_owned()],
                &[]
            ),
            Some("alias")
        );
    }

    #[test]
    fn unit_file_links_report_linked() {
        let lib = temp_dir("linklib");
        let etc = temp_dir("linketc");
        fs::write(lib.join("foo.service"), "[Unit]\n").unwrap();
        symlink(lib.join("foo.service"), etc.join("foo.service")).unwrap();

        assert_eq!(
            detect_in("foo.service", &[etc.to_string_lossy().into_owned()], &[]),
            Some("linked")
        );
    }

    #[test]
    fn plain_files_report_static() {
        let dir = temp_dir("plain");
        fs::write(dir.join("foo.service"), "[Unit]\n").unwrap();
        assert_eq!(
            detect_in("foo.service", &[dir.to_string_lossy().into_owned()], &[]),
            Some("static")
        );
    }

    #[test]
    fn generator_output_reports_generated() {
        let empty = temp_dir("genempty");
        let generator = temp_dir("genout");
        fs::write(generator.join("foo.service"), "[Unit]\n").unwrap();
        let generator = generator.to_string_lossy().into_owned();
        assert_eq!(
            detect_in(
                "foo.service",
                &[empty.to_string_lossy().into_owned()],
                &[generator.as_str()],
            ),
            Some("generated")
        );
    }

    #[test]
    fn unknown_names_report_nothing() {
        let dir = temp_dir("unknown");
        assert_eq!(
            detect_in("nope.service", &[dir.to_string_lossy().into_owned()], &[]),
            None
        );
    }
}
