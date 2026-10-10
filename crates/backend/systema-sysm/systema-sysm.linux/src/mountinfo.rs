use std::collections::HashMap;
use std::fs;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;
use systema_sysm_common::mount_table::{
    build_mount_unit_ir, commit_mount_units, reconcile, MountTableEntry, MountTableSnapshot,
};
use systema_sysf::ir::UnitIR;
use tokio::sync::Notify;
use tracing::{debug, info, warn};

use sysa::controller::UnitStatus;
use sysa::worker_ipc::EventPublisher;

use crate::linux::state::{MountInstance, MountRegistry, MountState};

/// A parsed entry from `/proc/self/mountinfo`.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct MountInfoEntry {
    pub mount_id: u32,
    pub parent_id: u32,
    pub major_minor: String,
    pub root: String,
    pub mount_point: String,
    pub mount_options: String,
    pub optional_fields: Vec<String>,
    pub filesystem_type: String,
    pub mount_source: String,
    pub super_options: String,
}

/// Parsed snapshot of `/proc/self/mountinfo`.
#[derive(Debug, Clone, Default)]
pub struct MountInfoSnapshot {
    pub entries: Vec<MountInfoEntry>,
}

impl MountInfoSnapshot {
    pub fn refresh() -> Result<Self> {
        let content = fs::read_to_string("/proc/self/mountinfo")?;
        let mut entries = Vec::new();

        for line in content.lines() {
            if let Some(entry) = parse_mountinfo_line(line) {
                entries.push(entry);
            }
        }

        Ok(MountInfoSnapshot { entries })
    }

    /// Convert into the platform-neutral table representation consumed by
    /// the shared reconciliation logic.  The `ignore` mountinfo optional
    /// field maps to `ignored` (mounts managed elsewhere).
    pub fn to_mount_table(&self) -> MountTableSnapshot {
        let entries = self
            .entries
            .iter()
            .map(|e| MountTableEntry {
                mount_point: e.mount_point.clone(),
                what: e.mount_source.clone(),
                fstype: e.filesystem_type.clone(),
                options: e.mount_options.clone(),
                ignored: e.optional_fields.iter().any(|f| f == "ignore"),
            })
            .collect();
        MountTableSnapshot::new(entries)
    }
}

/// Cheap live check: refreshes mountinfo and returns whether `mount_point` is present.
pub fn mount_point_is_mounted(mount_point: &str) -> bool {
    MountInfoSnapshot::refresh()
        .ok()
        .map(|s| s.to_mount_table().is_mounted(mount_point))
        .unwrap_or(false)
}

/// One-shot reconcile of the registry against the current mount table, used
/// by `sync_state()` so the connect-time full snapshot includes
/// already-mounted filesystems even before the monitor's first poll.
pub fn refresh_registry(registry: &MountRegistry) {
    let Ok(snapshot) = MountInfoSnapshot::refresh() else {
        return;
    };
    let table = snapshot.to_mount_table();
    let _ = reconcile(&table, |unit_name, entry| {
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
    });
}

fn parse_mountinfo_line(line: &str) -> Option<MountInfoEntry> {
    // Format:
    // 36 35 98:0 /mnt1 /mnt2 rw,noatime master:1 - ext3 /dev/root rw,errors=continue
    // The separator '-' is always present between optional fields and the rest.
    let parts: Vec<&str> = line.split(' ').collect();
    if parts.len() < 10 {
        return None;
    }

    let mount_id: u32 = parts[0].parse().ok()?;
    let parent_id: u32 = parts[1].parse().ok()?;
    let major_minor = parts[2].to_string();
    let root = parts[3].to_string();
    let mount_point = parts[4].to_string();
    let mount_options = parts[5].to_string();

    // Find the separator '-'
    let sep_pos = parts.iter().position(|&s| s == "-")?;
    let optional_fields: Vec<String> = parts[6..sep_pos].iter().map(|s| s.to_string()).collect();

    // After separator: fstype, mount_source, super_options
    let after_sep = &parts[sep_pos + 1..];
    if after_sep.len() < 3 {
        return None;
    }

    let filesystem_type = after_sep[0].to_string();
    let mount_source = after_sep[1].to_string();
    let super_options = after_sep[2..].join(" ");

    Some(MountInfoEntry {
        mount_id,
        parent_id,
        major_minor,
        root,
        mount_point,
        mount_options,
        optional_fields,
        filesystem_type,
        mount_source,
        super_options,
    })
}

/// MountInfo monitor: waits for changes to /proc/self/mountinfo (poll(2)
/// with POLLPRI, the documented procfs mechanism — inotify emits no events
/// for this file) and reconciles the registry against the real mount table.
pub struct MountInfoMonitor {
    registry: MountRegistry,
    last_snapshot: Option<MountTableSnapshot>,
    event_pub: EventPublisher,
}

/// How long the blocking mountinfo poll waits for a change before re-checking
/// the stop flag.  Bounded so that shutting the worker down can never wait on
/// it for more than a tick.
const POLL_TIMEOUT_MS: i32 = 500;

/// Sets the stop flag when dropped.
///
/// Held by [`MountInfoMonitor::run`]'s task for exactly as long as that task
/// lives: `poll(2)` cannot be interrupted from the outside, and tokio's
/// runtime shutdown waits for already-running blocking tasks — so without
/// this an unbounded `poll(-1)` would pin the worker at exit (System Init
/// used to have to SIGKILL it).  The runtime drops task futures before it
/// shuts the blocking pool down, which is what makes the ordering work.
struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

impl MountInfoMonitor {
    pub fn new(registry: MountRegistry, event_pub: EventPublisher) -> Self {
        MountInfoMonitor {
            registry,
            last_snapshot: None,
            event_pub,
        }
    }

    /// How often to rescan the mount table as a safety net.  poll(2) on
    /// /proc/self/mountinfo is the documented change-notification mechanism,
    /// but a periodic rescan guarantees convergence even if a wakeup is
    /// ever missed.  Every rescan diffs against the previous snapshot, so an
    /// unchanged table produces no events and no state updates.
    const RESCAN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

    pub async fn run(&mut self) {
        // Initial poll: capture the current mount table on startup
        // (change notifications only report changes, not the present state).
        self.poll().await;

        // Block on the mountinfo fd until the kernel reports a mount-table
        // change.  poll(2) with POLLPRI on /proc/self/mountinfo is the
        // documented procfs mechanism for this; inotify never fires for
        // this file.
        let file = match fs::File::open("/proc/self/mountinfo") {
            Ok(f) => f,
            Err(e) => {
                warn!("Failed to open /proc/self/mountinfo for polling: {e}");
                return;
            }
        };
        let fd = file.as_raw_fd();

        let notify = Arc::new(Notify::new());
        let notify_clone = notify.clone();

        // The poller runs on the blocking pool, where nothing can cancel it —
        // and tokio waits for already-running blocking tasks when the runtime
        // shuts down.  A bare `poll(-1)` would therefore hang this worker's
        // exit; instead the poll is bounded and watches a stop flag that
        // `_stop` raises when this task is dropped.
        let stop = Arc::new(AtomicBool::new(false));
        let _stop = StopOnDrop(stop.clone());
        tokio::task::spawn_blocking(move || {
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLPRI,
                revents: 0,
            };
            loop {
                let rc = unsafe { libc::poll(&mut pfd, 1, POLL_TIMEOUT_MS) };
                if stop.load(Ordering::Relaxed) {
                    debug!("mountinfo poller stopping (monitor went away)");
                    return;
                }
                if rc < 0 {
                    let err = std::io::Error::last_os_error();
                    if err.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    warn!("poll on /proc/self/mountinfo failed: {err}");
                    return;
                }
                // rc == 0 is the timeout tick: nothing changed, loop around.
                //
                // A non-zero revents (POLLPRI/POLLERR, or POLLNVAL) means the
                // table changed; any read that follows clears the flag, so
                // poll() resets it.  POLLNVAL is a real error.
                if pfd.revents & libc::POLLNVAL != 0 {
                    warn!("poll on /proc/self/mountinfo: invalid fd");
                    return;
                }
                if pfd.revents != 0 {
                    notify_clone.notify_one();
                }
            }
        });

        loop {
            tokio::select! {
                _ = notify.notified() => {}
                _ = tokio::time::sleep(Self::RESCAN_INTERVAL) => {}
            }
            self.poll().await;
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
            MountState::Mounted => ("active", "mounted"),
            MountState::Unmounting => ("deactivating", "unmounting"),
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

    /// Poll the registry against the current mount table.  Runs on
    /// startup, on every mount-table change (poll(2) wakeup), and on a
    /// periodic safety-net timer.  Every run re-probes the table, discovers
    /// mount points that are not yet in the registry, commits their UnitIRs
    /// to System A *before* any state update for them is published, and
    /// diffs known units' states against the previous snapshot.
    async fn poll(&mut self) {
        let snapshot = match MountInfoSnapshot::refresh() {
            Ok(s) => s,
            Err(e) => {
                warn!("Failed to read /proc/self/mountinfo: {}", e);
                return;
            }
        };
        let table = snapshot.to_mount_table();

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
            // The connect-time snapshot may already have created entries
            // before this monitor started; commit those too.
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

        // Check each mount instance against current mountinfo.
        let mount_points: Vec<(String, String, String)> = {
            let reg = self.registry.lock();
            reg.iter()
                .map(|(name, inst)| (name.clone(), inst.mount_point.clone(), inst.what.clone()))
                .collect()
        };

        for (unit_name, mp, _what) in &mount_points {
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
                        MountState::Mounted => {
                            info!("Mount point {} disappeared (external unmount)", mp);
                            inst.from_mountinfo = false;
                            inst.state = MountState::Dead;
                        }
                        MountState::Unmounting => {
                            debug!("Mount point {} disappeared (unmount completed)", mp);
                            inst.from_mountinfo = false;
                            inst.state = MountState::Dead;
                        }
                        _ => {}
                    }
                    // Publish if state actually changed
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

    fn entry(mount_point: &str, fstype: &str, optional: &[&str]) -> MountInfoEntry {
        MountInfoEntry {
            mount_id: 1,
            parent_id: 0,
            major_minor: "0:0".to_string(),
            root: "/".to_string(),
            mount_point: mount_point.to_string(),
            mount_options: String::new(),
            optional_fields: optional.iter().map(|s| s.to_string()).collect(),
            filesystem_type: fstype.to_string(),
            mount_source: "test".to_string(),
            super_options: String::new(),
        }
    }

    #[test]
    fn to_mount_table_maps_mountinfo_fields() {
        let mut snapshot = MountInfoSnapshot::default();
        let mut real = entry("/tmp", "tmpfs", &[]);
        real.mount_source = "tmpfs".to_string();
        real.mount_options = "rw,noatime".to_string();
        let mut ignored = entry("/mnt/other", "ext4", &["ignore"]);
        ignored.mount_source = "/dev/sdb1".to_string();
        snapshot.entries = vec![real, ignored];

        let table = snapshot.to_mount_table();

        // The `ignore` optional field maps to the shared `ignored` marker
        // consumed by the shared reconcile logic.
        assert!(table.is_mounted("/tmp"));
        assert!(!table.is_mounted("/mnt/other"));
        let tmp = table
            .entries
            .iter()
            .find(|e| e.mount_point == "/tmp")
            .unwrap();
        assert_eq!(tmp.what, "tmpfs");
        assert_eq!(tmp.fstype, "tmpfs");
        assert_eq!(tmp.options, "rw,noatime");
        assert!(!tmp.ignored);
    }

    /// System Init used to have to SIGKILL this worker ten seconds after
    /// SIGTERM: the blocking mountinfo `poll(-1)` outlived the task that
    /// spawned it, so runtime shutdown sat in the pool's `shutdown(None)`
    /// forever.  The bounded poll plus the [`StopOnDrop`] guard must release
    /// it within one poll tick of the task being dropped — which is exactly
    /// the real shutdown order (`Runtime::drop` drops tasks first, then
    /// drains the blocking pool).
    #[test]
    fn runtime_drop_releases_the_mountinfo_poller() {
        let (done_tx, done_rx) = std::sync::mpsc::channel();

        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            rt.block_on(async {
                let registry = crate::linux::state::new_mount_registry();
                tokio::spawn(async move {
                    let publisher = EventPublisher::new(tx, "system-m-1", Default::default());
                    let mut monitor = MountInfoMonitor::new(registry, publisher);
                    monitor.run().await;
                });
                // Long enough to pass the initial poll and reach the
                // blocking poll loop.
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            });

            drop(rt);
            let _ = done_tx.send(());
        });

        done_rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .expect(
                "runtime drop is stuck waiting for the mountinfo poller — the \
                 bounded poll/stop guard is not releasing it",
            );
    }
}
