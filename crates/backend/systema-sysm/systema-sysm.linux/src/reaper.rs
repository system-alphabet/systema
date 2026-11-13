//! Child shutdown for the mount worker.
//!
//! `linux.rs` registers this process as a child subreaper, so daemons that
//! `mount(8)` leaves behind (the `mount.fuse` helper exits and the FUSE
//! daemon it started becomes our child instead of PID 1's).  They are our
//! children from then on, which makes stopping them on shutdown this
//! worker's job.
//!
//! The SIGTERM/wait/SIGKILL sequence mirrors System S's service stop
//! (`systema-syss/src/process.rs::stop_service`): TERM the tree, give it a
//! bounded amount of time to exit, then KILL what is left.  Unlike System S
//! there is no per-unit registry or cgroup to enumerate, so the process set
//! comes straight from `/proc`, and the wait doubles as the reap — the
//! kernel keeps an exited child as a zombie until its parent collects it.

use std::time::{Duration, Instant};
use tracing::{info, warn};

/// How long the children get to exit after SIGTERM, matching System S's
/// worker-wide shutdown budget.
const STOP_TIMEOUT_SECS: u64 = 15;

/// Interval between liveness checks while the children shut down.
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// One process whose parent is this worker.
struct Child {
    pid: u32,
    comm: String,
}

/// Terminate every child process of this worker: the transient
/// `mount`/`umount` helpers plus the daemons adopted through the subreaper
/// role.  Idempotent — a second call finds nothing left to signal.
pub(crate) async fn stop_children() {
    let mut remaining = scan_children();
    if remaining.is_empty() {
        info!("Shutdown: no child processes to stop");
        return;
    }

    info!(
        "Shutdown: stopping {} child process(es): {:?}",
        remaining.len(),
        remaining
            .iter()
            .map(|child| child.comm.as_str())
            .collect::<Vec<_>>()
    );

    // Phase 1 — SIGTERM every child, like System S's stop.
    for child in &remaining {
        send_signal(child.pid, libc::SIGTERM);
        info!(
            "Shutdown: sent SIGTERM to {} (PID {})",
            child.comm, child.pid
        );
    }

    // Phase 2 — wait for them to exit, reaping each one as it goes: we are
    // the parent, so nothing else can collect the exit status.
    let deadline = Instant::now() + Duration::from_secs(STOP_TIMEOUT_SECS);
    loop {
        remaining.retain(|child| !reap_if_exited(child.pid));
        if remaining.is_empty() {
            info!("Shutdown: all child processes exited");
            return;
        }
        if Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(STOP_POLL_INTERVAL).await;
    }

    // Phase 3 — SIGKILL the stragglers, as System S's stop escalates.
    for child in &remaining {
        warn!(
            "Shutdown: {} (PID {}) did not exit in {}s; sending SIGKILL",
            child.comm, child.pid, STOP_TIMEOUT_SECS
        );
        send_signal(child.pid, libc::SIGKILL);
    }
}

/// Deliver `signal` to `pid` — to its whole process group when it is still
/// its own group leader (a daemonized FUSE helper calls `setsid()`), and to
/// the single process otherwise.  Mirrors System S's `group_leader_of` +
/// `killpg` fallback so a daemon's children are covered too.
fn send_signal(pid: u32, signal: libc::c_int) {
    let pid = pid as libc::pid_t;
    // SAFETY: getpgid(2)/kill(2) with a pid read from /proc and a constant
    // signal; a vanished process merely makes them return -1/ESRCH.
    let pgid = unsafe { libc::getpgid(pid) };
    unsafe {
        if pgid == pid {
            libc::kill(-pgid, signal);
        } else {
            libc::kill(pid, signal);
        }
    }
}

/// Collect `pid` if it already exited.  Returns `true` when it is gone —
/// reaped now, or no longer our child (a reconnect/reparent race).
fn reap_if_exited(pid: u32) -> bool {
    let mut status: libc::c_int = 0;
    // SAFETY: `status` is a valid out-parameter and WNOHANG never blocks.
    // 0 means "still running"; the pid means "collected"; -1 is ECHILD (not
    // ours any more) or a transient error we must not spin on.
    let ret = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
    ret != 0
}

/// PIDs (with `comm`) whose parent is this worker, read from `/proc`.
///
/// `/proc/<pid>/stat` is `pid (comm) state ppid ...`; `comm` may itself
/// contain spaces and parentheses, so it is delimited by the first `(` and
/// the last `)`.
fn scan_children() -> Vec<Child> {
    let me = std::process::id();
    let mut children = Vec::new();

    let Ok(dir) = std::fs::read_dir("/proc") else {
        warn!("Shutdown: cannot read /proc; child processes will be left running");
        return children;
    };
    for entry in dir.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue; // vanished between readdir and open
        };
        let Some((comm, rest)) = split_stat(&stat) else {
            continue;
        };
        let mut fields = rest.split_whitespace();
        if fields.next().is_none() {
            continue; // state
        }
        let Some(ppid) = fields.next().and_then(|f| f.parse::<u32>().ok()) else {
            continue;
        };
        if ppid == me {
            if let Ok(pid) = name.parse::<u32>() {
                children.push(Child {
                    pid,
                    comm: comm.to_string(),
                });
            }
        }
    }
    children
}

/// Split `/proc/<pid>/stat` into `(comm, remainder)` around the parenthesised
/// command name.
fn split_stat(stat: &str) -> Option<(&str, &str)> {
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    if close < open {
        return None;
    }
    Some((&stat[open + 1..close], stat[close + 1..].trim_start()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_stat_plain() {
        let (comm, rest) = split_stat("1234 (vmware-vmblock-fuse) S 1000 1000 1000 0 -1").unwrap();
        assert_eq!(comm, "vmware-vmblock-fuse");
        assert!(rest.starts_with("S 1000"));
    }

    #[test]
    fn split_stat_comm_with_parens_and_space() {
        let (comm, rest) = split_stat("7 (a b (c)) R 1 1 1 0 -1").unwrap();
        assert_eq!(comm, "a b (c)");
        assert!(rest.starts_with("R 1"));
    }

    #[test]
    fn split_stat_rejects_garbage() {
        assert!(split_stat("not a stat file").is_none());
        assert!(split_stat("1 ) 2 ( 3").is_none());
    }

    /// A real child (the stand-in for an adopted mount daemon) is found by
    /// the `/proc` scan, terminated and reaped.
    #[tokio::test]
    async fn stop_children_terminates_and_reaps_a_child() {
        let _child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("spawn the test child");
        let pid = _child.id();
        assert!(
            scan_children().iter().any(|c| c.pid == pid),
            "the spawned child must show up as this worker's child"
        );

        stop_children().await;

        // The kernel only reports the child as gone once its exit status has
        // been collected, so this also asserts the reap.
        assert!(
            !scan_children().iter().any(|c| c.pid == pid),
            "the child must be stopped and reaped"
        );
    }
}
