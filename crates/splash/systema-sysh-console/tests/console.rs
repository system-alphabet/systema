//! End-to-end check of the console frontend against a fake notify
//! directory: it must retry silently while the directory is unusable, join
//! the broadcast channel by binding `console.sock`, and print completed
//! starts with the escape sequences intact even though stdout is a pipe.

use std::fs;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn scratch(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("sysh-console-{}-{tag}", std::process::id()))
}

fn spawn_console(notify_dir: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_systema-sysh.console"))
        .env("SYSTEMA_NOTIFY_DIR", notify_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn console")
}

fn wait_for<F: Fn() -> bool>(cond: F, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    cond()
}

fn send_event(socket: &Path, body: &str) {
    let sender = UnixDatagram::unbound().unwrap();
    sender.send_to(body.as_bytes(), socket).unwrap();
}

/// Strip SGR sequences so the structure can be asserted as plain text.
fn plain(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c in chars.by_ref() {
                if c == 'm' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Assert one `[<start>] <unit> <status> [<end>]` line and return its two
/// timestamps.
fn parse_line(line: &str, unit: &str, status: &str) -> (u64, u64) {
    assert!(
        line.starts_with('['),
        "line does not open with a bracket: {line:?}"
    );
    let (start, rest) = line[1..].split_once(']').expect("start bracket");
    assert_eq!(start.len(), 10, "timestamp field is not 10 wide: {line:?}");
    let start: u64 = start.trim().parse().expect("start timestamp");

    let rest = rest.strip_prefix(' ').expect("space after start bracket");
    let (head, end) = rest.rsplit_once(" [").expect("completion bracket");
    assert_eq!(head, format!("{unit} {status}"));
    let end = end.strip_suffix(']').expect("closing bracket");
    assert_eq!(end.len(), 10, "timestamp field is not 10 wide: {line:?}");
    let end: u64 = end.trim().parse().expect("end timestamp");
    (start, end)
}

#[test]
fn joins_the_broadcast_channel_and_reports_completed_starts() {
    let dir = scratch("live");
    let _ = fs::remove_dir_all(&dir);
    // Occupy the path with a regular file first: the console's bind (and
    // its create_dir_all) then fails, which is exactly the silent-retry
    // path.  Removing the file later is what lets it join.
    fs::write(&dir, b"blocker").unwrap();

    let child = spawn_console(&dir);
    std::thread::sleep(Duration::from_millis(300));

    fs::remove_file(&dir).unwrap();
    fs::create_dir(&dir).unwrap();

    let socket = dir.join("console.sock");
    assert!(
        wait_for(|| socket.exists(), Duration::from_secs(5)),
        "console never bound {}",
        socket.display()
    );

    // A restart-style announcement, then the completion.
    send_event(&socket, "UNIT_STARTING=slow.service\n");
    std::thread::sleep(Duration::from_millis(30));
    send_event(&socket, "UNIT_STARTED=slow.service\nRESULT=success\n");
    // No announcement: the console joined too late, but the completion is
    // still reported.
    send_event(&socket, "UNIT_STARTED=late.service\nRESULT=failed\n");
    // Channel chatter that must not produce output.
    send_event(&socket, "MANAGER_READY=1\n");
    send_event(&socket, "UNIT_STOPPED=slow.service\nRESULT=success\n");
    std::thread::sleep(Duration::from_millis(200));

    let mut child = child;
    child.kill().ok();
    let output = child.wait_with_output().expect("collect console output");

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(stderr, "", "the console must stay silent on stderr");

    let rendered = plain(&stdout);
    let lines: Vec<&str> = rendered.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 2, "unexpected output: {rendered:?}");

    let (start, end) = parse_line(lines[0], "slow.service", "OK");
    assert!(start <= end, "start {start} after completion {end}");
    let (late_start, late_end) = parse_line(lines[1], "late.service", "FAIL");
    assert_eq!(late_start, late_end);

    // stdout is a pipe here, so this also proves the escapes are forced.
    assert!(stdout.contains("\x1b[90m["), "no gray bracket: {stdout:?}");
    assert!(
        stdout.contains("\x1b[97mslow.service\x1b[0m"),
        "no white unit name: {stdout:?}"
    );
    assert!(
        stdout.contains("\x1b[92mOK\x1b[0m"),
        "no green OK: {stdout:?}"
    );
    assert!(
        stdout.contains("\x1b[91mFAIL\x1b[0m"),
        "no red FAIL: {stdout:?}"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_stale_socket_is_reclaimed_by_the_next_run() {
    let dir = scratch("cleanup");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();

    let child = spawn_console(&dir);
    let socket = dir.join("console.sock");
    assert!(
        wait_for(|| socket.exists(), Duration::from_secs(5)),
        "console never bound {}",
        socket.display()
    );

    // SIGKILL: destructors never run, so the socket file survives.  System
    // A would reclaim it on the next broadcast; this checks the faster path
    // — a second console takes the name over at startup.
    let mut child = child;
    child.kill().ok();
    child.wait().ok();
    assert!(
        socket.exists(),
        "killed console should leave the file behind"
    );

    let mut second = spawn_console(&dir);
    assert!(
        wait_for(
            || UnixDatagram::unbound()
                .unwrap()
                .send_to(b"", &socket)
                .is_ok(),
            Duration::from_secs(5)
        ),
        "second console could not take over a stale socket"
    );
    second.kill().ok();
    second.wait().ok();
    let _ = fs::remove_dir_all(&dir);
}
