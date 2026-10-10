//! Discovery of the system mount table on Unix platforms without procfs.
//!
//! Sources, in preference order:
//! 1. `getmntinfo(3)` — FreeBSD/macOS/OpenBSD/DragonFly (and NetBSD, which
//!    uses `statvfs`),
//! 2. `getmntent(3)` over `/etc/mnttab` — Solaris/illumos,
//! 3. `mount -p` output — fallback (this crate is also built on Linux, where
//!    the dedicated Linux worker normally does the job; `mount -p` keeps the
//!    table discoverable everywhere else).
//!
//! Everything downstream of the raw table (unit-name escaping, registry
//! reconciliation, dynamic `UnitIR` construction, commit flow to System A)
//! lives in `systema-sysm-common` and is shared with the Linux worker.

use std::collections::HashMap;
use std::ffi::CString;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use systema_sysm_common::mount_table::{
    build_mount_unit_ir, commit_mount_units, reconcile, MountTableEntry, MountTableSnapshot,
};
use systema_sysf::ir::UnitIR;
use tracing::{debug, info, warn};

use sysa::controller::UnitStatus;
use sysa::worker_ipc::EventPublisher;

use crate::state::{MountInstance, MountRegistry, MountState};

// `CStr` is only needed by the native table readers (getmntinfo/mnttab);
// the `mount -p` fallback decodes text.
#[cfg(any(
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "dragonfly",
    target_os = "macos",
    target_os = "netbsd",
    target_os = "solaris",
    target_os = "illumos"
))]
use std::ffi::CStr;

/// Read the current mount table from the platform's native source.
fn read_mount_table() -> Result<Vec<MountTableEntry>> {
    #[cfg(any(
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "dragonfly",
        target_os = "macos"
    ))]
    {
        read_getmntinfo_statfs()
    }
    #[cfg(target_os = "netbsd")]
    {
        read_getmntinfo_statvfs()
    }
    #[cfg(any(target_os = "solaris", target_os = "illumos"))]
    {
        read_mnttab()
    }
    #[cfg(not(any(
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "dragonfly",
        target_os = "macos",
        target_os = "netbsd",
        target_os = "solaris",
        target_os = "illumos"
    )))]
    {
        read_mount_p()
    }
}

// ---------------------------------------------------------------------------
// BSDs / macOS: getmntinfo(3)
// ---------------------------------------------------------------------------

#[cfg(any(
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "dragonfly",
    target_os = "macos"
))]
/// `MNT_NOWAIT` — libc exports it on every BSD except DragonFly, which
/// shares FreeBSD's value (2).
#[cfg(any(
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "macos",
    target_os = "netbsd"
))]
const MNT_NOWAIT_FLAG: libc::c_int = libc::MNT_NOWAIT;
#[cfg(target_os = "dragonfly")]
const MNT_NOWAIT_FLAG: libc::c_int = 2;

#[cfg(any(
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "dragonfly",
    target_os = "macos"
))]
fn read_getmntinfo_statfs() -> Result<Vec<MountTableEntry>> {
    let mut mntbuf: *mut libc::statfs = std::ptr::null_mut();
    let count = unsafe { libc::getmntinfo(&mut mntbuf, MNT_NOWAIT_FLAG) };
    if count < 0 {
        anyhow::bail!(sysa::l10n::t_("getmntinfo failed"));
    }
    let mut entries = Vec::new();
    for i in 0..count {
        let m = unsafe { &*mntbuf.add(i as usize) };
        let mount_point = unsafe { CStr::from_ptr(m.f_mntonname.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        let what = unsafe { CStr::from_ptr(m.f_mntfromname.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        let fstype = unsafe { CStr::from_ptr(m.f_fstypename.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        let flags = m.f_flags as u64;
        entries.push(MountTableEntry {
            mount_point,
            what,
            fstype,
            options: flags_to_options(flags, bsd_flag_table()),
            ignored: flags & m_ignore_bit() != 0,
        });
    }
    Ok(entries)
}

#[cfg(target_os = "netbsd")]
fn read_getmntinfo_statvfs() -> Result<Vec<MountTableEntry>> {
    let mut mntbuf: *mut libc::statvfs = std::ptr::null_mut();
    let count = unsafe { libc::getmntinfo(&mut mntbuf, MNT_NOWAIT_FLAG) };
    if count < 0 {
        anyhow::bail!(sysa::l10n::t_("getmntinfo failed"));
    }
    let mut entries = Vec::new();
    for i in 0..count {
        let idx = i as usize;
        let m = unsafe { &*mntbuf.add(idx) };
        let mount_point = unsafe { CStr::from_ptr(m.f_mntonname.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        let what = unsafe { CStr::from_ptr(m.f_mntfromname.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        let fstype = unsafe { CStr::from_ptr(m.f_fstypename.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        let flags = m.f_flag as u64;
        entries.push(MountTableEntry {
            mount_point,
            what,
            fstype,
            options: flags_to_options(flags, bsd_flag_table()),
            ignored: flags & m_ignore_bit() != 0,
        });
    }
    Ok(entries)
}

/// Bit meaning "not really a mount we manage" — `MNT_IGNORE` where it
/// exists, otherwise (OpenBSD) there is no such flag so nothing matches.
#[cfg(any(
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "dragonfly",
    target_os = "macos",
    target_os = "netbsd"
))]
fn m_ignore_bit() -> u64 {
    #[cfg(any(
        target_os = "freebsd",
        target_os = "macos",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    {
        libc::MNT_IGNORE as u64
    }
    #[cfg(target_os = "openbsd")]
    {
        0
    }
}

/// Decode the common `MNT_*` flag bits into an option string.  The bit
/// values differ between the BSDs and macOS (e.g. `MNT_IGNORE`), so each
/// target provides its own constant table; the decoder itself is
/// platform-neutral and unit-tested.
///
/// On non-BSD platforms nothing consumes this function (the mount table is
/// read from mnttab or `mount -p`), hence the allow.
#[allow(dead_code)]
fn flags_to_options(flags: u64, table: &[(u64, &'static str)]) -> String {
    let mut opts = Vec::new();
    for (bit, name) in table {
        if flags & bit != 0 {
            opts.push(*name);
        }
    }
    opts.join(",")
}

#[cfg(any(
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "dragonfly",
    target_os = "macos",
    target_os = "netbsd"
))]
fn bsd_flag_table() -> &'static [(u64, &'static str)] {
    &[
        (libc::MNT_RDONLY as u64, "ro"),
        (libc::MNT_SYNCHRONOUS as u64, "sync"),
        (libc::MNT_ASYNC as u64, "async"),
        (libc::MNT_NOEXEC as u64, "noexec"),
        (libc::MNT_NOSUID as u64, "nosuid"),
        #[cfg(any(
            target_os = "macos",
            target_os = "dragonfly",
            target_os = "netbsd",
            target_os = "openbsd"
        ))]
        (libc::MNT_NODEV as u64, "nodev"),
        (libc::MNT_NOATIME as u64, "noatime"),
        #[cfg(any(target_os = "freebsd", target_os = "dragonfly"))]
        (libc::MNT_NOSYMFOLLOW as u64, "nosymfollow"),
        #[cfg(any(
            target_os = "macos",
            target_os = "freebsd",
            target_os = "netbsd"
        ))]
        (libc::MNT_UNION as u64, "union"),
        #[cfg(target_os = "netbsd")]
        (libc::MNT_RELATIME as u64, "relatime"),
    ]
}

// ---------------------------------------------------------------------------
// Solaris / illumos: getmntent(3) over /etc/mnttab
// ---------------------------------------------------------------------------

#[cfg(any(target_os = "solaris", target_os = "illumos"))]
fn read_mnttab() -> Result<Vec<MountTableEntry>> {
    let path = CString::new("/etc/mnttab").unwrap();
    let mode = CString::new("r").unwrap();
    let f = unsafe { libc::fopen(path.as_ptr(), mode.as_ptr()) };
    if f.is_null() {
        anyhow::bail!(sysa::l10n::t_("failed to open /etc/mnttab"));
    }
    let mut entries = Vec::new();
    loop {
        let m = unsafe { libc::getmntent(f) };
        if m.is_null() {
            break;
        }
        let what = unsafe { CStr::from_ptr((*m).mnt_special) }
            .to_string_lossy()
            .into_owned();
        let mount_point = unsafe { CStr::from_ptr((*m).mnt_mountp) }
            .to_string_lossy()
            .into_owned();
        let fstype = unsafe { CStr::from_ptr((*m).mnt_fstype) }
            .to_string_lossy()
            .into_owned();
        let options = unsafe { CStr::from_ptr((*m).mnt_mntopts) }
            .to_string_lossy()
            .into_owned();
        entries.push(MountTableEntry {
            mount_point,
            what,
            fstype,
            options,
            // mnttab has no ignore marker.
            ignored: false,
        });
    }
    unsafe { libc::fclose(f) };
    Ok(entries)
}

// ---------------------------------------------------------------------------
// Fallback: `mount -p` (fstab-style output)
// ---------------------------------------------------------------------------

#[cfg(not(any(
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "dragonfly",
    target_os = "macos",
    target_os = "netbsd",
    target_os = "solaris",
    target_os = "illumos"
)))]
fn read_mount_p() -> Result<Vec<MountTableEntry>> {
    let output = std::process::Command::new("mount")
        .arg("-p")
        .output()
        .map_err(|e| {
            anyhow::anyhow!(sysa::l10n::fmt(
                sysa::l10n::t_("mount -p failed: {e}"),
                &[("e", &e.to_string())]
            ))
        })?;
    if !output.status.success() {
        anyhow::bail!(sysa::l10n::fmt(
            sysa::l10n::t_("mount -p exited with {status}"),
            &[("status", &(output.status).to_string())]
        ));
    }
    Ok(parse_mount_p_output(&String::from_utf8_lossy(&output.stdout)))
}

/// Parse fstab-style `mount -p` output:
/// `device mountpoint fstype options dump pass` (dump/pass ignored).
#[cfg(not(any(
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "dragonfly",
    target_os = "macos",
    target_os = "netbsd",
    target_os = "solaris",
    target_os = "illumos"
)))]
fn parse_mount_p_output(output: &str) -> Vec<MountTableEntry> {
    let mut entries = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 4 {
            continue;
        }
        entries.push(MountTableEntry {
            what: unescape_mount_field(fields[0]),
            mount_point: unescape_mount_field(fields[1]),
            fstype: unescape_mount_field(fields[2]),
            options: unescape_mount_field(fields[3]),
            ignored: false,
        });
    }
    entries
}

/// Decode mount(8) escaping: `\040` space, `\011` tab, `\012` newline,
/// `\134` backslash (any octal escape, really).
#[cfg(not(any(
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "dragonfly",
    target_os = "macos",
    target_os = "netbsd",
    target_os = "solaris",
    target_os = "illumos"
)))]
fn unescape_mount_field(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 4 <= bytes.len() {
            if let Ok(s) = std::str::from_utf8(&bytes[i + 1..i + 4]) {
                if let Ok(octal) = u32::from_str_radix(s, 8) {
                    if let Ok(byte) = u8::try_from(octal) {
                        out.push(byte);
                        i += 4;
                        continue;
                    }
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------------
// Cheap live check
// ---------------------------------------------------------------------------

/// Cheap live check without a full table read: a path is a mount point iff
/// it stat()s successfully on a device different from its parent directory.
pub fn mount_point_is_mounted(mount_point: &str) -> bool {
    let path = match CString::new(mount_point) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::stat(path.as_ptr(), &mut st) } != 0 {
        return false;
    }
    let parent = Path::new(mount_point)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("/"));
    let parent = match CString::new(parent.to_string_lossy().as_bytes()) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let mut pst: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::stat(parent.as_ptr(), &mut pst) } != 0 {
        return false;
    }
    (st.st_dev as u64) != (pst.st_dev as u64)
}

// ---------------------------------------------------------------------------
// One-shot reconcile + background monitor
// ---------------------------------------------------------------------------

/// One-shot reconcile of the registry against the real mount table, used by
/// `sync_state()`: filesystems already mounted at boot get registry entries
/// before the background monitor's first tick.  Returns the created names.
pub fn refresh_registry(registry: &MountRegistry) -> Vec<String> {
    let table = match read_mount_table() {
        Ok(t) => MountTableSnapshot::new(t),
        Err(e) => {
            warn!("Failed to read mount table: {e}");
            return Vec::new();
        }
    };
    reconcile(&table, |unit_name, entry| {
        let mut reg = registry.lock();
        if reg.contains_key(unit_name) {
            return false;
        }
        let mut inst = MountInstance::new(
            unit_name.to_string(),
            entry.mount_point.clone(),
            entry.what.clone(),
        );
        inst.state = MountState::Mounted;
        inst.from_mountinfo = true;
        inst.fstype = entry.fstype.clone();
        inst.options = entry.options.clone();
        reg.insert(unit_name.to_string(), inst);
        true
    })
}

/// MountTable monitor: periodically re-reads the system mount table and
/// reconciles the registry against it (there is no kernel change
/// notification mechanism like Linux's poll(2) on procfs, so a 2-second
/// rescan is the mechanism).
pub struct MountTableMonitor {
    registry: MountRegistry,
    last_snapshot: Option<MountTableSnapshot>,
    event_pub: EventPublisher,
}

impl MountTableMonitor {
    pub fn new(registry: MountRegistry, event_pub: EventPublisher) -> Self {
        MountTableMonitor {
            registry,
            last_snapshot: None,
            event_pub,
        }
    }

    /// How often to rescan the mount table, matching the Linux worker's
    /// safety-net interval.  Every rescan diffs against the previous
    /// snapshot, so an unchanged table produces no events and no state
    /// updates.
    const RESCAN_INTERVAL: Duration = Duration::from_secs(2);

    pub async fn run(&mut self) {
        loop {
            self.poll().await;
            tokio::time::sleep(Self::RESCAN_INTERVAL).await;
        }
    }

    /// Publish the current runtime state of a mount unit as a unified
    /// `unit.state_update` (single event per state change).
    fn publish_state_change(&self, unit_name: &str) {
        let entry = {
            let reg = self.registry.lock();
            reg.get(unit_name)
                .map(|inst| (inst.state, inst.mount_point.clone()))
        };
        let (state, _mount_point) = match entry {
            Some(pair) => pair,
            None => return,
        };

        let (active_state, sub_state) = match state {
            MountState::Dead => ("inactive", "dead"),
            MountState::Mounting => ("activating", "mounting"),
            MountState::Mounted => ("active", "mounted"),
            MountState::Unmounting => ("deactivating", "unmounting"),
            MountState::Failed => ("failed", "failed"),
        };
        let status = UnitStatus {
            unit_name: unit_name.to_string(),
            active_state: active_state.to_string(),
            sub_state: sub_state.to_string(),
            main_pid: 0,
            invocation_id: String::new(),
            extensions: HashMap::new(),
        };
        self.event_pub
            .publish_unit_state_update(vec![status], false);
    }

    /// Poll the registry against the current mount table.  Every run
    /// re-reads the table, discovers mount points that are not yet in the
    /// registry, commits their UnitIRs to System A *before* any state
    /// update for them is published, and diffs known units' states against
    /// the previous snapshot.
    async fn poll(&mut self) {
        let table = match read_mount_table() {
            Ok(t) => MountTableSnapshot::new(t),
            Err(e) => {
                warn!("Failed to read mount table: {e}");
                return;
            }
        };

        // Discover mount points not yet in the registry (already-mounted
        // filesystems at boot, or new mounts appearing later).  The shared
        // reconcile logic drives both the registry insertion and the
        // dynamic UnitIR generation.
        let first_run = self.last_snapshot.is_none();
        let mut irs: HashMap<String, UnitIR> = HashMap::new();
        let created = reconcile(&table, |unit_name, entry| {
            let mut reg = self.registry.lock();
            if reg.contains_key(unit_name) {
                return false;
            }
            let mut inst = MountInstance::new(
                unit_name.to_string(),
                entry.mount_point.clone(),
                entry.what.clone(),
            );
            inst.state = MountState::Mounted;
            inst.from_mountinfo = true;
            inst.fstype = entry.fstype.clone();
            inst.options = entry.options.clone();
            reg.insert(unit_name.to_string(), inst);
            drop(reg);
            irs.insert(unit_name.to_string(), build_mount_unit_ir(unit_name, entry));
            true
        });

        // Dynamically generate a UnitIR for every discovered mount unit and
        // commit it to System A so the unit is KNOWN before any state update
        // for it is published.  A state_update for a unit SysA does not know
        // would be ignored.
        if first_run {
            // sync_state may already have created entries before this
            // monitor started; commit those too.
            let discovered: Vec<(String, String, String, String, String)> = {
                let reg = self.registry.lock();
                reg.iter()
                    .filter(|(_, inst)| inst.from_mountinfo)
                    .map(|(name, inst)| {
                        (
                            name.clone(),
                            inst.mount_point.clone(),
                            inst.what.clone(),
                            inst.fstype.clone(),
                            inst.options.clone(),
                        )
                    })
                    .collect()
            };
            for (name, mp, what, fstype, options) in discovered {
                irs.insert(
                    name.clone(),
                    build_mount_unit_ir(
                        &name,
                        &MountTableEntry {
                            mount_point: mp,
                            what,
                            fstype,
                            options,
                            ignored: false,
                        },
                    ),
                );
            }
        }
        if !irs.is_empty() {
            commit_mount_units(&irs).await;
        }

        // Only now (commit completed) publish the state of the new units.
        if !created.is_empty() {
            info!(
                "Discovered {} new mount unit(s): {:?}",
                created.len(),
                created
            );
            let statuses: Vec<UnitStatus> = created
                .iter()
                .map(|name| UnitStatus {
                    unit_name: name.clone(),
                    active_state: "active".to_string(),
                    sub_state: "mounted".to_string(),
                    main_pid: 0,
                    invocation_id: String::new(),
                    extensions: HashMap::new(),
                })
                .collect();
            self.event_pub.publish_unit_state_update(statuses, false);
        }

        let prev = match &self.last_snapshot {
            Some(p) => p,
            None => {
                // First poll: initial reconciliation.
                info!("Mount table ({} entries):", table.entries.len());
                for entry in &table.entries {
                    info!(
                        "  {} → {} type={} opts={}",
                        entry.what, entry.mount_point, entry.fstype, entry.options,
                    );
                }

                // Check every registry entry against the snapshot.
                // Any Dead entry whose mount point already exists → set Mounted.
                let unit_states: Vec<(String, String)> = {
                    let reg = self.registry.lock();
                    reg.iter()
                        .filter(|(_, inst)| inst.state == MountState::Dead)
                        .map(|(name, inst)| (name.clone(), inst.mount_point.clone()))
                        .collect()
                };

                for (unit_name, mp) in &unit_states {
                    if table.is_mounted(mp) {
                        let mut reg = self.registry.lock();
                        if let Some(inst) = reg.get_mut(unit_name) {
                            if inst.state == MountState::Dead {
                                info!("Initial reconciliation: {} ({}) → Mounted", unit_name, mp,);
                                inst.from_mountinfo = true;
                                inst.state = MountState::Mounted;
                            }
                        }
                        drop(reg);
                        self.publish_state_change(unit_name);
                    }
                }

                self.last_snapshot = Some(table);
                return;
            }
        };

        // Check each mount instance against the current table.
        let mount_points: Vec<(String, String)> = {
            let reg = self.registry.lock();
            reg.iter()
                .map(|(name, inst)| (name.clone(), inst.mount_point.clone()))
                .collect()
        };

        for (unit_name, mp) in &mount_points {
            let prev_mounted = prev.is_mounted(mp);
            let now_mounted = table.is_mounted(mp);

            if !prev_mounted && now_mounted {
                let mut reg = self.registry.lock();
                if let Some(inst) = reg.get_mut(unit_name) {
                    if inst.state == MountState::Dead {
                        debug!("Mount point {} appeared (external mount)", mp);
                        inst.from_mountinfo = true;
                        inst.state = MountState::Mounted;
                    }
                }
                drop(reg);
                self.publish_state_change(unit_name);
            } else if prev_mounted && !now_mounted {
                let mut reg = self.registry.lock();
                if let Some(inst) = reg.get_mut(unit_name) {
                    let old_state = inst.state;
                    match inst.state {
                        MountState::Mounted | MountState::Unmounting => {
                            info!("Mount point {} disappeared (external unmount)", mp);
                            inst.from_mountinfo = false;
                            inst.state = MountState::Dead;
                        }
                        _ => {}
                    }
                    // Publish if state actually changed.
                    if old_state != inst.state {
                        drop(reg);
                        self.publish_state_change(unit_name);
                    }
                }
            }
        }

        self.last_snapshot = Some(table);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(any(
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "dragonfly",
        target_os = "macos",
        target_os = "netbsd",
        target_os = "solaris",
        target_os = "illumos"
    )))]
    #[test]
    fn unescape_decodes_mount_escapes() {
        assert_eq!(unescape_mount_field("a\\040b"), "a b");
        assert_eq!(unescape_mount_field("a\\011b"), "a\tb");
        assert_eq!(unescape_mount_field("a\\134b"), "a\\b");
        assert_eq!(unescape_mount_field("plain"), "plain");
        assert_eq!(unescape_mount_field("\\040"), " ");
    }

    #[cfg(not(any(
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "dragonfly",
        target_os = "macos",
        target_os = "netbsd",
        target_os = "solaris",
        target_os = "illumos"
    )))]
    #[test]
    fn parse_mount_p_output_builds_entries() {
        let output = concat!(
            "# fstab-like output of `mount -p`\n",
            "/dev/sda1 / ext4 rw,relatime 0 0\n",
            "tmpfs /tmp tmpfs rw 0 0\n",
            "/dev/mapper/lvm\\040vol /mnt/lvm xfs rw,nosuid 0 0\n",
        );
        let entries = parse_mount_p_output(output);

        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].mount_point, "/");
        assert_eq!(entries[0].what, "/dev/sda1");
        assert_eq!(entries[0].fstype, "ext4");
        assert_eq!(entries[0].options, "rw,relatime");
        assert_eq!(entries[1].mount_point, "/tmp");
        assert_eq!(entries[1].what, "tmpfs");
        assert_eq!(entries[2].mount_point, "/mnt/lvm");
        assert_eq!(entries[2].what, "/dev/mapper/lvm vol");
    }

    #[test]
    fn flags_to_options_decodes_common_bits() {
        let table = &[
            (1, "ro"),
            (2, "sync"),
            (4, "noexec"),
            (8, "nosuid"),
            (16, "nodev"),
            (64, "async"),
            (256, "noatime"),
        ];
        assert_eq!(flags_to_options(1 | 4 | 256, table), "ro,noexec,noatime");
        assert_eq!(flags_to_options(0, table), "");
        assert_eq!(flags_to_options(2 | 8 | 64, table), "sync,nosuid,async");
        assert_eq!(flags_to_options(1 | 2 | 4 | 8 | 16 | 64 | 256, table), "ro,sync,noexec,nosuid,nodev,async,noatime");
    }
}
