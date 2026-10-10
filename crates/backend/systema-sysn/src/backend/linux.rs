//! inotify-based backend for Linux.
//!
//! Watches the parent directory of every spec path (catches creation,
//! deletion, rename) plus the spec path itself when it exists (catches
//! in-place writes).  Events carry the directory entry name and precise
//! masks, which are translated into the neutral [`FsEvent`] bits.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::os::fd::{AsRawFd, RawFd};
use std::path::Path;
use std::sync::Mutex;

use async_trait::async_trait;
use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask};
use tokio::io::unix::AsyncFd;
use tracing::{debug, warn};

use crate::spec::PathSpec;

use super::{FsEvent, PathBackend, PathChange, WatchTargets};

const READ_BUF_SIZE: usize = 32 * 1024;

/// Mask applied to every watch: we never know in advance which condition a
/// change satisfies, so observe everything and let the engine re-evaluate.
const ALL_MASK: WatchMask = WatchMask::CREATE
    .union(WatchMask::DELETE)
    .union(WatchMask::MOVED_FROM)
    .union(WatchMask::MOVED_TO)
    .union(WatchMask::ATTRIB)
    .union(WatchMask::CLOSE_WRITE)
    .union(WatchMask::MODIFY)
    .union(WatchMask::DELETE_SELF)
    .union(WatchMask::MOVE_SELF);

/// Newtype so `AsyncFd` can register readiness on the inotify fd while
/// `read_events`/watch operations (which require `&mut self`) go through the
/// mutex.
struct InotifyHandle(Mutex<Inotify>);

impl AsRawFd for InotifyHandle {
    fn as_raw_fd(&self) -> RawFd {
        self.0.lock().unwrap().as_raw_fd()
    }
}

pub struct InotifyBackend {
    inotify: AsyncFd<InotifyHandle>,
    /// Watch descriptor → owning unit.
    by_wd: Mutex<HashMap<WatchDescriptor, String>>,
    /// Unit → watch descriptors it owns.
    units: Mutex<HashMap<String, Vec<WatchDescriptor>>>,
}

impl InotifyBackend {
    pub fn new() -> anyhow::Result<Self> {
        let inotify = Inotify::init()?;
        let inotify = AsyncFd::new(InotifyHandle(Mutex::new(inotify)))?;
        Ok(InotifyBackend {
            inotify,
            by_wd: Mutex::new(HashMap::new()),
            units: Mutex::new(HashMap::new()),
        })
    }

    fn add_watch(&self, unit: &str, path: &str) {
        let guard = self.inotify.get_ref().0.lock().unwrap();
        let wd = match guard.watches().add(Path::new(path), ALL_MASK) {
            Ok(wd) => wd,
            Err(e) => {
                debug!("inotify: cannot watch '{path}' for '{unit}': {e}");
                return;
            }
        };
        drop(guard);
        self.by_wd.lock().unwrap().insert(wd.clone(), unit.to_string());
        self.units
            .lock()
            .unwrap()
            .entry(unit.to_string())
            .or_default()
            .push(wd);
    }
}

#[async_trait]
impl PathBackend for InotifyBackend {
    fn arm(&self, unit: &str, specs: &[PathSpec]) -> anyhow::Result<()> {
        self.disarm(unit);
        let mut seen = std::collections::HashSet::new();
        for spec in specs {
            for path in WatchTargets::for_spec(spec).into_paths() {
                if seen.insert(path.clone()) {
                    self.add_watch(unit, &path);
                }
            }
        }
        Ok(())
    }

    fn disarm(&self, unit: &str) {
        let wds = self.units.lock().unwrap().remove(unit);
        let Some(wds) = wds else {
            return;
        };
        let inotify = self.inotify.get_ref().0.lock().unwrap();
        for wd in &wds {
            if inotify.watches().remove(wd.clone()).is_err() {
                debug!("inotify: rm_watch failed for unit '{unit}'");
            }
        }
        drop(inotify);
        let mut by_wd = self.by_wd.lock().unwrap();
        for wd in &wds {
            by_wd.remove(wd);
        }
    }

    fn armed_units(&self) -> Vec<String> {
        self.units.lock().unwrap().keys().cloned().collect()
    }

    async fn changes(&self) -> Vec<PathChange> {
        let mut buf = [0u8; READ_BUF_SIZE];
        loop {
            let mut guard = match self.inotify.readable().await {
                Ok(g) => g,
                Err(e) => {
                    warn!("inotify readable() failed: {e}; retrying");
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    continue;
                }
            };
            let mut by_unit: HashMap<String, FsEvent> = HashMap::new();
            let mut overflow = false;

            let events = self.inotify.get_ref().0.lock().unwrap().read_events(&mut buf);
            match events {
                Ok(events) => {
                    for ev in events {
                        if ev.mask.contains(EventMask::Q_OVERFLOW) {
                            overflow = true;
                            continue;
                        }
                        if ev.mask.contains(EventMask::IGNORED) {
                            continue;
                        }
                        let unit = self
                            .by_wd
                            .lock()
                            .unwrap()
                            .get(&ev.wd)
                            .cloned()
                            .unwrap_or_default();
                        if unit.is_empty() {
                            continue;
                        }
                        let event = by_unit.entry(unit).or_default();
                        let fs = mask_to_fs(ev.mask);
                        event.created |= fs.created;
                        event.deleted |= fs.deleted;
                        event.renamed |= fs.renamed;
                        event.attrib |= fs.attrib;
                        event.closed_write |= fs.closed_write;
                        event.written |= fs.written;
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    // Spurious wake-up; wait for the next readable edge.
                    guard.clear_ready();
                    continue;
                }
                Err(e) => {
                    warn!("inotify read_events failed: {e}");
                    guard.clear_ready();
                    continue;
                }
            }
            guard.clear_ready();

            if overflow {
                debug!("inotify queue overflow — re-scanning all armed units");
                for unit in self.armed_units() {
                    by_unit.entry(unit).or_default();
                }
            }
            if by_unit.is_empty() {
                continue;
            }
            return by_unit
                .into_iter()
                .map(|(unit, event)| PathChange { unit, event })
                .collect();
        }
    }
}

/// Translate inotify masks into neutral [`FsEvent`] bits.
fn mask_to_fs(mask: EventMask) -> FsEvent {
    FsEvent {
        created: mask.contains(EventMask::CREATE),
        deleted: mask.contains(EventMask::DELETE),
        renamed: mask.contains(EventMask::MOVED_FROM) || mask.contains(EventMask::MOVED_TO),
        attrib: mask.contains(EventMask::ATTRIB),
        closed_write: mask.contains(EventMask::CLOSE_WRITE),
        written: mask.contains(EventMask::MODIFY),
    }
}
