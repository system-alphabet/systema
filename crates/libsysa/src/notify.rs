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

use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixDatagram;

use tracing::warn;

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
        let path = entry.path();
        match UnixDatagram::unbound().and_then(|sock| sock.send_to(body.as_bytes(), &path)) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                // Listener gone; drop the stale socket file.
                let _ = std::fs::remove_file(&path);
            }
            Err(e) => {
                warn!("notify broadcast to {} failed: {}", path.display(), e);
            }
        }
    }
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
}
