//! The notify channel: a one-way event stream from System A to the
//! processes that watch the boot.
//!
//! System A binds `notify.socket` once it has finished loading, then writes
//! every boot-progress event — units starting / started (success/failure) /
//! stopping / stopped, worker startup completion — to every connected
//! client.  SysAInit, System U and the boot console join by *connecting*;
//! a successful connection is itself the readiness signal, so no
//! `MANAGER_READY` datagram is published any more.
//!
//! # Framing
//!
//! One event is one or more `key=value` lines, each terminated by `\n`,
//! followed by an empty line:
//!
//! ```text
//! UNIT_STARTING=sshd.service
//!
//! UNIT_STARTED=sshd.service
//! RESULT=success
//!
//! WORKER_READY=system-d-1
//! ```
//!
//! (Every event above ends with a blank line; the examples show one event
//! each.)  [`NotifyClient::recv_event`] returns the lines of one event,
//! still newline-terminated.
//!
//! System A announces its own exit with `SHUTDOWN=1`.  Receivers treat that
//! event as "close your connection": the console exits, System Init closes
//! the stream and keeps running.  System A then waits (bounded) until every
//! client has disconnected before it leaves.

use std::io::{self, Read};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tracing::{info, warn};

/// Key of the allocator's farewell event.
const SHUTDOWN_KEY: &str = "SHUTDOWN";

/// How often [`NotifyHub::announce_shutdown`] re-broadcasts and re-checks
/// the client list while it waits for them to leave.
const SHUTDOWN_POLL: Duration = Duration::from_millis(50);

/// How often a waiting client retries `connect(2)` when it is not woken by
/// a filesystem event (and on platforms without inotify).
const CONNECT_POLL: Duration = Duration::from_millis(50);

/// How many queued events a slow client may fall behind before its
/// connection is dropped (notify is best effort, and a stuck reader must
/// not grow System A's memory without bound).
const CLIENT_QUEUE: usize = 1024;

/// Serialize one event as `key=value` lines (each line terminated).
pub fn build_body(events: &[(&str, &str)]) -> String {
    let mut body = String::new();
    for (key, value) in events {
        body.push_str(key);
        body.push('=');
        body.push_str(value);
        body.push('\n');
    }
    body
}

/// Whether one received event carries the allocator's shutdown
/// announcement (`SHUTDOWN=1`).
pub fn is_shutdown(body: &str) -> bool {
    body.lines()
        .any(|line| matches!(line.split_once('='), Some((key, _)) if key.trim() == SHUTDOWN_KEY))
}

/// One framed event: its `key=value` lines plus the blank-line separator.
fn frame(events: &[(&str, &str)]) -> Vec<u8> {
    let mut frame = build_body(events);
    frame.push('\n');
    frame.into_bytes()
}

/// The allocator side of the notify channel.
///
/// Every client gets its own queue and writer task, so a slow reader cannot
/// block System A or delay the other clients, while each client still sees
/// the events in order.
pub struct NotifyHub {
    path: PathBuf,
    clients: Arc<Mutex<Vec<mpsc::Sender<Vec<u8>>>>>,
}

impl NotifyHub {
    /// Bind `path` and start accepting clients.
    ///
    /// Calling this is what publishes the socket, so it must run once
    /// System A has finished loading.  A stale socket file from a previous
    /// run is removed first.
    pub fn bind(path: &str) -> io::Result<Self> {
        let path = PathBuf::from(path);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path)?;
        info!("Notify socket listening on {}", path.display());

        let clients: Arc<Mutex<Vec<mpsc::Sender<Vec<u8>>>>> = Arc::new(Mutex::new(Vec::new()));
        let accepted = Arc::clone(&clients);
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(CLIENT_QUEUE);
                        accepted.lock().unwrap().push(tx);
                        info!("Notify client connected");
                        tokio::spawn(async move {
                            let mut stream = stream;
                            while let Some(event) = rx.recv().await {
                                if stream.write_all(&event).await.is_err() {
                                    break;
                                }
                            }
                            let _ = stream.shutdown().await;
                        });
                    }
                    Err(e) => {
                        warn!("notify accept error: {e}");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        });

        Ok(Self { path, clients })
    }

    /// Send one event to every connected client.  Failures are logged and
    /// never fatal; a client whose queue is full or whose connection is
    /// gone is dropped.
    pub fn broadcast(&self, events: &[(&str, &str)]) {
        let event = frame(events);
        self.clients
            .lock()
            .unwrap()
            .retain(|client| client.try_send(event.clone()).is_ok());
    }

    /// Announce that System A is exiting, then wait until every client has
    /// disconnected, or `timeout` elapses.  Finally removes the socket
    /// file.
    pub async fn announce_shutdown(&self, timeout: Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            self.broadcast(&[(SHUTDOWN_KEY, "1")]);
            let open = self.clients.lock().unwrap().len();
            if open == 0 {
                info!("notify: shutdown announced; every client disconnected");
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                warn!("notify: {open} client(s) still connected after {timeout:?}; exiting anyway");
                break;
            }
            tokio::time::sleep(SHUTDOWN_POLL).await;
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The client side of the notify channel.
pub struct NotifyClient {
    stream: UnixStream,
    pending: Vec<u8>,
}

impl NotifyClient {
    /// Connect to `path`, waiting for the socket to appear.
    ///
    /// `should_stop` is polled while waiting; returning `true` aborts with
    /// [`io::ErrorKind::Interrupted`].  On Linux the wait rides an inotify
    /// watch on the socket's directory; elsewhere it polls.
    pub fn connect_waiting<F>(path: &str, should_stop: F) -> io::Result<Self>
    where
        F: Fn() -> bool,
    {
        let path = Path::new(path);
        let mut watcher = WaitWatcher::new(path.parent());
        loop {
            if should_stop() {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "notify connect cancelled",
                ));
            }
            match UnixStream::connect(path) {
                Ok(stream) => {
                    return Ok(Self {
                        stream,
                        pending: Vec::new(),
                    })
                }
                Err(_) => watcher.wait(CONNECT_POLL),
            }
        }
    }

    /// Set a read timeout; with one, [`Self::recv_event`] returns
    /// `WouldBlock`/`TimedOut` when no event arrives in time.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.stream.set_read_timeout(timeout)
    }

    /// Receive one framed event, or `None` when System A closed the
    /// connection.
    pub fn recv_event(&mut self) -> io::Result<Option<String>> {
        loop {
            if let Some(end) = find_frame_end(&self.pending) {
                let body = String::from_utf8_lossy(&self.pending[..end]).into_owned();
                self.pending.drain(..end + 2);
                return Ok(Some(format!("{body}\n")));
            }
            let mut chunk = [0u8; 4096];
            match self.stream.read(&mut chunk) {
                Ok(0) => return Ok(None),
                Ok(n) => self.pending.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }
}

/// Position of the blank-line separator in `pending`, if a whole event is
/// buffered.
fn find_frame_end(pending: &[u8]) -> Option<usize> {
    pending.windows(2).position(|pair| pair == b"\n\n")
}

/// Waits for the socket file to (re)appear.
///
/// On Linux this is an inotify watch on the socket's directory; every other
/// platform falls back to sleeping for the poll interval.  A missing
/// directory simply degrades to polling.
struct WaitWatcher {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    inotify: Option<inotify::Inotify>,
}

impl WaitWatcher {
    fn new(dir: Option<&Path>) -> Self {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            use inotify::{Inotify, WatchMask};

            let inotify = dir.and_then(|dir| {
                let inotify = Inotify::init().ok()?;
                inotify
                    .watches()
                    .add(dir, WatchMask::CREATE | WatchMask::MOVED_TO)
                    .ok()?;
                Some(inotify)
            });
            Self { inotify }
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            let _ = dir;
            Self {}
        }
    }

    fn wait(&mut self, timeout: Duration) {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            use std::os::unix::io::AsRawFd;

            if let Some(inotify) = self.inotify.as_mut() {
                let mut pollfd = libc::pollfd {
                    fd: inotify.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                let millis = timeout.as_millis().min(i32::MAX as u128) as i32;
                // A watcher on a directory that disappeared fires
                // IN_IGNORED; reading the queued events keeps the loop from
                // spinning on a permanently readable descriptor.
                let ready = unsafe { libc::poll(&mut pollfd, 1, millis) };
                if ready > 0 {
                    let mut buffer = [0u8; 4096];
                    let _ = inotify.read_events(&mut buffer);
                }
                return;
            }
        }
        std::thread::sleep(timeout);
    }
}

/// Handle used by System A: the process-wide hub created by [`start`].
static HUB: std::sync::OnceLock<NotifyHub> = std::sync::OnceLock::new();

/// Bind the notify socket and publish it.  System A calls this exactly
/// once, when it has finished loading; clients treat a successful
/// connection as allocator readiness.
pub fn start() -> io::Result<()> {
    let hub = NotifyHub::bind(crate::paths::instance().notify_socket)?;
    HUB.set(hub).map_err(|_| {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "notify channel already started",
        )
    })
}

/// Broadcast one event to every connected client.  No-op until [`start`]
/// has run.
pub fn broadcast(events: &[(&str, &str)]) {
    if let Some(hub) = HUB.get() {
        hub.broadcast(events);
    }
}

/// Announce that System A is exiting and wait (bounded) for the clients to
/// disconnect.  No-op until [`start`] has run.
pub async fn announce_shutdown(timeout: Duration) {
    if let Some(hub) = HUB.get() {
        hub.announce_shutdown(timeout).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn test_socket(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("sysa-notify-unit-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("notify.socket")
    }

    async fn wait_for_clients(hub: &NotifyHub, expected: usize) {
        for _ in 0..200 {
            if hub.clients.lock().unwrap().len() == expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "expected {expected} notify client(s), got {}",
            hub.clients.lock().unwrap().len()
        );
    }

    #[test]
    fn build_body_emits_key_value_lines() {
        assert_eq!(
            build_body(&[("UNIT_STARTED", "foo.service"), ("RESULT", "success")]),
            "UNIT_STARTED=foo.service\nRESULT=success\n"
        );
    }

    #[test]
    fn shutdown_event_is_recognized_among_other_keys() {
        assert!(is_shutdown("SHUTDOWN=1\n"));
        assert!(is_shutdown("UNIT_STARTED=sshd.service\nSHUTDOWN=1\n"));
        assert!(!is_shutdown("UNIT_STARTED=sshd.service\n"));
        assert!(!is_shutdown("SHUTDOWN\n"));
        assert!(!is_shutdown(""));
    }

    #[tokio::test]
    async fn clients_receive_framed_events_in_order() {
        let path = test_socket("roundtrip");
        let hub = NotifyHub::bind(path.to_str().unwrap()).unwrap();

        let reader = {
            let path = path.clone();
            std::thread::spawn(move || {
                let mut client = NotifyClient::connect_waiting(path.to_str().unwrap(), || false)
                    .expect("connect");
                let first = client.recv_event().unwrap().unwrap();
                let second = client.recv_event().unwrap().unwrap();
                (first, second)
            })
        };
        wait_for_clients(&hub, 1).await;

        hub.broadcast(&[("UNIT_STARTING", "a.service")]);
        hub.broadcast(&[("UNIT_STARTED", "a.service"), ("RESULT", "success")]);

        let (first, second) = tokio::task::spawn_blocking(move || reader.join().unwrap())
            .await
            .unwrap();
        assert_eq!(first, "UNIT_STARTING=a.service\n");
        assert_eq!(second, "UNIT_STARTED=a.service\nRESULT=success\n");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn every_client_receives_every_event() {
        let path = test_socket("multi");
        let hub = NotifyHub::bind(path.to_str().unwrap()).unwrap();

        let readers: Vec<_> = (0..2)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let mut client =
                        NotifyClient::connect_waiting(path.to_str().unwrap(), || false)
                            .expect("connect");
                    client.recv_event().unwrap().unwrap()
                })
            })
            .collect();
        wait_for_clients(&hub, 2).await;

        hub.broadcast(&[("UNIT_STOPPED", "b.service")]);
        for reader in readers {
            // The writer task needs the runtime, so the blocking join runs
            // off the async worker thread.
            let event = tokio::task::spawn_blocking(move || reader.join().unwrap())
                .await
                .unwrap();
            assert_eq!(event, "UNIT_STOPPED=b.service\n");
        }
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn shutdown_wait_ends_when_the_clients_disconnect() {
        let path = test_socket("shutdown");
        let hub = NotifyHub::bind(path.to_str().unwrap()).unwrap();

        let reader = {
            let path = path.clone();
            std::thread::spawn(move || {
                let mut client = NotifyClient::connect_waiting(path.to_str().unwrap(), || false)
                    .expect("connect");
                let event = client.recv_event().unwrap().unwrap();
                assert!(is_shutdown(&event));
                // Dropping the client closes the connection, which is what
                // System A waits for.
            })
        };
        wait_for_clients(&hub, 1).await;

        let started = std::time::Instant::now();
        hub.announce_shutdown(Duration::from_secs(5)).await;
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the wait must end when the client leaves, not at the deadline"
        );
        reader.join().unwrap();
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn connect_waiting_can_be_cancelled() {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let waiter = std::thread::spawn(move || {
            NotifyClient::connect_waiting("/nonexistent/notify.socket", || {
                flag.load(Ordering::SeqCst)
            })
        });

        // No socket will ever appear; cancellation must end the wait.
        tokio::time::sleep(Duration::from_millis(100)).await;
        stop.store(true, Ordering::SeqCst);
        let err = waiter.join().unwrap().err().expect("cancelled");
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
    }
}
