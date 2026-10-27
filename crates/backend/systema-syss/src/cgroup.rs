//! Unit cgroup filesystem probes for the stop path.
//!
//! Before stopping a service, System S may know its unit cgroup (learned
//! from a `unit.cgroup` lookup relayed through System A).  While the main
//! process dies with a plain signal, stray children that escaped the
//! process group only die with the whole cgroup — this module reads the
//! cgroup's process list and `populated` flag, and asks the kernel to kill
//! the subtree where the `cgroup.kill` interface (v2, kernel ≥ 5.14)
//! exists.
//!
//! Everything here degrades silently: an unreadable probe is "no
//! information" and the caller falls back to pid-based signalling, exactly
//! like on platforms without a cgroup filesystem (where the stub below
//! answers "nothing" for every probe).
//!
//! [`Querier`] is platform-neutral: it asks System A (which relays to System
//! R) where a unit's cgroup lives.  A `UnitCgroupResult.available = false`
//! verdict ("no cgroup kill on this machine") is cached for the life of the
//! IPC connection and cleared by the reconnect hook, so reconnects ask again
//! while ordinary stops do not repeat the round trip.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use sysa::proto::UnitCgroupResult;
use sysa::worker_ipc::EventPublisher;
use tracing::debug;

/// How long a `unit.cgroup` lookup may hold up the stop before System S
/// degrades to its process-group kill path.  Far below any stop timeout:
/// a degraded stop is always better than a delayed one.
const QUERY_BUDGET: Duration = Duration::from_millis(200);

/// Stop-time cgroup lookup for services.
///
/// Clones cheaply and shares one `no_backend` verdict with the connection's
/// reconnect hook, so a reconnect wipes the verdict and the next stop asks
/// again.
#[derive(Clone)]
pub struct Querier {
    pubb: EventPublisher,
    no_backend: Arc<AtomicBool>,
}

impl Querier {
    /// Querier over `pubb`, sharing `no_backend` with the connection's
    /// reconnect hook (which stores `false` back into it — see `run` in
    /// `ipc`): on a fresh connection System R may have come up with a
    /// cgroup backend after all.
    pub fn new(pubb: EventPublisher, no_backend: Arc<AtomicBool>) -> Self {
        Querier { pubb, no_backend }
    }

    /// Learn the cgroup path of `unit_name` for a stop.
    ///
    /// `cached` (the unit's remembered path) short-circuits the lookup: the
    /// path is stable for a unit, so later stops reuse it without a round
    /// trip.  `None` means "no cgroup kill this time" — every failure mode
    /// (no backend, transport error, timeout, not found) degrades the same
    /// way instead of delaying the stop beyond [`QUERY_BUDGET`].
    pub async fn resolve(&self, unit_name: &str, cached: Option<String>) -> Option<String> {
        if let Some(path) = cached {
            return Some(path);
        }
        if self.no_backend.load(Ordering::Relaxed) {
            return None;
        }

        match self.pubb.query_unit_cgroup(unit_name, QUERY_BUDGET).await {
            Ok(result) => self.learn(&result),
            Err(e) => {
                debug!(
                    "unit {}: cgroup lookup failed: {e}; using the process group",
                    unit_name
                );
                None
            }
        }
    }

    /// Map one answer onto the stop-time decision.
    ///
    /// An existing directory is the path to kill; a missing directory is a
    /// transient answer (the cgroup may still appear between two stops —
    /// never memorized); "no cgroup backend on this machine" is remembered
    /// for the life of the connection so later stops skip the round trip.
    fn learn(&self, result: &UnitCgroupResult) -> Option<String> {
        if result.available && result.found {
            debug!("unit cgroup: stopping through {}", result.cgroup_path);
            return Some(result.cgroup_path.clone());
        }
        if result.available {
            debug!(
                "unit cgroup: no directory yet ({}); using the process group",
                if result.error.is_empty() {
                    "not found"
                } else {
                    result.error.as_str()
                }
            );
            return None;
        }
        debug!("unit cgroup: no cgroup kill on this machine; caching the verdict");
        self.no_backend.store(true, Ordering::Relaxed);
        None
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod imp {
    use std::path::Path;
    use std::{fs, io::Write};

    /// Deepest cgroup nesting the subtree walk descends into.  Real
    /// hierarchies are a handful of levels deeper than `.slice`; the bound
    /// only exists so a pathological tree cannot spin the stop path.
    const MAX_DEPTH: u32 = 32;

    /// PIDs in the cgroup at `path` **and every cgroup beneath it**
    /// (`cgroup.procs` of each directory): systemd's default kill mode
    /// reaches the whole subtree of the unit, so processes that moved
    /// themselves into a child cgroup must be signalled too.
    ///
    /// A failed read is an empty list: the caller unions the result with
    /// its usual pid targets, so a missing file can only ever shrink the
    /// extra fanout, never break the stop.
    pub fn read_pids(path: &str) -> Vec<u32> {
        let mut pids = Vec::new();
        collect_pids(Path::new(path), &mut pids, 0);
        pids
    }

    /// Depth-first accumulation behind [`read_pids`].
    fn collect_pids(dir: &Path, out: &mut Vec<u32>, depth: u32) {
        if depth > MAX_DEPTH {
            return;
        }
        if let Ok(list) = fs::read_to_string(dir.join("cgroup.procs")) {
            out.extend(list.lines().filter_map(|l| l.trim().parse::<u32>().ok()));
        }
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            // Only child cgroups are directories; the interface files
            // (`cgroup.procs`, `cgroup.events`, ...) are skipped.
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                collect_pids(&entry.path(), out, depth + 1);
            }
        }
    }

    /// Whether the cgroup subtree holds live processes, according to the
    /// `populated` line of `cgroup.events`.  `None` when the flag cannot be
    /// read (old kernels) — the caller then falls back to watching
    /// [`read_pids`].
    pub fn populated(path: &str) -> Option<bool> {
        let events = fs::read_to_string(Path::new(path).join("cgroup.events")).ok()?;
        for line in events.lines() {
            if let Some(value) = line.strip_prefix("populated ") {
                return Some(value.trim() != "0");
            }
        }
        None
    }

    /// Kill the whole cgroup subtree with the `cgroup.kill` interface
    /// (kernel ≥ 5.14).  `true` only when the interface was present and
    /// accepted the request; otherwise the caller falls back to
    /// pid-based SIGKILLs.
    pub fn kill(path: &str) -> bool {
        let Ok(mut f) = fs::File::create(Path::new(path).join("cgroup.kill")) else {
            return false;
        };
        f.write_all(b"1").is_ok()
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
mod imp {
    /// No cgroup filesystem on this platform: no extra pids to signal.
    pub fn read_pids(_path: &str) -> Vec<u32> {
        Vec::new()
    }

    /// No `cgroup.events`: the populated flag is unknown, callers watch
    /// the main pid group via [`read_pids`] instead.
    pub fn populated(_path: &str) -> Option<bool> {
        None
    }

    /// There is nothing to kill through cgroupfs; callers use the plain
    /// SIGKILL path.
    pub fn kill(_path: &str) -> bool {
        false
    }
}

pub use imp::*;

#[cfg(test)]
mod tests {
    use super::*;
    use sysa::worker_ipc::EventPublisher;

    fn querier(
        no_backend: Arc<AtomicBool>,
    ) -> (Querier, tokio::sync::mpsc::UnboundedReceiver<bytes::Bytes>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (
            Querier::new(
                EventPublisher::new(tx, "system-s-1", Default::default(), Default::default()),
                no_backend,
            ),
            rx,
        )
    }

    /// A learned path is reused without any round trip; the channel stays
    /// empty.
    #[tokio::test]
    async fn resolve_reuses_the_cached_path() {
        let (q, mut rx) = querier(Arc::new(AtomicBool::new(false)));
        let path = q
            .resolve(
                "app.service",
                Some("/sys/fs/cgroup/system.slice/app.service".into()),
            )
            .await;
        assert_eq!(
            path.as_deref(),
            Some("/sys/fs/cgroup/system.slice/app.service")
        );
        assert!(
            rx.try_recv().is_err(),
            "no envelope may be sent for a cached path"
        );
    }

    /// A negative verdict suppresses all following lookups for the
    /// connection ("本机无 cgroup 杀法").
    #[tokio::test]
    async fn resolve_skips_the_lookup_after_a_negative_verdict() {
        let flag = Arc::new(AtomicBool::new(true));
        let (q, mut rx) = querier(flag.clone());
        assert_eq!(q.resolve("app.service", None).await, None);
        assert!(
            rx.try_recv().is_err(),
            "no envelope may be sent after a verdict"
        );
        assert!(flag.load(Ordering::Relaxed));
    }

    /// The decision mapping: keep an existing path, treat a missing
    /// directory as transient, memorize "no cgroup kill on this machine".
    #[test]
    fn learn_maps_the_verdicts() {
        let flag = Arc::new(AtomicBool::new(false));
        let (q, _rx) = querier(flag.clone());

        let found = UnitCgroupResult {
            available: true,
            found: true,
            cgroup_path: "/sys/fs/cgroup/system.slice/app.service".to_string(),
            error: String::new(),
        };
        assert_eq!(
            q.learn(&found).as_deref(),
            Some("/sys/fs/cgroup/system.slice/app.service")
        );
        assert!(!flag.load(Ordering::Relaxed));

        // A missing directory is a transient answer — asked again next stop.
        let missing = UnitCgroupResult {
            available: true,
            found: false,
            cgroup_path: String::new(),
            error: String::new(),
        };
        assert_eq!(q.learn(&missing), None);
        assert!(!flag.load(Ordering::Relaxed));

        // "No cgroup kill on this machine" is remembered for the connection.
        let unavailable = UnitCgroupResult {
            available: false,
            found: false,
            cgroup_path: String::new(),
            error: "no cgroup backend".to_string(),
        };
        assert_eq!(q.learn(&unavailable), None);
        assert!(flag.load(Ordering::Relaxed));
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "android")))]
mod fs_tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU32;

    /// A fake cgroup directory: writable tmpfs, so `kill()` verifies the
    /// write path (the real cgroupfs's permission/kernel-version checks
    /// cannot be reproduced in userspace; only the accepted/declined
    /// behaviour of the function is under test).
    fn fake_cgroup(tag: &str, events: Option<&str>, procs: Option<&str>) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "syss-cgroup-fake-{}-{}-{}",
            tag,
            std::process::id(),
            PROBE_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        if let Some(events) = events {
            fs::write(dir.join("cgroup.events"), events).unwrap();
        }
        if let Some(procs) = procs {
            fs::write(dir.join("cgroup.procs"), procs).unwrap();
        }
        dir
    }

    static PROBE_SEQ: AtomicU32 = AtomicU32::new(0);

    #[test]
    fn read_pids_parses_the_process_list() {
        let dir = fake_cgroup("pids", None, Some("12\n345\n\n6789\n"));
        assert_eq!(read_pids(dir.to_str().unwrap()), vec![12, 345, 6789]);
    }

    /// The walk covers child cgroups: a process that moved itself one level
    /// down still belongs to the unit's tree and must be signalled.
    #[test]
    fn read_pids_descends_into_child_cgroups() {
        let dir = fake_cgroup("subtree", None, Some("12\n"));
        let child = dir.join("child");
        fs::create_dir_all(&child).unwrap();
        fs::write(child.join("cgroup.procs"), "345\n").unwrap();
        let grandchild = child.join("nested");
        fs::create_dir_all(&grandchild).unwrap();
        fs::write(grandchild.join("cgroup.procs"), "678\n").unwrap();

        let mut pids = read_pids(dir.to_str().unwrap());
        pids.sort_unstable();
        assert_eq!(pids, vec![12, 345, 678]);
    }

    #[test]
    fn read_pids_tolerates_a_missing_cgroup() {
        assert!(read_pids("/nonexistent/systema/cgroup").is_empty());
    }

    #[test]
    fn populated_follows_the_events_file() {
        let busy = fake_cgroup("busy", Some("frozen 0\npopulated 1\n"), None);
        assert_eq!(populated(busy.to_str().unwrap()), Some(true));
        let empty = fake_cgroup("empty", Some("frozen 0\npopulated 0\n"), None);
        assert_eq!(populated(empty.to_str().unwrap()), Some(false));
    }

    #[test]
    fn populated_is_unknown_without_the_events_file() {
        // Old kernel: `cgroup.events` lacks the `populated` line entirely.
        let old = fake_cgroup("oldkernel", Some("冻结 0\n"), None);
        assert_eq!(populated(old.to_str().unwrap()), None);
        assert_eq!(populated("/nonexistent/systema/cgroup"), None);
    }

    #[test]
    fn kill_requires_a_writable_cgroup_directory() {
        let dir = fake_cgroup("kill", None, None);
        assert!(
            kill(dir.to_str().unwrap()),
            "an existing directory accepts the request"
        );
        assert!(fs::read_to_string(dir.join("cgroup.kill")).unwrap() == "1");
        assert!(
            !kill("/nonexistent/systema/cgroup"),
            "a missing directory declines it"
        );
    }
}
