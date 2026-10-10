//! Socket-activation monitors for `Accept=no` sockets.
//!
//! systemd semantics being replicated: a socket unit with `Accept=no` hands
//! its listening fds to the associated service and stops watching them;
//! when the service stops again, the socket watches once more and data
//! arrival re-activates it.  This is what makes e.g. `systemd-udevd.socket`
//! activate `systemd-udevd.service` on the very first uevent even when the
//! trigger service raced ahead of the daemon.
//!
//! Implementation: one monitor thread per listener fd.  The thread polls(2)
//! for readability and never reads — no data is ever consumed from the
//! socket (it stays intact for the service that receives the fd), and
//! `O_NONBLOCK` is deliberately not touched because it is shared across
//! duplicated fds.
//!
//! Lifecycle: armed → readable edge → publish fire event → suppressed.
//! The allocator reports service state changes back as `event.publish`;
//! [`ActivationRegistry::apply_service_state`] re-arms monitors whose
//! service is no longer active/activating and suppresses those whose is.

use std::collections::{HashMap, HashSet, VecDeque};
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info, warn};

/// At most [`TRIGGER_LIMIT_BURST`] fires per [`TRIGGER_LIMIT_INTERVAL`]
/// per listener (mirrors systemd's TriggerLimitIntervalSec/Burst).
const TRIGGER_LIMIT_INTERVAL: Duration = Duration::from_secs(1);
const TRIGGER_LIMIT_BURST: usize = 10;

/// How often a suppressed monitor re-checks its flags / an armed monitor
/// wakes up to notice `stop`.
const POLL_TICK: Duration = Duration::from_millis(250);

#[derive(Debug, Default)]
struct MonitorFlags {
    /// Whether readable edges currently cause fires.
    armed: AtomicBool,
    /// Set when the socket unit is stopped; terminates the thread.
    stop: AtomicBool,
}

struct Monitor {
    flags: Arc<MonitorFlags>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Registry of all activation monitors owned by this worker.
///
/// Connection-independent: lives across IPC reconnects.  Fired events are
/// funneled through `tx` into a tokio forwarder task that stamps them onto
/// whichever `EventPublisher` is currently connected.
pub struct ActivationRegistry {
    /// Socket unit name → its monitors (one per listener fd).
    units: Mutex<HashMap<String, Vec<Monitor>>>,
    /// Service name → socket units watching it (for rearm routing).
    by_service: Mutex<HashMap<String, HashSet<String>>>,
    tx: UnboundedSender<String>,
}

impl ActivationRegistry {
    /// Create the registry plus the receiving half for the forwarder task.
    pub fn new() -> (Arc<Self>, tokio::sync::mpsc::UnboundedReceiver<String>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (
            Arc::new(ActivationRegistry {
                units: Mutex::new(HashMap::new()),
                by_service: Mutex::new(HashMap::new()),
                tx,
            }),
            rx,
        )
    }

    /// Start one monitor thread per given fd for `unit_name`, which
    /// activates `service_name`.  Replaces any previous monitors for the
    /// unit (a restart of the socket unit).
    pub fn start_unit(&self, unit_name: &str, service_name: &str, fds: &[RawFd]) {
        self.stop_unit(unit_name);

        let mut monitors = Vec::new();
        for &fd in fds {
            let flags = Arc::new(MonitorFlags::default());
            // Arm *before* spawning: a service that becomes active between
            // spawn and the thread's first instruction must not be undone
            // by the thread unconditionally re-arming itself.
            flags.armed.store(true, Ordering::Release);
            let thread = spawn_monitor(unit_name.to_string(), fd, self.tx.clone(), flags.clone());
            monitors.push(Monitor {
                flags,
                thread: Some(thread),
            });
        }

        self.units.lock().insert(unit_name.to_string(), monitors);
        if !service_name.is_empty() {
            self.by_service
                .lock()
                .entry(service_name.to_string())
                .or_default()
                .insert(unit_name.to_string());
        }
        debug!(
            "Activation monitoring '{}' (service '{}') on {} fd(s)",
            unit_name,
            service_name,
            fds.len()
        );
    }

    /// Stop and reap all monitors for a socket unit.
    pub fn stop_unit(&self, unit_name: &str) {
        let mut units = self.units.lock();
        if let Some(mut monitors) = units.remove(unit_name) {
            for mon in monitors.iter_mut() {
                mon.flags.stop.store(true, Ordering::Release);
            }
            for mon in monitors.iter_mut() {
                if let Some(t) = mon.thread.take() {
                    let _ = t.join();
                }
            }
        }
        drop(units);
        let mut by_service = self.by_service.lock();
        for set in by_service.values_mut() {
            set.remove(unit_name);
        }
        by_service.retain(|_, s| !s.is_empty());
    }

    /// Apply allocator-side service state feedback.
    ///
    /// `active_state` "active"/"activating"/"reloading" means the service
    /// owns the sockets now — suppress the monitors.  Anything else
    /// ("inactive", "failed", …) re-arms them so the next data edge
    /// re-activates the service.
    pub fn apply_service_state(&self, service: &str, active_state: &str) {
        let watchers: Vec<String> = match self.by_service.lock().get(service) {
            Some(set) => set.iter().cloned().collect(),
            None => return,
        };
        if watchers.is_empty() {
            return;
        }
        let suppress = matches!(active_state, "active" | "activating" | "reloading");
        let units = self.units.lock();
        for w in watchers {
            if let Some(mons) = units.get(&w) {
                for mon in mons {
                    mon.flags.armed.store(!suppress, Ordering::Release);
                }
                debug!(
                    "Socket '{}' monitors {} (service '{}' {})",
                    w,
                    if suppress { "suppressed" } else { "re-armed" },
                    service,
                    active_state
                );
            }
        }
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        // Safety net for paths that skip stop_unit (e.g. a panicking test
        // or early error): signal the thread, *then* join so a monitor
        // parked in its sleep loop cannot hang the drop forever.
        if let Some(t) = self.thread.take() {
            self.flags.stop.store(true, Ordering::Release);
            let _ = t.join();
        }
    }
}

fn spawn_monitor(
    unit: String,
    fd: RawFd,
    tx: UnboundedSender<String>,
    flags: Arc<MonitorFlags>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name(format!("sock-act-{unit}"))
        .spawn(move || {
            let mut window: VecDeque<Instant> = VecDeque::new();
            debug!("Activation monitor for '{unit}' (fd {fd}) armed");
            while !flags.stop.load(Ordering::Acquire) {
                if !flags.armed.load(Ordering::Acquire) {
                    std::thread::sleep(POLL_TICK);
                    continue;
                }
                match poll_readable(fd, POLL_TICK) {
                    PollResult::Timeout => continue,
                    PollResult::Invalid => {
                        info!("Activation monitor for '{unit}': fd closed — exiting");
                        break;
                    }
                    PollResult::Readable => {}
                }

                // Re-check arming: the flag may have been cleared (service
                // took over) while we sat in poll().  The data stays queued
                // for whoever owns the fd; nothing is lost.
                if !flags.armed.load(Ordering::Acquire) {
                    continue;
                }

                // Readable edge while armed: apply the trigger limit.
                let now = Instant::now();
                while window
                    .front()
                    .is_some_and(|t| now.duration_since(*t) > TRIGGER_LIMIT_INTERVAL)
                {
                    window.pop_front();
                }
                if window.len() >= TRIGGER_LIMIT_BURST {
                    debug!("Trigger limit hit for '{unit}' — dropping event");
                    continue;
                }
                window.push_back(now);

                // Suppress before publishing so at most one fire is in flight.
                flags.armed.store(false, Ordering::Release);
                info!("Socket '{unit}' readable — requesting activation");
                if tx.send(unit.clone()).is_err() {
                    warn!("Activation channel closed — stopping monitor for '{unit}'");
                    break;
                }
            }
            debug!("Activation monitor for '{unit}' exited");
        })
        .expect("spawn socket activation monitor thread")
}

enum PollResult {
    Readable,
    Timeout,
    Invalid,
}

/// poll(2) the fd for readability without consuming anything or touching
/// its status flags.  HUP/ERR count as readable edges (e.g. FIFO writer
/// hang-up still carries data worth activating for); NVAL means closed.
fn poll_readable(fd: RawFd, timeout: Duration) -> PollResult {
    let mut fds = [libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    }];
    let timeout_ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    loop {
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), 1, timeout_ms) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            warn!("poll() failed on fd {fd}: {err}");
            return PollResult::Invalid;
        }
        if rc == 0 {
            return PollResult::Timeout;
        }
        let revents = fds[0].revents;
        if revents & libc::POLLNVAL != 0 {
            return PollResult::Invalid;
        }
        if revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            return PollResult::Readable;
        }
        return PollResult::Timeout;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;

    fn wait_until(mut pred: impl FnMut() -> bool, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if pred() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        pred()
    }

    #[test]
    fn fires_on_readable_and_suppresses_then_rearms() {
        let (registry, mut rx) = ActivationRegistry::new();
        let (a, b) = UnixStream::pair().unwrap();
        let fd = a.as_raw_fd();

        registry.start_unit("test.socket", "test.service", &[fd]);
        assert!(rx.try_recv().is_err(), "no event expected before data");

        // Write data: readable edge must produce exactly one fire.
        use std::io::Write;
        let mut b = b;
        b.write_all(b"x").unwrap();
        assert!(
            wait_until(|| rx.try_recv().is_ok(), Duration::from_secs(2)),
            "expected fire after write"
        );

        // Suppressed: further writes must not produce more events.  Drain
        // any pending event first, then confirm the channel stays quiet.
        let _ = rx.try_recv();
        b.write_all(b"y").unwrap();
        std::thread::sleep(Duration::from_millis(400));
        assert!(rx.try_recv().is_err(), "monitor should be suppressed");

        // Service went inactive: re-arm → next write fires again.
        registry.apply_service_state("test.service", "inactive");
        b.write_all(b"z").unwrap();
        assert!(
            wait_until(|| rx.try_recv().is_ok(), Duration::from_secs(2)),
            "expected re-fire after rearm"
        );

        registry.stop_unit("test.socket");
    }

    #[test]
    fn service_active_suppresses_before_first_edge() {
        let (registry, mut rx) = ActivationRegistry::new();
        let (a, mut b) = UnixStream::pair().unwrap();

        registry.start_unit("s2.socket", "s2.service", &[a.as_raw_fd()]);
        registry.apply_service_state("s2.service", "active");

        use std::io::Write;
        b.write_all(b"x").unwrap();
        std::thread::sleep(Duration::from_millis(400));
        assert!(rx.try_recv().is_err(), "active service must suppress");

        registry.apply_service_state("s2.service", "inactive");
        assert!(
            wait_until(|| rx.try_recv().is_ok(), Duration::from_secs(2)),
            "data already queued must fire promptly after rearm"
        );
        registry.stop_unit("s2.socket");
    }

    #[test]
    fn stop_unit_terminates_thread() {
        let (registry, rx) = ActivationRegistry::new();
        let mut rx = rx;
        let (a, mut b) = UnixStream::pair().unwrap();
        registry.start_unit("s3.socket", "s3.service", &[a.as_raw_fd()]);
        registry.stop_unit("s3.socket");
        // After stop, no fires may arrive.
        use std::io::Write;
        b.write_all(b"x").unwrap();
        std::thread::sleep(Duration::from_millis(400));
        assert!(rx.try_recv().is_err(), "stopped monitor must not fire");
    }

    #[test]
    fn invalid_fd_exits_cleanly() {
        let (registry, _rx) = ActivationRegistry::new();
        registry.start_unit("s4.socket", "s4.service", &[i32::MAX - 1]);
        // POLLNVAL path: thread exits on its own; nothing to assert beyond
        // not hanging/crashing.  Give it a moment then stop idempotently.
        std::thread::sleep(Duration::from_millis(300));
        registry.stop_unit("s4.socket");
    }
}
