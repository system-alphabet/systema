//! First-boot classification and machine-id resolution.
//!
//! This is the single authority for the `ConditionFirstBoot=` /
//! `AssertFirstBoot=` truth value (evaluated by System A before dispatch)
//! and for the `%m` specifier (expanded by the Finder parser and System S).
//! Both decision points share the same candidate list, so a machine id
//! committed by upstream tooling (`systemd-machine-id-commit`,
//! `systemd-update-done`) permanently silences first-boot-only units across
//! every consumer at once.
//!
//! Classification rules, in order:
//!
//! 1. **Forced first boot** — the flag file
//!    ([`sysa_ipc::paths::firstboot_flag_file`], default `/firstboot`)
//!    exists.  Deliberate override: operators may create it to make
//!    `ConditionFirstBoot=yes` units run on *every* boot (factory-reset
//!    image maintenance, reprovisioning); deleting it restores normal
//!    classification.
//! 2. **Machine id state** — first boot iff *no* machine id file exists in
//!    the candidate list (build-time [`sysa_ipc::paths::machine_id_file`]
//!    first, then [`MACHINE_ID_FALLBACK_FILES`]).  Fuller semantics detail:
//!    unlike systemd we do not read file contents — an existing file always
//!    counts as initialized; only the absence of a machine id anywhere
//!    counts as first boot.

use std::path::Path;

use crate::paths;

/// Fallback machine-id files consulted after the primary
/// [`sysa_ipc::paths::machine_id_file`], covering the locations other
/// ecosystems settle the id in: usrmerge distributions keep it in
/// `/usr/etc`, the D-Bus daemon at the head of its own list, and BSD
/// systems under `/var/db`.
pub const MACHINE_ID_FALLBACK_FILES: &[&str] = &[
    "/usr/etc/machine-id",
    "/usr/local/etc/machine-id",
    "/var/lib/dbus/machine-id",
    "/var/db/machine-id",
];

/// All machine-id candidates in resolution order: the build-time primary
/// path first, then the static fallbacks.
pub fn machine_id_files() -> Vec<String> {
    let mut files = vec![paths::instance().machine_id_file.to_string()];
    files.extend(MACHINE_ID_FALLBACK_FILES.iter().map(|f| f.to_string()));
    files
}

/// Core classification, injected so tests can drive it with temporary
/// paths.  `flag_file` forces first boot when it exists; otherwise first
/// boot is the absence of every machine-id candidate.
pub fn is_first_boot_with(flag_file: &str, machine_id_files: &[String]) -> bool {
    if Path::new(flag_file).exists() {
        return true;
    }
    !machine_id_files.iter().any(|f| Path::new(f).exists())
}

/// True when the current boot counts as the system's first boot; see the
/// module docs for the full rule set.
pub fn is_first_boot() -> bool {
    is_first_boot_with(paths::instance().firstboot_flag_file, &machine_id_files())
}

/// The machine id itself, as expanded for the `%m` specifier: the first
/// candidate that exists, is readable, and carries a non-empty id.  `None`
/// when no candidate yields an id (the `%m` expanders substitute `""`).
pub fn read_machine_id() -> Option<String> {
    read_machine_id_from(&machine_id_files())
}

/// `read_machine_id` over an injected candidate list (testable core).
pub fn read_machine_id_from(files: &[String]) -> Option<String> {
    files
        .iter()
        .filter_map(|f| {
            std::fs::read_to_string(f)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .next()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::process;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let dir = std::env::temp_dir().join(format!(
                "sysa-firstboot-test-{}-{}",
                process::id(),
                nanos
            ));
            fs::create_dir_all(&dir).expect("create temp dir");
            TempDir(dir)
        }

        fn file(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn paths_str(files: &[PathBuf]) -> Vec<String> {
        files.iter().map(|p| p.display().to_string()).collect()
    }

    #[test]
    fn no_machine_id_anywhere_is_first_boot() {
        let tmp = TempDir::new();
        let flag = tmp.file("flag");
        let candidates = paths_str(&[tmp.file("machine-id")]);
        assert!(is_first_boot_with(&flag.display().to_string(), &candidates));
    }

    #[test]
    fn existing_machine_id_is_not_first_boot() {
        let tmp = TempDir::new();
        let id = tmp.file("machine-id");
        fs::write(&id, "0123456789abcdef0123456789abcdef").unwrap();
        let flag = tmp.file("flag");
        let candidates = paths_str(&[id]);
        assert!(!is_first_boot_with(
            &flag.display().to_string(),
            &candidates
        ));
    }

    #[test]
    fn flag_file_forces_first_boot_even_with_machine_id() {
        let tmp = TempDir::new();
        let id = tmp.file("machine-id");
        fs::write(&id, "0123456789abcdef0123456789abcdef").unwrap();
        let flag = tmp.file("flag");
        fs::write(&flag, "").unwrap();
        let candidates = paths_str(&[id]);
        assert!(is_first_boot_with(&flag.display().to_string(), &candidates));
    }

    #[test]
    fn fallback_candidate_is_honored() {
        let tmp = TempDir::new();
        let id = tmp.file("id-in-fallback");
        fs::write(&id, "0123456789abcdef0123456789abcdef").unwrap();
        let candidates = paths_str(&[tmp.file("absent"), id]);
        assert!(!is_first_boot_with("/definitely/absent/flag", &candidates));
    }

    #[test]
    fn read_machine_id_skips_missing_and_empty_files() {
        let tmp = TempDir::new();
        let empty = tmp.file("empty");
        fs::write(&empty, "  \n").unwrap();
        let good = tmp.file("good");
        fs::write(&good, " 0123456789abcdef0123456789abcdef\n").unwrap();
        let candidates = paths_str(&[empty, good]);
        assert_eq!(
            read_machine_id_from(&candidates),
            Some("0123456789abcdef0123456789abcdef".to_string())
        );
    }

    #[test]
    fn read_machine_id_none_when_all_missing() {
        let tmp = TempDir::new();
        let candidates = paths_str(&[tmp.file("a"), tmp.file("b")]);
        assert_eq!(read_machine_id_from(&candidates), None);
    }
}
