//! sd_notify(3) support for `Type=notify` / `Type=notify-reload` services.
//!
//! systemd's manager creates a single datagram socket per manager instance
//! at `<runtime>/systemd/notify` (`manager_setup_notify()`,
//! `src/core/manager.c:1100`) and passes `NOTIFY_SOCKET=<path>` to every
//! notify-type service (`exec-invoke.c:2218`).  Services send datagrams of
//! newline-separated `KEY=VALUE` pairs (`sd_notify(3)`); the manager matches
//! each message to a service by the sender PID carried in `SCM_CREDENTIALS`
//! (`SO_PASSCRED`), because many services share the one socket.
//!
//! This module replicates that scheme for systema-syss:
//!   * the socket is bound once at worker startup with mode 0777 — pid1
//!     runs with `umask(0)` in systemd (`main.c:3588`), which is what makes
//!     `/run/systemd/notify` world-writable on real systems; authorization
//!     happens via the credentials, not file permissions — so unprivileged
//!     service processes (e.g. `systemd --user`) may connect,
//!   * a reader thread parses datagrams and updates a shared tracker,
//!   * `wait_ready()` / `wait_reload()` block a start/reload job until the
//!     service reports readiness, mirroring `service.c`:
//!       - start:  `READY=1` in `SERVICE_START` → running,
//!       - reload: `RELOADING=1` with a `MONOTONIC_USEC` newer than the
//!         reload signal (`service_notify_message_process_state()`) →
//!         `SERVICE_RELOAD_NOTIFY`, then `READY=1` → reload done.
//!
//! A start/reload that never completes is aborted when the main PID goes
//! away (the monitor reaps it) or a `TimeoutStartSec`-style deadline
//! elapses.

use std::collections::HashMap;
use std::io::IoSliceMut;
use std::os::unix::io::{AsRawFd, IntoRawFd, RawFd};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use nix::cmsg_space;
use nix::sys::socket::{
    bind, recvmsg, socket, AddressFamily, MsgFlags, SockFlag, SockType, UnixAddr,
};
// nix only models per-message sender credentials (`UnixCredentials`,
// `SCM_CREDS`) on Linux and the FreeBSD family; OpenBSD has neither a
// `LOCAL_CREDS` socket option nor an `SCM_CREDS` control message.
#[cfg(not(target_os = "openbsd"))]
use nix::sys::socket::UnixCredentials;
use nix::sys::stat::{fchmod, Mode};
use tracing::{debug, info, warn};

use crate::process::{is_alive, pid_is_zombie};

// The sd_notify sender-credential mechanism differs by platform: Linux
// attaches per-message `SCM_CREDENTIALS` (enabled via `SO_PASSCRED`); the
// FreeBSD family carries `SCM_CREDS` (enabled via `LOCAL_CREDS`).  The
// reader logic is otherwise identical, so only the socket option and the
// received control-message variant are abstracted here.  OpenBSD offers
// neither mechanism (only `getpeereid()`, which needs a connected socket
// and cannot serve one socket shared by every service), so sd_notify is
// disabled there.
mod imp {
    // `AsRawFd` is only needed by the credential setup below, which
    // OpenBSD does not have.
    #[cfg(not(target_os = "openbsd"))]
    use std::os::unix::io::AsRawFd;
    use std::os::unix::io::OwnedFd;

    use nix::sys::socket::ControlMessageOwned;

    /// Enable delivery of the sender's credentials on the notify socket.
    ///
    /// OpenBSD has no per-message credential mechanism at all, so fail up
    /// front: `setup_at()` then disables the notify socket and callers
    /// treat notify services like `Type=simple`.
    #[cfg(target_os = "openbsd")]
    pub fn enable_sender_credentials(_fd: &OwnedFd) -> nix::Result<()> {
        Err(nix::errno::Errno::EOPNOTSUPP)
    }

    /// Enable delivery of the sender's credentials on the notify socket.
    #[cfg(not(target_os = "openbsd"))]
    pub fn enable_sender_credentials(fd: &OwnedFd) -> nix::Result<()> {
        let one: nix::libc::c_int = 1;
        // SAFETY: fd is a valid socket; LOCAL_CREDS is a boolean socket
        // option on the BSDs (no-op elsewhere), SO_PASSCRED on Linux.
        let ret = unsafe {
            nix::libc::setsockopt(
                fd.as_raw_fd(),
                nix::libc::SOL_SOCKET,
                credentials_option(),
                &one as *const _ as *const nix::libc::c_void,
                std::mem::size_of::<nix::libc::c_int>() as nix::libc::socklen_t,
            )
        };
        if ret == 0 {
            Ok(())
        } else {
            Err(nix::errno::Errno::last())
        }
    }

    /// Extract the sender PID from a received control message, if present.
    pub fn sender_pid(c: &ControlMessageOwned) -> Option<u32> {
        match c {
            #[cfg(any(target_os = "linux", target_os = "android"))]
            ControlMessageOwned::ScmCredentials(u) => Some(u.pid() as u32),
            #[cfg(any(
                target_os = "freebsd",
                target_os = "netbsd",
                target_os = "dragonfly"
            ))]
            ControlMessageOwned::ScmCreds(u) => Some(u.pid() as u32),
            _ => None,
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn credentials_option() -> nix::libc::c_int {
        nix::libc::SO_PASSCRED
    }
    #[cfg(any(
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    fn credentials_option() -> nix::libc::c_int {
        nix::libc::LOCAL_CREDS
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    )))]
    fn credentials_option() -> nix::libc::c_int {
        0
    }
}

/// `NOTIFY_SOCKET` value passed to notify-type services: `<runtime>/notify`
/// (i.e. `/run/systemd/notify`), exactly where systemd binds its socket.
pub fn notify_socket_path() -> String {
    format!("{}/notify", sysa::paths::instance().runstatedir)
}

/// CLOCK_MONOTONIC in microseconds — the same clock domain and unit that
/// `sd_notify()` reports in `MONOTONIC_USEC=`.
fn now_monotonic_usec() -> u64 {
    let mut ts = nix::libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        nix::libc::clock_gettime(nix::libc::CLOCK_MONOTONIC, &mut ts);
    }
    ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1_000
}

/// Per-invocation readiness state, keyed by the service's main PID.
#[derive(Debug, Clone, Default)]
struct Entry {
    unit_name: String,
    start_ready: bool,
    /// `Some` while a reload cycle is being tracked.
    reload: Option<ReloadState>,
}

#[derive(Debug, Clone)]
struct ReloadState {
    /// CLOCK_MONOTONIC (µs) when the reload signal was dispatched; a
    /// `RELOADING=1` is only accepted when its `MONOTONIC_USEC` is newer.
    begin_usec: u64,
    reloading_seen: bool,
    done: bool,
}

#[derive(Default)]
struct Tracker {
    by_pid: HashMap<u32, Entry>,
}

/// Handle for waiting on sd_notify readiness signals.
///
/// `register_start()` is called when a notify-type service is spawned;
/// the reader thread updates the entry as `READY=1` / `RELOADING=1`
/// datagrams arrive; `wait_ready()` / `wait_reload()` block until the
/// requested phase completes (or the process exits / times out).
#[derive(Clone, Default)]
pub struct NotifyManager {
    tracker: Arc<Mutex<Tracker>>,
}

impl NotifyManager {
    /// Bind `<runtime>/notify` and start the reader thread.
    ///
    /// Returns `None` when the socket cannot be created; callers then skip
    /// readiness waiting entirely (services are treated like Type=simple).
    pub fn setup() -> Option<Self> {
        Self::setup_at(&notify_socket_path())
    }

    fn setup_at(path: &str) -> Option<Self> {
        let fd = match socket(
            AddressFamily::Unix,
            SockType::Datagram,
            SockFlag::SOCK_CLOEXEC | SockFlag::SOCK_NONBLOCK,
            None,
        ) {
            Ok(fd) => fd,
            Err(e) => {
                warn!("Failed to allocate sd_notify socket: {e}");
                return None;
            }
        };

        // A stale socket file may survive a worker restart (the runtime dir
        // is only cleared on reboot); unlink before binding, like systemd's
        // `sockaddr_un_unlink()` in manager_setup_notify().
        let _ = std::fs::remove_file(path);

        let addr = match UnixAddr::new(path) {
            Ok(a) => a,
            Err(e) => {
                warn!("Notify socket path '{path}' invalid for AF_UNIX: {e}");
                return None;
            }
        };
        if let Err(e) = bind(fd.as_raw_fd(), &addr) {
            warn!("Failed to bind sd_notify socket to '{path}': {e}");
            return None;
        }

        // 0777 so unprivileged service processes can connect; the sender
        // is authorized via SCM_CREDENTIALS below.
        if let Err(e) = fchmod(fd.as_raw_fd(), Mode::from_bits_truncate(0o777)) {
            warn!("Failed to chmod sd_notify socket '{path}': {e}");
        }

        // Enable sender credentials (SO_PASSCRED on Linux, LOCAL_CREDS on
        // the BSDs) so the reader can attribute each message to a service.
        if let Err(e) = imp::enable_sender_credentials(&fd) {
            warn!("Failed to enable sender credentials for sd_notify socket: {e}");
            return None;
        }

        let manager = Self::default();
        spawn_reader(fd.into_raw_fd(), manager.tracker.clone());
        info!("Listening for sd_notify messages on {path}");
        Some(manager)
    }
    /// Track a newly spawned notify-type service waiting for its initial
    /// `READY=1`.  Any previous invocation of the same unit is forgotten
    /// (its main PID is gone by the time a new one spawns).
    pub fn register_start(&self, unit_name: &str, pid: u32) {
        let mut map = self.tracker.lock().unwrap();
        map.by_pid.retain(|_, e| e.unit_name != unit_name);
        map.by_pid.insert(
            pid,
            Entry {
                unit_name: unit_name.to_string(),
                start_ready: false,
                reload: None,
            },
        );
    }

    /// Begin tracking a reload cycle for a running service.  Must be called
    /// right before the reload signal is dispatched.  Returns `false` when
    /// the service is not tracked at all (nothing to wait for).
    pub fn register_reload(&self, unit_name: &str, pid: u32) -> bool {
        let mut map = self.tracker.lock().unwrap();
        let entry = match map.by_pid.get_mut(&pid) {
            Some(e) => e,
            None => {
                // The service passed its initial start (we are reloading it
                // after all); a missing entry only means the start-time wait
                // cleaned up after itself.
                map.by_pid.insert(
                    pid,
                    Entry {
                        unit_name: unit_name.to_string(),
                        start_ready: true,
                        reload: None,
                    },
                );
                map.by_pid.get_mut(&pid).unwrap()
            }
        };
        entry.reload = Some(ReloadState {
            begin_usec: now_monotonic_usec(),
            reloading_seen: false,
            done: false,
        });
        true
    }

    /// Wait until the service reports `READY=1` (startup), its main PID
    /// exits, or `timeout_secs` elapses.
    pub async fn wait_ready(&self, unit_name: &str, pid: u32, timeout_secs: u64) -> Result<()> {
        self.wait_until(unit_name, pid, timeout_secs, |e| e.start_ready, "READY=1")
            .await
    }

    /// Wait for a reload cycle to complete: `RELOADING=1` (validated against
    /// `MONOTONIC_USEC`) followed by `READY=1`.
    pub async fn wait_reload(&self, unit_name: &str, pid: u32, timeout_secs: u64) -> Result<()> {
        self.wait_until(
            unit_name,
            pid,
            timeout_secs,
            |e| e.reload.as_ref().is_some_and(|r| r.done),
            // sd_notify protocol tokens, not prose — interpolated verbatim
            // into the translated "{what}" slot below.
            "READY=1 after RELOADING=1",
        )
        .await
    }

    async fn wait_until(
        &self,
        unit_name: &str,
        pid: u32,
        timeout_secs: u64,
        done: impl Fn(&Entry) -> bool,
        what: &str,
    ) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(timeout_secs.max(1));
        loop {
            let finished = {
                let map = self.tracker.lock().unwrap();
                match map.by_pid.get(&pid) {
                    None => {
                        return Err(anyhow!(sysa::l10n::fmt(
                            sysa::l10n::t_("{unit_name}: sd_notify entry vanished"),
                            &[("unit_name", &unit_name.to_string())]
                        )));
                    }
                    Some(e) => done(e),
                }
            };
            if finished {
                let mut map = self.tracker.lock().unwrap();
                map.by_pid.remove(&pid);
                return Ok(());
            }
            if !is_alive(pid) || pid_is_zombie(pid) {
                let mut map = self.tracker.lock().unwrap();
                map.by_pid.remove(&pid);
                return Err(anyhow!(sysa::l10n::fmt(
                    sysa::l10n::t_("{unit_name} (PID {pid}) exited before {what}"),
                    &[
                        ("unit_name", &unit_name.to_string()),
                        ("pid", &pid.to_string()),
                        ("what", &what.to_string())
                    ]
                )));
            }
            if Instant::now() >= deadline {
                let mut map = self.tracker.lock().unwrap();
                map.by_pid.remove(&pid);
                return Err(anyhow!(sysa::l10n::fmt(
                    sysa::l10n::t_("{unit_name} timed out waiting for {what} ({timeout_secs}s)"),
                    &[
                        ("unit_name", &unit_name.to_string()),
                        ("what", &what.to_string()),
                        ("timeout_secs", &timeout_secs.to_string())
                    ]
                )));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// Reader thread: drain the socket and feed parsed messages into the
/// tracker.  Runs forever; the worker dies with the system.
fn spawn_reader(fd: RawFd, tracker: Arc<Mutex<Tracker>>) {
    let _ = std::thread::Builder::new()
        .name("sd-notify-reader".to_string())
        .spawn(move || {
            let mut buf = vec![0u8; 8192];
            loop {
                // OpenBSD models no credential control message; its notify
                // socket is never handed to the reader anyway
                // (`enable_sender_credentials()` fails there), so size the
                // buffer for a plain file-descriptor cmsg instead.
                #[cfg(target_os = "openbsd")]
                let mut cmsg_buf = cmsg_space!(RawFd);
                #[cfg(not(target_os = "openbsd"))]
                let mut cmsg_buf = cmsg_space!(UnixCredentials);
                let (n, sender_pid) = {
                    let mut iov = [IoSliceMut::new(&mut buf)];
                    let msg = match recvmsg::<()>(
                        fd,
                        &mut iov,
                        Some(&mut cmsg_buf),
                        MsgFlags::MSG_DONTWAIT,
                    ) {
                        Ok(m) => m,
                        Err(nix::errno::Errno::EAGAIN) => {
                            std::thread::sleep(Duration::from_millis(10));
                            continue;
                        }
                        Err(e) => {
                            warn!("sd_notify recvmsg failed: {e}");
                            std::thread::sleep(Duration::from_millis(100));
                            continue;
                        }
                    };
                    let pid = match msg.cmsgs() {
                        Ok(cmsgs) => cmsgs.filter_map(|c| imp::sender_pid(&c)).next(),
                        Err(_) => None,
                    };
                    (msg.bytes, pid)
                };

                let payload = &buf[..n];
                let Some(pid) = sender_pid else {
                    debug!("sd_notify message without sender credentials, ignoring");
                    continue;
                };

                if let Err(e) = apply_message(&tracker, pid, payload) {
                    debug!("sd_notify: {e}");
                }
            }
        });
}

/// Parse one datagram of newline-separated `KEY=VALUE` lines and update the
/// tracker entry for `pid`.  Unknown keys, unknown PIDs and garbage lines
/// are ignored (sd_notify(3) states that messages may carry arbitrary
/// key/value pairs).
fn apply_message(tracker: &Mutex<Tracker>, pid: u32, payload: &[u8]) -> Result<()> {
    let text = std::str::from_utf8(payload)?;
    let mut is_ready = false;
    let mut is_reloading = false;
    let mut monotonic_usec: Option<u64> = None;
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        let (key, value) = match line.split_once('=') {
            Some((k, v)) => (k, v),
            // Bare keys (e.g. `READY`) are accepted like `KEY=1`.
            None => (line, "1"),
        };
        match key {
            "READY" => is_ready = value == "1",
            "RELOADING" => is_reloading = value == "1",
            "MONOTONIC_USEC" => monotonic_usec = value.parse().ok(),
            _ => {}
        }
    }

    let mut map = tracker.lock().unwrap();
    let entry = match map.by_pid.get_mut(&pid) {
        Some(e) => e,
        None => return Ok(()),
    };

    if is_reloading {
        if let Some(reload) = entry.reload.as_mut() {
            match monotonic_usec {
                Some(t) if t >= reload.begin_usec => {
                    reload.reloading_seen = true;
                    debug!("{}: got RELOADING=1 (monotonic {t})", entry.unit_name);
                }
                Some(t) => {
                    debug!(
                        "{}: RELOADING=1 predates the reload signal ({t} < {}), ignoring",
                        entry.unit_name, reload.begin_usec
                    );
                }
                None => {
                    debug!(
                        "{}: RELOADING=1 without MONOTONIC_USEC, ignoring",
                        entry.unit_name
                    );
                }
            }
        }
    }
    if is_ready {
        entry.start_ready = true;
        if let Some(reload) = entry.reload.as_mut() {
            if reload.reloading_seen {
                reload.done = true;
            }
        }
        debug!("{}: got READY=1", entry.unit_name);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    fn tracker_with_entry(unit: &str, pid: u32) -> StdMutex<Tracker> {
        let mut map = Tracker::default();
        map.by_pid.insert(
            pid,
            Entry {
                unit_name: unit.to_string(),
                start_ready: false,
                reload: Some(ReloadState {
                    begin_usec: 1_000,
                    reloading_seen: false,
                    done: false,
                }),
            },
        );
        StdMutex::new(map)
    }

    fn lookup(tracker: &StdMutex<Tracker>, pid: u32) -> Entry {
        tracker
            .lock()
            .unwrap()
            .by_pid
            .get(&pid)
            .cloned()
            .unwrap_or_default()
    }

    #[test]
    fn ready_marks_start_ready() {
        let t = tracker_with_entry("user@1000.service", 42);
        apply_message(&t, 42, b"READY=1\nSTATUS=Reached basic.target.").unwrap();
        assert!(lookup(&t, 42).start_ready);
    }

    #[test]
    fn bare_ready_is_accepted() {
        let t = tracker_with_entry("user@1000.service", 42);
        apply_message(&t, 42, b"READY").unwrap();
        assert!(lookup(&t, 42).start_ready);
    }

    #[test]
    fn reloading_requires_monotonic_newer_than_begin() {
        let t = tracker_with_entry("user@1000.service", 42);
        // Older than the reload begin_usec (1000): ignored.
        apply_message(&t, 42, b"RELOADING=1\nMONOTONIC_USEC=500").unwrap();
        assert!(!lookup(&t, 42).reload.unwrap().reloading_seen);
        // Newer: accepted.
        apply_message(&t, 42, b"RELOADING=1\nMONOTONIC_USEC=2000").unwrap();
        assert!(lookup(&t, 42).reload.unwrap().reloading_seen);
    }

    #[test]
    fn reloading_without_monotonic_is_ignored() {
        let t = tracker_with_entry("user@1000.service", 42);
        apply_message(&t, 42, b"RELOADING=1").unwrap();
        assert!(!lookup(&t, 42).reload.unwrap().reloading_seen);
    }

    #[test]
    fn ready_completes_reload_only_after_reloading_seen() {
        let t = tracker_with_entry("user@1000.service", 42);
        // READY=1 before RELOADING=1 does not complete the reload.
        apply_message(&t, 42, b"READY=1").unwrap();
        assert!(!lookup(&t, 42).reload.unwrap().done);
        apply_message(&t, 42, b"RELOADING=1\nMONOTONIC_USEC=2000").unwrap();
        apply_message(&t, 42, b"READY=1").unwrap();
        assert!(lookup(&t, 42).reload.unwrap().done);
    }

    #[test]
    fn ready_without_reload_cycle_only_sets_start_ready() {
        let t = StdMutex::new(Tracker::default());
        t.lock().unwrap().by_pid.insert(
            7,
            Entry {
                unit_name: "svc.service".to_string(),
                start_ready: false,
                reload: None,
            },
        );
        apply_message(&t, 7, b"READY=1").unwrap();
        assert!(lookup(&t, 7).start_ready);
    }

    #[test]
    fn unknown_pid_is_ignored() {
        let t = tracker_with_entry("user@1000.service", 42);
        apply_message(&t, 999, b"READY=1").unwrap();
        assert!(!lookup(&t, 42).start_ready);
    }

    #[test]
    fn non_utf8_payload_is_rejected() {
        let t = tracker_with_entry("user@1000.service", 42);
        assert!(apply_message(&t, 42, b"\xff\xfe").is_err());
    }

    #[test]
    fn garbage_lines_are_ignored() {
        let t = tracker_with_entry("user@1000.service", 42);
        apply_message(&t, 42, b"WATCHDOG=1\nSTATUS=whatever\n=1\nnonsense").unwrap();
        assert!(!lookup(&t, 42).start_ready);
    }

    #[test]
    fn register_start_forgets_previous_invocation() {
        let manager = NotifyManager::default();
        manager.register_start("user@1000.service", 1);
        manager.register_start("user@1000.service", 2);
        let map = manager.tracker.lock().unwrap();
        assert!(!map.by_pid.contains_key(&1));
        assert!(map.by_pid.contains_key(&2));
    }

    #[test]
    fn register_reload_creates_entry_if_missing() {
        let manager = NotifyManager::default();
        assert!(manager.register_reload("svc.service", 55));
        let map = manager.tracker.lock().unwrap();
        let e = map.by_pid.get(&55).unwrap();
        assert!(e.start_ready);
        assert!(e.reload.is_some());
    }

    #[tokio::test]
    async fn wait_ready_fails_when_process_is_gone() {
        let manager = NotifyManager::default();
        manager.register_start("svc.service", 999_999_999);
        let err = manager.wait_ready("svc.service", 999_999_999, 10).await;
        assert!(err.is_err());
        let err = err.unwrap_err().to_string();
        assert!(err.contains("exited before READY=1"), "{err}");
        // The entry is cleaned up on failure.
        assert!(!manager
            .tracker
            .lock()
            .unwrap()
            .by_pid
            .contains_key(&999_999_999));
    }

    #[tokio::test]
    async fn wait_ready_succeeds_after_readiness() {
        let manager = NotifyManager::default();
        // The test process itself: a live PID that passes the exit checks.
        let pid = std::process::id();
        manager.register_start("svc.service", pid);
        // Simulate the reader thread applying a READY=1 datagram.
        let tracker = manager.tracker.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            apply_message(&tracker, pid, b"READY=1\nSTATUS=Reached basic.target.").unwrap();
        });
        manager.wait_ready("svc.service", pid, 10).await.unwrap();
        // The entry is cleaned up on success.
        assert!(!manager.tracker.lock().unwrap().by_pid.contains_key(&pid));
    }

    #[tokio::test]
    async fn end_to_end_reader_thread_receives_real_datagram() {
        // Bind a real sd_notify socket in a scratch path and exercise the
        // whole chain: socket → reader thread (SO_PASSCRED ucred) →
        // tracker → wait_ready.
        let sock_path = format!("/tmp/opencode/sd-notify-test-{}.sock", std::process::id());
        let _ = std::fs::remove_file(&sock_path);
        let manager = NotifyManager::setup_at(&sock_path).expect("socket setup must succeed");
        let pid = std::process::id();
        manager.register_start("svc.service", pid);

        // A plain datagram from an unbound sender; the kernel attaches
        // SCM_CREDENTIALS because the receiving socket has SO_PASSCRED.
        let sender = std::os::unix::net::UnixDatagram::unbound().unwrap();
        sender.connect(&sock_path).unwrap();
        sender
            .send(b"READY=1\nSTATUS=Reached basic.target.")
            .unwrap();

        manager.wait_ready("svc.service", pid, 10).await.unwrap();
        let _ = std::fs::remove_file(&sock_path);
    }
}
