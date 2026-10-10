//! Path predicates, glob matching, and stat signatures.
//!
//! A `.path` unit declares one or more conditions (`PathExists=`,
//! `PathExistsGlob=`, `PathChanged=`, `PathModified=`,
//! `DirectoryNotEmpty=`).  This module turns those declarations into
//! [`PathSpec`] values and provides the primitive checks the engine uses to
//! decide whether a condition is satisfied.

use std::path::{Path, PathBuf};

/// Kind of condition a `.path` unit waits for (systemd `[Path]` section).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PathSpecKind {
    /// Activate when the path exists (`PathExists=`).  Level-triggered.
    Exists,
    /// Activate when at least one file matching the glob exists
    /// (`PathExistsGlob=`).  Level-triggered.
    ExistsGlob,
    /// Activate when the path is created or modified (`PathChanged=`).
    /// Edge-triggered.
    Changed,
    /// Activate when the path is modified, including simple writes
    /// (`PathModified=`).  Edge-triggered.
    Modified,
    /// Activate when the directory is non-empty (`DirectoryNotEmpty=`).
    /// Level-triggered.
    DirectoryNotEmpty,
}

impl PathSpecKind {
    /// Level-triggered specs are evaluated purely by a stat/scan at arm time
    /// and after any event; edge-triggered specs additionally compare the
    /// path's stat signature against the one recorded at arm time.
    pub fn is_edge(self) -> bool {
        matches!(self, PathSpecKind::Changed | PathSpecKind::Modified)
    }
}

/// One watch condition derived from the `[Path]` section.
#[derive(Debug, Clone)]
pub struct PathSpec {
    pub kind: PathSpecKind,
    pub path: String,
}

/// stat() signature used to detect content changes for edge-triggered specs
/// and to re-check conditions after the target unit terminates.
///
/// ctime is deliberately excluded: chmod/chown (attribute-only changes) bump
/// ctime but do not count as a path *change* for systemd-compatible edge
/// triggering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PathSignature {
    pub mtime_sec: u64,
    pub mtime_nsec: u64,
    pub size: u64,
    pub inode: u64,
}

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

/// Capture the stat() signature of `path`, or `None` if it does not exist.
#[cfg(unix)]
pub fn stat_signature(path: &Path) -> Option<PathSignature> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    Some(PathSignature {
        mtime_sec: meta.mtime().max(0) as u64,
        mtime_nsec: meta.mtime_nsec().max(0) as u64,
        size: meta.size(),
        inode: meta.ino(),
    })
}

/// Split a glob pattern path into its directory and the basename glob, e.g.
/// `/var/spool/*.txt` → (`/var/spool`, `*.txt`).  The directory is the whole
/// path up to the last `/`; a pattern without `/` yields `.` as the directory.
pub fn split_glob_pattern(pattern: &str) -> (PathBuf, String) {
    match pattern.rfind('/') {
        Some(idx) => {
            let dir = &pattern[..idx];
            let base = &pattern[idx + 1..];
            (PathBuf::from(if dir.is_empty() { "/" } else { dir }), base.to_string())
        }
        None => (PathBuf::from("."), pattern.to_string()),
    }
}

/// Shell-style glob matcher supporting `*` (any sequence) and `?` (any
/// single character), mirroring the simple matcher used for
/// `ListUnitsByPatterns` in System A.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    fn inner(pat: &[char], name: &[char]) -> bool {
        match (pat.first(), name.first()) {
            (None, None) => true,
            (Some('*'), _) => {
                for i in 0..=name.len() {
                    if inner(&pat[1..], &name[i..]) {
                        return true;
                    }
                }
                false
            }
            (Some('?'), Some(_)) => inner(&pat[1..], &name[1..]),
            (Some(p), Some(n)) if p == n => inner(&pat[1..], &name[1..]),
            _ => false,
        }
    }
    let pat: Vec<char> = pattern.chars().collect();
    let nm: Vec<char> = name.chars().collect();
    inner(&pat, &nm)
}

/// Whether a directory entry should be ignored: systemd skips dot-files when
/// scanning for `PathExistsGlob=`/`DirectoryNotEmpty=`.
pub fn is_dotfile(name: &str) -> bool {
    name.starts_with('.')
}

/// Evaluate a level-triggered spec against the live filesystem.
///
/// Edge-triggered specs (`Changed`, `Modified`) are not evaluated here — the
/// engine decides those from events and signature comparisons.
pub fn evaluate_spec(spec: &PathSpec) -> bool {
    let path = Path::new(&spec.path);
    match spec.kind {
        PathSpecKind::Exists => path.exists(),
        PathSpecKind::DirectoryNotEmpty => {
            let Ok(rd) = std::fs::read_dir(path) else {
                return false;
            };
            rd.flatten().any(|e| {
                e.file_name()
                    .to_str()
                    .map(|n| !is_dotfile(n))
                    .unwrap_or(false)
            })
        }
        PathSpecKind::ExistsGlob => {
            let (dir, glob) = split_glob_pattern(&spec.path);
            let Ok(rd) = std::fs::read_dir(dir) else {
                return false;
            };
            rd.flatten().any(|e| {
                e.file_name()
                    .to_str()
                    .map(|n| !is_dotfile(n) && glob_match(&glob, n))
                    .unwrap_or(false)
            })
        }
        PathSpecKind::Changed | PathSpecKind::Modified => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matches_star_question() {
        assert!(glob_match("*.txt", "report.txt"));
        assert!(!glob_match("*.txt", "report.TXT"));
        assert!(!glob_match("*.txt", "report.md"));
        assert!(glob_match("foo?bar", "fooXbar"));
        assert!(!glob_match("foo?bar", "foobar"));
        assert!(glob_match("", ""));
        assert!(!glob_match("", "x"));
        assert!(glob_match("*", ""));
    }

    #[test]
    fn split_glob_handles_bare_pattern() {
        let (dir, base) = split_glob_pattern("*.conf");
        assert_eq!(dir, PathBuf::from("."));
        assert_eq!(base, "*.conf");

        let (dir, base) = split_glob_pattern("/var/spool/*.txt");
        assert_eq!(dir, PathBuf::from("/var/spool"));
        assert_eq!(base, "*.txt");
    }

    #[test]
    fn dotfile_detection() {
        assert!(is_dotfile(".hidden"));
        assert!(!is_dotfile("visible"));
    }

    #[test]
    fn exists_predicate_on_temp_file() {
        let tmp = std::env::temp_dir().join(format!("sysn-exists-{}", std::process::id()));
        let spec = PathSpec {
            kind: PathSpecKind::Exists,
            path: tmp.display().to_string(),
        };
        assert!(!evaluate_spec(&spec));
        std::fs::write(&tmp, b"x").unwrap();
        assert!(evaluate_spec(&spec));
        std::fs::remove_file(&tmp).unwrap();
        assert!(!evaluate_spec(&spec));
    }

    #[test]
    fn directory_not_empty_skips_dotfiles() {
        let tmp = std::env::temp_dir().join(format!("sysn-dne-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let spec = PathSpec {
            kind: PathSpecKind::DirectoryNotEmpty,
            path: tmp.display().to_string(),
        };
        assert!(!evaluate_spec(&spec));
        std::fs::write(tmp.join(".hidden"), b"x").unwrap();
        assert!(!evaluate_spec(&spec));
        std::fs::write(tmp.join("file"), b"x").unwrap();
        assert!(evaluate_spec(&spec));
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn glob_predicate_matches_files_in_dir() {
        let tmp = std::env::temp_dir().join(format!("sysn-glob-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let pattern = tmp.join("*.log").display().to_string();
        let spec = PathSpec {
            kind: PathSpecKind::ExistsGlob,
            path: pattern,
        };
        assert!(!evaluate_spec(&spec));
        std::fs::write(tmp.join("app.log"), b"x").unwrap();
        assert!(evaluate_spec(&spec));
        std::fs::write(tmp.join(".hidden.log"), b"x").unwrap();
        assert!(evaluate_spec(&spec));
        std::fs::write(tmp.join("app.txt"), b"x").unwrap();
        assert!(evaluate_spec(&spec));
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn signature_changes_on_write() {
        let tmp = std::env::temp_dir().join(format!("sysn-sig-{}", std::process::id()));
        std::fs::write(&tmp, b"one").unwrap();
        let before = stat_signature(&tmp).unwrap();
        std::fs::write(&tmp, b"two").unwrap();
        let after = stat_signature(&tmp).unwrap();
        assert!(before != after);
        std::fs::remove_file(&tmp).unwrap();
    }
}
