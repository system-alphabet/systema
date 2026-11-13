//! The notify broadcast channel.
//!
//! A special channel, distinct from the allocator socket: it carries
//! human-visible boot progress events — units starting / started
//! (success/failure) / stopping / stopped, worker startup completion, and
//! the allocator's own readiness.  SysAInit displays these as the bootlog,
//! and the future boot-animation program consumes the same stream.
//!
//! There is **no subscription model**: every listener socket found in the
//! notify directory receives every message.  A listener joins simply by
//! binding its own Unix datagram socket file in the directory (e.g.
//! `init.sock`, `bootanim.sock`); System A enumerates the directory and
//! sends each event to every socket.
//!
//! Message format is sd_notify-style `key=value` lines, one event per
//! datagram:
//!
//! ```text
//! MANAGER_READY=1
//! STATUS=ipc-ready
//! UNIT_STARTING=sshd.service
//! UNIT_STARTED=sshd.service
//! RESULT=success
//! WORKER_READY=system-d-1
//! ```
//!
//! System A announces its own exit with `SHUTDOWN=1`.  Receivers treat that
//! event as "close your listener": the console exits, System Init closes its
//! socket and keeps running.  System A then waits (bounded) until every
//! listener socket file is gone before it leaves.

use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixDatagram;
use std::time::Duration;

use tracing::{info, warn};

/// Key of the allocator's farewell event.
const SHUTDOWN_KEY: &str = "SHUTDOWN";

/// How often [`announce_shutdown_in`] re-broadcasts and re-checks the
/// directory while it waits for the listeners to close.
const SHUTDOWN_POLL: Duration = Duration::from_millis(50);

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

/// Whether one received datagram carries the allocator's shutdown
/// announcement (`SHUTDOWN=1`).
pub fn is_shutdown(body: &str) -> bool {
    body.lines()
        .any(|line| matches!(line.split_once('='), Some((key, _)) if key.trim() == SHUTDOWN_KEY))
}

/// Broadcast one event to every listener socket in the configured notify
/// directory.  Failures are logged and never fatal.
pub fn broadcast(events: &[(&str, &str)]) {
    let dir = crate::paths::instance().notify_dir.clone();
    broadcast_in(&dir, events);
}

/// Broadcast into an explicit directory (used by tests).
pub fn broadcast_in(dir: &str, events: &[(&str, &str)]) {
    let body = build_body(events);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if !ft.is_socket() {
            continue;
        }
        send_to_path(&entry.path(), &body);
    }
}

/// One datagram to one listener socket, dropping a stale socket file whose
/// listener already closed.
fn send_to_path(path: &std::path::Path, body: &str) {
    match UnixDatagram::unbound().and_then(|sock| sock.send_to(body.as_bytes(), path)) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
            // Listener gone; drop the stale socket file.
            let _ = std::fs::remove_file(path);
        }
        Err(e) => {
            warn!("notify broadcast to {} failed: {}", path.display(), e);
        }
    }
}

/// Announce that System A is exiting, then wait until every listener has
/// closed its socket, or `timeout` elapses.
///
/// The announcement is re-sent while waiting: a listener that died without
/// unlinking its socket answers `ECONNREFUSED`, which drops the file, so the
/// wait converges instead of always running out the clock.
pub async fn announce_shutdown(timeout: Duration) {
    let dir = crate::paths::instance().notify_dir.clone();
    announce_shutdown_in(&dir, timeout).await;
}

/// [`announce_shutdown`] against an explicit directory (used by tests).
pub async fn announce_shutdown_in(dir: &str, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        broadcast_in(dir, &[(SHUTDOWN_KEY, "1")]);
        let open = socket_count(dir);
        if open == 0 {
            info!("notify: shutdown announced; every listener closed its socket");
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            warn!("notify: {open} listener socket(s) still open after {timeout:?}; exiting anyway");
            return;
        }
        tokio::time::sleep(SHUTDOWN_POLL).await;
    }
}

/// Number of listener socket files currently in `dir` (a missing directory
/// has none).
fn socket_count(dir: &str) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| entry.file_type().map(|ft| ft.is_socket()).unwrap_or(false))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn test_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("sysa-notify-unit-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn build_body_emits_key_value_lines() {
        assert_eq!(
            build_body(&[("MANAGER_READY", "1"), ("STATUS", "ipc-ready")]),
            "MANAGER_READY=1\nSTATUS=ipc-ready\n"
        );
    }

    #[test]
    fn every_listener_receives_every_event() {
        let dir = test_dir("multi");
        let sock_a = UnixDatagram::bind(dir.join("a.sock")).unwrap();
        let sock_b = UnixDatagram::bind(dir.join("b.sock")).unwrap();

        broadcast_in(dir.to_str().unwrap(), &[("UNIT_STARTED", "foo.service")]);

        let mut buf = [0u8; 256];
        let n = sock_a.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"UNIT_STARTED=foo.service\n");
        let n = sock_b.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"UNIT_STARTED=foo.service\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_socket_files_are_removed() {
        let dir = test_dir("stale");
        let sock = UnixDatagram::bind(dir.join("stale.sock")).unwrap();
        drop(sock); // listener gone; the socket file remains

        broadcast_in(dir.to_str().unwrap(), &[("WORKER_READY", "system-s-1")]);

        assert!(!dir.join("stale.sock").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_socket_files_are_ignored() {
        let dir = test_dir("nonsock");
        fs::write(dir.join("README"), "not a socket\n").unwrap();
        let sock = UnixDatagram::bind(dir.join("live.sock")).unwrap();

        broadcast_in(dir.to_str().unwrap(), &[("MANAGER_READY", "1")]);

        assert!(dir.join("README").exists());
        let mut buf = [0u8; 256];
        let n = sock.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"MANAGER_READY=1\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn shutdown_event_is_recognized_among_other_keys() {
        assert!(is_shutdown("SHUTDOWN=1\n"));
        assert!(is_shutdown("UNIT_STARTED=sshd.service\nSHUTDOWN=1\n"));
        assert!(!is_shutdown("MANAGER_READY=1\n"));
        assert!(!is_shutdown("SHUTDOWN\n"));
        assert!(!is_shutdown(""));
    }

    #[tokio::test]
    async fn announce_shutdown_waits_for_every_listener_to_close() {
        let dir = test_dir("shutdown-close");
        let path = dir.join("listener.sock");
        let sock = UnixDatagram::bind(&path).unwrap();
        let closer_path = path.clone();

        // Answer like the real receivers: consume the datagram, unlink the
        // socket file.
        let closer = std::thread::spawn(move || {
            let mut buf = [0u8; 256];
            let n = sock.recv(&mut buf).unwrap();
            assert!(is_shutdown(&String::from_utf8_lossy(&buf[..n])));
            fs::remove_file(&closer_path).unwrap();
        });

        let started = std::time::Instant::now();
        announce_shutdown_in(dir.to_str().unwrap(), Duration::from_secs(5)).await;
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the wait must end when the listener closes, not at the deadline"
        );
        closer.join().unwrap();
        assert!(!path.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn announce_shutdown_gives_up_at_the_deadline() {
        let dir = test_dir("shutdown-timeout");
        let _sock = UnixDatagram::bind(dir.join("stubborn.sock")).unwrap();

        let started = std::time::Instant::now();
        announce_shutdown_in(dir.to_str().unwrap(), Duration::from_millis(100)).await;
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(100),
            "elapsed: {elapsed:?}"
        );
        assert!(elapsed < Duration::from_secs(2), "elapsed: {elapsed:?}");
        let _ = fs::remove_dir_all(&dir);
    }
}
