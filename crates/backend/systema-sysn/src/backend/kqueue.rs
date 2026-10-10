//! kqueue-based backend for the BSDs and macOS.
//!
//! Registers one `EVFILT_VNODE` watch per target path (parent directory plus
//! the spec path itself when it exists).  kqueue events carry no entry name,
//! so every event is translated into a coarse wake-up — the engine re-scans
//! the filesystem to decide whether a condition is actually satisfied.  An
//! in-place write to a watched file is caught via the file's `NOTE_WRITE`;
//! a write to a *new* file inside a watched directory is caught via the
//! directory's `NOTE_WRITE` and the engine's signature comparison.
//!
//! The kqueue fd is polled with a zero timeout in a 100ms loop rather than
//! registered with the tokio reactor: registering a kqueue fd itself via
//! kevent/EVFILT_READ is unreliable on some BSD/macOS versions, and a plain
//! poll keeps the backend portable across all of them.

use std::collections::{HashMap, HashSet};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use tracing::{debug, warn};

use crate::spec::PathSpec;

use super::{FsEvent, PathBackend, PathChange, WatchTargets};

/// Events delivered per EVFILT_VNODE watch.  `NOTE_DELETE`/`NOTE_RENAME`/
/// `NOTE_REVOKE` apply to the watched vnode itself; the rest also fire on
/// directory entry changes for a watched directory.
///
/// The low seven `NOTE_*` bits are identical across FreeBSD, OpenBSD,
/// NetBSD, DragonFly, and macOS, so they are defined here rather than taken
/// from the platform libc bindings.
const KQ_DELETE: u32 = 0x0001;
const KQ_WRITE: u32 = 0x0002;
const KQ_EXTEND: u32 = 0x0004;
const KQ_ATTRIB: u32 = 0x0008;
const KQ_LINK: u32 = 0x0010;
const KQ_RENAME: u32 = 0x0020;
const KQ_REVOKE: u32 = 0x0040;

const VNODE_FFLAGS: u32 =
    KQ_WRITE | KQ_EXTEND | KQ_ATTRIB | KQ_LINK | KQ_DELETE | KQ_RENAME | KQ_REVOKE;

const MAX_EVENTS: usize = 64;

/// Polling interval for the kqueue fd.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

pub struct KqueueBackend {
    kq: OwnedFd,
    /// Watch fd → owning unit.
    by_fd: Mutex<HashMap<RawFd, String>>,
    /// Unit → watch fds it owns (kept open until disarm).
    units: Mutex<HashMap<String, Vec<RawFd>>>,
}

impl KqueueBackend {
    pub fn new() -> anyhow::Result<Self> {
        // SAFETY: kqueue() returns a new descriptor we own.
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_("kqueue() failed: {error}"),
                &[("error", &(std::io::Error::last_os_error()).to_string())]
            ));
        }
        Ok(KqueueBackend {
            kq: unsafe { OwnedFd::from_raw_fd(kq) },
            by_fd: Mutex::new(HashMap::new()),
            units: Mutex::new(HashMap::new()),
        })
    }

    /// Open a watch on `path` for `unit`, registering an EVFILT_VNODE
    /// kevent.  Returns the fd on success.
    fn add_watch(&self, unit: &str, path: &str) -> Option<RawFd> {
        // SAFETY: path is a NUL-terminated C string from a Rust String.
        let c_path = std::ffi::CString::new(path).ok()?;
        // O_RDONLY|O_NONBLOCK: avoid blocking on special files; the fd is
        // only used as a vnode reference for kevent.
        let fd = unsafe {
            libc::open(c_path.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC)
        };
        if fd < 0 {
            debug!(
                "kqueue: cannot open '{path}' for '{unit}': {}",
                std::io::Error::last_os_error()
            );
            return None;
        }

        // Built via zeroed() (not a struct literal) so the `ext` padding
        // field added to `kevent` on FreeBSD 12+ needs no per-platform field.
        let mut change: libc::kevent = unsafe { std::mem::zeroed() };
        change.ident = fd as libc::uintptr_t;
        change.filter = libc::EVFILT_VNODE;
        change.flags = libc::EV_ADD | libc::EV_CLEAR | libc::EV_RECEIPT;
        change.fflags = VNODE_FFLAGS;
        change.data = 0;
        change.udata = std::ptr::null_mut();
        // SAFETY: kevent() with one change; the fd and constants are valid.
        let n = unsafe {
            libc::kevent(
                self.kq.as_raw_fd(),
                &change,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        };
        if n < 0 {
            debug!(
                "kqueue: kevent registration failed for '{path}': {}",
                std::io::Error::last_os_error()
            );
            unsafe { libc::close(fd) };
            return None;
        }

        self.by_fd.lock().unwrap().insert(fd, unit.to_string());
        self.units
            .lock()
            .unwrap()
            .entry(unit.to_string())
            .or_default()
            .push(fd);
        Some(fd)
    }
}

#[async_trait]
impl PathBackend for KqueueBackend {
    fn arm(&self, unit: &str, specs: &[PathSpec]) -> anyhow::Result<()> {
        self.disarm(unit);
        let mut seen: HashSet<String> = HashSet::new();
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
        let fds = self.units.lock().unwrap().remove(unit);
        let Some(fds) = fds else {
            return;
        };
        let mut by_fd = self.by_fd.lock().unwrap();
        for fd in fds {
            by_fd.remove(&fd);
            // SAFETY: fd was opened by add_watch and is closed exactly once.
            unsafe { libc::close(fd) };
        }
    }

    fn armed_units(&self) -> Vec<String> {
        self.units.lock().unwrap().keys().cloned().collect()
    }

    async fn changes(&self) -> Vec<PathChange> {
        let zero_timeout = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        loop {
            // The kevent buffer holds a raw `udata` pointer and is therefore
            // not `Send`, so it must not survive the `.await` below. The
            // buffer is scoped to this block and dropped before the sleep.
            let result = {
                let mut events: [libc::kevent; MAX_EVENTS] =
                    unsafe { std::mem::zeroed() };
                // SAFETY: events array is MAX_EVENTS long and valid for write.
                let n = unsafe {
                    libc::kevent(
                        self.kq.as_raw_fd(),
                        std::ptr::null(),
                        0,
                        events.as_mut_ptr(),
                        MAX_EVENTS as i32,
                        &zero_timeout,
                    )
                };
                if n > 0 {
                    Some(self.collect(&events[..n as usize]))
                } else {
                    if n < 0 {
                        let err = std::io::Error::last_os_error();
                        if err.kind() == std::io::ErrorKind::WouldBlock
                            || err.kind() == std::io::ErrorKind::Interrupted
                        {
                            // No events pending; poll again after a short pause.
                        } else {
                            warn!("kqueue kevent() failed: {err}; retrying");
                        }
                    }
                    None
                }
            };
            if let Some(changes) = result {
                return changes;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }
}

impl KqueueBackend {
    /// Aggregate raw kevents into per-unit [`PathChange`]s.
    fn collect(&self, events: &[libc::kevent]) -> Vec<PathChange> {
        let mut by_unit: HashMap<String, FsEvent> = HashMap::new();
        let by_fd = self.by_fd.lock().unwrap();
        for ev in events {
            if ev.flags & libc::EV_ERROR != 0 {
                debug!("kqueue: EV_ERROR on fd {}: {}", ev.ident, ev.data);
                continue;
            }
            if ev.fflags == 0 {
                continue;
            }
            let fd = ev.ident as RawFd;
            let unit = match by_fd.get(&fd) {
                Some(u) => u.clone(),
                None => continue,
            };
            let event = by_unit.entry(unit).or_default();
            let fs = flags_to_fs(ev.fflags);
            event.created |= fs.created;
            event.deleted |= fs.deleted;
            event.renamed |= fs.renamed;
            event.attrib |= fs.attrib;
            event.closed_write |= fs.closed_write;
            event.written |= fs.written;
        }
        drop(by_fd);
        by_unit
            .into_iter()
            .map(|(unit, event)| PathChange { unit, event })
            .collect()
    }
}

/// Translate kqueue fflags into neutral [`FsEvent`] bits.
fn flags_to_fs(flags: u32) -> FsEvent {
    FsEvent {
        created: false,
        deleted: flags & KQ_DELETE != 0 || flags & KQ_REVOKE != 0,
        renamed: flags & KQ_RENAME != 0,
        attrib: flags & KQ_ATTRIB != 0 || flags & KQ_LINK != 0,
        closed_write: false,
        written: flags & KQ_WRITE != 0 || flags & KQ_EXTEND != 0,
    }
}
