//! Platform-neutral mount-table facts and the logic built on top of them.
//!
//! Each System M variant supplies the raw table (Linux: `/proc/self/
//! mountinfo`; other Unixes: `getmntinfo(3)`, `/etc/mnttab` or `mount -p`)
//! and converts it into [`MountTableSnapshot`].  Everything downstream —
//! the discovery gate, unit-name escaping, reconciliation, dynamic unit
//! registration with System A — is shared here.
//!
//! Mount-point policy itself lives in [`sysa::mounts`]; this module only
//! applies it while reconciling.

use std::collections::HashMap;

use sysa::finder::UnitFinder;
use systema_sysf::ir::{MountConfig as IrMountConfig, UnitIR, UnitType};
use tracing::{info, warn};

/// A single entry of the mount table in a platform-neutral form.
#[derive(Debug, Clone)]
pub struct MountTableEntry {
    /// Mount point path, e.g. `/tmp`.
    pub mount_point: String,
    /// Device or remote source, e.g. `tmpfs` or `/dev/sda1`.
    pub what: String,
    /// Filesystem type, e.g. `tmpfs`, `ext4`.
    pub fstype: String,
    /// Comma-separated options string, e.g. `rw,relatime`.
    pub options: String,
    /// Mounted with an "ignore" marker (mountinfo `ignore` optional field,
    /// BSD `MNT_IGNORE`): managed elsewhere, excluded from discovery.
    pub ignored: bool,
}

/// Snapshot of the current mount table.
#[derive(Debug, Clone, Default)]
pub struct MountTableSnapshot {
    pub entries: Vec<MountTableEntry>,
    /// Last entry per mount point — the topmost mount wins.
    by_mount_point: HashMap<String, usize>,
}

impl MountTableSnapshot {
    pub fn new(entries: Vec<MountTableEntry>) -> Self {
        let mut by_mount_point = HashMap::new();
        for (idx, entry) in entries.iter().enumerate() {
            by_mount_point.insert(entry.mount_point.clone(), idx);
        }
        MountTableSnapshot {
            entries,
            by_mount_point,
        }
    }

    /// Whether `mount_point` hosts a real (non-autofs, non-ignored)
    /// filesystem.  An autofs sentinel occupying the path does not count as
    /// "mounted": the mount registry tracks the real filesystem on top.
    pub fn is_mounted(&self, mount_point: &str) -> bool {
        match self.by_mount_point.get(mount_point) {
            Some(&idx) => {
                let entry = &self.entries[idx];
                !entry.ignored && entry.fstype != "autofs"
            }
            None => false,
        }
    }
}

/// Derive the mount unit name for a mount point, following systemd's
/// `unit_name_from_path()` escaping exactly: leading `/` is dropped and
/// `/` separators become `-`, every literal `-` becomes `\x2d`, `\` becomes
/// `\x5c`, `:`/`_`/`.` (but not a leading `.`) pass through, any other byte
/// becomes lowercase `\xHH`, and the root path `/` → `-.mount`.
pub fn mount_unit_name_from_path(path: &str) -> String {
    let trimmed = path.trim_start_matches('/').trim_end_matches('/');
    if trimmed.is_empty() {
        return "-.mount".to_string();
    }
    let mut name = String::with_capacity(trimmed.len());
    for (i, &b) in trimmed.as_bytes().iter().enumerate() {
        match b {
            b'/' => name.push('-'),
            b'-' => name.push_str("\\x2d"),
            b'\\' => name.push_str("\\x5c"),
            b':' | b'_' | b'.' if i > 0 => name.push(b as char),
            _ if b.is_ascii_alphanumeric() => name.push(b as char),
            _ => name.push_str(&format!("\\x{:02x}", b)),
        }
    }
    name.push_str(".mount");
    name
}

/// Build a dynamic mount `UnitIR` from mount-table facts, mirroring what a
/// `.mount` unit file derived from the same mount point would look like.
///
/// Only facts the mount table actually knows are provided (`unit_type`,
/// the mount point as description, and the `[Mount]` config).
pub fn build_mount_unit_ir(unit_name: &str, entry: &MountTableEntry) -> UnitIR {
    UnitIR {
        id: unit_name.to_string(),
        unit_type: Some(UnitType::Mount),
        description: Some(entry.mount_point.clone()),
        source_format: Some("dynamic".to_string()),
        source_path: None,
        slice: None,
        dependencies: None,
        service: None,
        mount: Some(IrMountConfig {
            what: entry.what.clone(),
            where_: entry.mount_point.clone(),
            type_: entry.fstype.clone(),
            options: entry.options.clone(),
            timeout_sec: 0,
        }),
        automount: None,
        timer: None,
        socket: None,
        conditions: None,
        asserts: None,
        wanted_by: None,
        required_by: None,
        aliases: Vec::new(),
        resource_control: None,
    }
}

/// Reconcile a registry against the mount table.
///
/// Passes every mount point that should become a unit to `create`, which is
/// responsible for inserting the unit into the calling crate's own registry
/// and returning `true` if a new unit was created.  Returns the names of the
/// created units.
///
/// This is also where the discovery gate sits: autofs sentinels, mounts
/// marked ignored, and mount points outside the unit system's jurisdiction
/// ([`sysa::mounts::is_exempt_from_units`]) are dropped here rather than
/// downstream, so System A never receives a unit it would have to know
/// mount-point policy to handle.
pub fn reconcile<F>(snapshot: &MountTableSnapshot, mut create: F) -> Vec<String>
where
    F: FnMut(&str, &MountTableEntry) -> bool,
{
    let mut created = Vec::new();
    for entry in &snapshot.entries {
        // autofs entries are automount sentinels, not real filesystems (the
        // real fs on top is a separate entry); ignored mounts are managed
        // elsewhere; exempt mount points never enter the unit system.
        if entry.fstype == "autofs"
            || entry.ignored
            || sysa::mounts::is_exempt_from_units(&entry.mount_point)
        {
            continue;
        }
        let unit_name = mount_unit_name_from_path(&entry.mount_point);
        if create(&unit_name, entry) {
            created.push(unit_name);
        }
    }
    created
}

/// Register + commit dynamically generated mount UnitIRs with System A via
/// the UnitFinder API.  Returns `true` on success.
pub async fn commit_mount_units(units: &HashMap<String, UnitIR>) -> bool {
    let json = match serde_json::to_vec(units) {
        Ok(json) => json,
        Err(e) => {
            warn!("Failed to serialize discovered mount UnitIRs: {e}");
            return false;
        }
    };
    let finder = UnitFinder::new();
    match finder
        .register_units("systema-sysm/discovery", json)
        .await
    {
        Ok(ack) if !ack.success => {
            warn!("Unit registration rejected: {}", ack.message);
            return false;
        }
        Err(e) => {
            warn!("Failed to register discovered mount units: {e}");
            return false;
        }
        Ok(_) => {}
    }
    match finder.commit_units("systema-sysm/discovery").await {
        Ok(ack) if !ack.success => {
            warn!("Unit commit rejected: {}", ack.message);
            false
        }
        Err(e) => {
            warn!("Failed to commit discovered mount units: {e}");
            false
        }
        Ok(ack) => {
            info!(
                "Committed {} dynamically discovered mount unit(s) to System A",
                ack.unit_count
            );
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn entry(mount_point: &str, fstype: &str) -> MountTableEntry {
        MountTableEntry {
            mount_point: mount_point.to_string(),
            what: "test".to_string(),
            fstype: fstype.to_string(),
            options: String::new(),
            ignored: false,
        }
    }

    #[test]
    fn mount_unit_name_escaping() {
        assert_eq!(mount_unit_name_from_path("/"), "-.mount");
        assert_eq!(mount_unit_name_from_path("/tmp"), "tmp.mount");
        assert_eq!(mount_unit_name_from_path("/mnt/data"), "mnt-data.mount");
        assert_eq!(mount_unit_name_from_path("/var/run"), "var-run.mount");
        assert_eq!(mount_unit_name_from_path("/mnt/-x"), "mnt-\\x2dx.mount");
        assert_eq!(mount_unit_name_from_path("/foo-bar"), "foo\\x2dbar.mount");
        assert_eq!(mount_unit_name_from_path("/mnt/a_b.c"), "mnt-a_b.c.mount");
        assert_eq!(mount_unit_name_from_path("/mnt/a:b"), "mnt-a:b.mount");
        assert_eq!(mount_unit_name_from_path("/.dotdir"), "\\x2edotdir.mount");
    }

    #[test]
    fn reconcile_skips_autofs_ignored_and_exempt() {
        let mut ignored = entry("/mnt/other", "ext4");
        ignored.ignored = true;
        let snapshot = MountTableSnapshot::new(vec![
            entry("/", "ext4"),
            entry("/tmp", "tmpfs"),
            entry("/mnt", "autofs"),
            ignored,
        ]);

        let mut registry: HashMap<String, MountTableEntry> = HashMap::new();
        let created = reconcile(&snapshot, |unit_name, e| {
            registry.insert(unit_name.to_string(), e.clone());
            true
        });

        // `/` is the extrinsic OS root and must never become `-.mount` —
        // otherwise it would pick up `Conflicts=umount.target`.
        assert_eq!(created, vec!["tmp.mount".to_string()]);
        assert!(!registry.contains_key("-.mount"));
        assert!(!registry.contains_key("mnt.mount"));
        assert!(!registry.contains_key("mnt-other.mount"));
    }

    #[test]
    fn reconcile_skips_exempt_mount_points() {
        let exempt = [
            // API
            "/proc",
            "/sys",
            "/dev",
            "/run",
            "/run/host/x",
            "/dev/shm",
            "/sys/fs/cgroup/system.slice",
            // extrinsic, but neither API nor systemd-ignored
            "/",
            "/usr",
            "/etc",
            "/proc/sys/fs/binfmt_misc",
            "/sys/kernel/debug",
            "/dev/mqueue",
            "/run/initramfs",
        ];
        let mut entries: Vec<MountTableEntry> = exempt.iter().map(|p| entry(p, "ext4")).collect();
        entries.push(entry("/home", "ext4"));
        entries.push(entry("/tmp", "tmpfs"));

        let snapshot = MountTableSnapshot::new(entries);
        let mut registry: HashMap<String, MountTableEntry> = HashMap::new();
        let created = reconcile(&snapshot, |unit_name, e| {
            registry.insert(unit_name.to_string(), e.clone());
            true
        });

        assert_eq!(
            created,
            vec!["home.mount".to_string(), "tmp.mount".to_string()]
        );
        assert_eq!(registry.len(), 2);
    }

    #[test]
    fn reconcile_is_idempotent() {
        let snapshot = MountTableSnapshot::new(vec![entry("/tmp", "tmpfs")]);
        let mut registry: HashMap<String, MountTableEntry> = HashMap::new();

        let first = reconcile(&snapshot, |unit_name, e| {
            if registry.contains_key(unit_name) {
                return false;
            }
            registry.insert(unit_name.to_string(), e.clone());
            true
        });
        let second = reconcile(&snapshot, |unit_name, e| {
            if registry.contains_key(unit_name) {
                return false;
            }
            registry.insert(unit_name.to_string(), e.clone());
            true
        });

        assert_eq!(first, vec!["tmp.mount".to_string()]);
        assert!(second.is_empty());
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn is_mounted_treats_autofs_sentinels_as_unmounted() {
        let snapshot = MountTableSnapshot::new(vec![entry("/mnt", "autofs")]);
        assert!(!snapshot.is_mounted("/mnt"));
        let snapshot2 = MountTableSnapshot::new(vec![entry("/tmp", "tmpfs")]);
        assert!(snapshot2.is_mounted("/tmp"));
    }

    #[test]
    fn build_mount_unit_ir_maps_mount_facts() {
        let ir = build_mount_unit_ir("tmp.mount", &entry("/tmp", "tmpfs"));
        assert_eq!(ir.id, "tmp.mount");
        assert_eq!(ir.unit_type, Some(UnitType::Mount));
        assert_eq!(ir.source_format.as_deref(), Some("dynamic"));
        // The description is the mount point path itself (e.g. "/tmp"),
        // not a prose label.
        assert_eq!(ir.description.as_deref(), Some("/tmp"));
        let mnt = ir.mount.unwrap();
        assert_eq!(mnt.what, "test");
        assert_eq!(mnt.where_, "/tmp");
        assert_eq!(mnt.type_, "tmpfs");
        assert_eq!(mnt.options, "");
        assert!(ir.service.is_none());
        assert!(ir.automount.is_none());
    }
}
