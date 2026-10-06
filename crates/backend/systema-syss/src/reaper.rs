//! Orphan reaper — the subreaper half of a PID 1's SIGCHLD duty.
//!
//! System S registers itself as a child subreaper
//! (`PR_SET_CHILD_SUBREAPER`, main.rs), so when an intermediate parent in a
//! service's process tree dies, its children — *including already-dead
//! zombies* — are reparented to us.  Nothing used to collect them:
//! `monitor_service` only `try_wait()`s the direct child it spawned itself,
//! and `reap_children()` runs at shutdown only.
//!
//! A leftover zombie is not merely cosmetic.  A zombie still answers
//! `kill(pid, sig)` with success, so kernel code that probes a process by
//! signalling it believes the message was delivered.  That is exactly the
//! VT deadlock seen on `init=/bin/bash` boots: plymountd takes
//! `VT_SETMODE VT_PROCESS` on tty1 and dies; Xorg's `VT_ACTIVATE` reaches
//! `change_console()` (drivers/tty/vt/vt_ioctl.c), whose
//! `kill_pid(vt_pid, relsig) == 0` says "owner will answer VT_RELDISP" —
//! and the zombie owner never does, so `VT_WAITACTIVE` blocks forever and
//! lightdm never gets a ready X.  Reaping the zombie restores the kernel's
//! own fallback: `kill_pid` fails, `change_console` reverts the console to
//! `VT_AUTO`, the switch completes.
//!
//! This task mirrors what systemd PID 1 does in `manager_dispatch_sigchld()`
//! (`src/core/manager.c:3176`): a non-destructive `waitid(WEXITED|WNOHANG|
//! WNOWAIT)` peek tells us whether any child is a zombie; only then do we do
//! the real work.  Two ordering rules keep us from stealing a pid another
//! code path is about to wait on (the monitor, `await_oneshot_exit`, or
//! `stop_service`):
//!
//! 1. children are enumerated from `/proc` **before** the registry snapshot:
//!    a process spawned during the sweep is registered by the time we
//!    compare (spawning and registering happen back-to-back with no `.await`
//!    between them, and this sweep never yields between the two steps), so it
//!    is either in the snapshot (skipped) or not in `/proc` yet (invisible);
//! 2. only pids *absent* from the snapshot — pids no unit expects to wait
//!    for — are passed to `waitpid(pid, WNOHANG)`.  A unit already in
//!    `Stopping` counts as absent: `monitor_service` drops its `Child`
//!    without reaping as soon as it sees that state, so nobody will ever
//!    collect that exit status but us.
//!
//! Everything from the peek to the last `waitpid` is synchronous, so on the
//! current-thread runtime no other task can fork, register, or reap in
//! between.

use std::collections::HashSet;
use std::time::Duration;

use tokio::signal::unix::{Signal, SignalKind};
use tracing::{info, warn};

use crate::ipc::SharedRegistry;
#[cfg(any(target_os = "linux", target_os = "android"))]
use crate::state::ServiceState;

/// Safety-net period.  SIGCHLD is the primary trigger (a reparented zombie
/// notifies its new parent); the timer catches coalesced or missed signals
/// and zombies whose original parent never died but never reaped either.
const SWEEP_INTERVAL: Duration = Duration::from_millis(500);

/// Run the reaper until the surrounding `select!` drops us (shutdown).
///
/// Enabled on Linux regardless of `--no-raper`: with a subreaper this
/// collects adopted orphans, without one it still collects our own direct
/// children that no path waits for any more (e.g. a unit left `Stopping`).
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) async fn run(shared: &SharedRegistry) {
    let mut sigchld = match tokio::signal::unix::signal(SignalKind::child()) {
        Ok(s) => Some(s),
        Err(e) => {
            // Timer-only fallback: never fail to start over this.
            warn!("orphan reaper: cannot listen for SIGCHLD ({e}); relying on timer");
            None
        }
    };

    let mut ticker = tokio::time::interval(SWEEP_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        // The `Option` result keeps both branches distinguishable without
        // holding a borrow of `sigchld` across the handlers.
        let chld = tokio::select! {
            sig = wait_sigchld(&mut sigchld) => Some(sig),
            _ = ticker.tick() => None,
        };
        if chld == Some(false) {
            sigchld = None; // listener exhausted → timer-only from here
        }
        sweep(shared).await;
    }
}

/// Non-Linux placeholder: no `/proc` enumeration, and the platform's
/// non-destructive child peek is not portable.  Log once and idle; the
/// shutdown path still reaps everything.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub(crate) async fn run(_shared: &SharedRegistry) {
    warn!("orphan reaper: not implemented on this platform; zombies may accumulate");
    std::future::pending::<()>().await
}

/// Await one SIGCHLD (returns `false` once the stream is exhausted).
#[cfg(any(target_os = "linux", target_os = "android"))]
async fn wait_sigchld(sig: &mut Option<Signal>) -> bool {
    match sig {
        Some(s) => s.recv().await.is_some(),
        // No listener: never ready, only the timer wakes us.
        None => std::future::pending().await,
    }
}

/// One non-destructive probe + one reaping pass.
#[cfg(any(target_os = "linux", target_os = "android"))]
async fn sweep(shared: &SharedRegistry) {
    // 1. Peek.  A zeroed siginfo keeps this safe whichever way the kernel
    //    behaves: it only fills siginfo when it found a child, so
    //    si_signo == 0 means "nothing to reap".  Returns -1/ECHILD when we
    //    have no children at all.
    let mut si: nix::libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: `si` is a valid, zeroed out-parameter the kernel fills in; the
    // constants are POSIX.  WNOWAIT leaves the zombie in place for others.
    let ret = unsafe {
        nix::libc::waitid(
            nix::libc::P_ALL,
            0,
            &mut si,
            nix::libc::WEXITED | nix::libc::WNOHANG | nix::libc::WNOWAIT,
        )
    };
    if ret == -1 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(nix::libc::ECHILD) {
            warn!("orphan reaper: waitid(WNOWAIT) failed: {err}");
        }
        return;
    }
    if si.si_signo == 0 {
        return; // children (or none), but no zombie: nothing to do
    }

    // 2. Our children first, registry second (see module docs, rule 1).
    let children = scan_children();

    // 3. Snapshot the pids some unit still expects to wait for.
    let tracked: HashSet<u32> = match shared.try_lock() {
        Ok(guard) => match guard.as_ref() {
            Some(reg) => reg
                .lock()
                .values()
                // A unit in `Stopping` has nobody waiting for the exit
                // status: `monitor_service` breaks out of its loop the
                // moment it sees Stopping (ipc/mod.rs) without reaping, and
                // `stop_service` only polls liveness — it cannot make
                // progress while the child is a zombie either, because a
                // zombie still answers kill(pid, 0).  So its pid is fair
                // game for us.
                .filter(|inst| inst.state != ServiceState::Stopping)
                .filter_map(|inst| inst.main_pid)
                .collect(),
            // Registry not installed yet: no service has been spawned.
            None => return,
        },
        // Held for a moment elsewhere; the next tick retries.
        Err(_) => return,
    };

    // 4. Reap the untracked ones.
    for (pid, comm) in children {
        if tracked.contains(&pid) {
            continue; // a monitor/oneshot/stop path owns this exit status
        }
        let mut status: i32 = 0;
        // SAFETY: `status` is a valid out-parameter for waitpid.
        let ret = unsafe { nix::libc::waitpid(pid as i32, &mut status, nix::libc::WNOHANG) };
        match ret {
            // Still running: leave it alone, collect it when it dies.
            0 => {}
            // Reaped (it was our zombie child — waitpid(WNOHANG) only ever
            // returns the pid when it collected an exited child).
            p if p == pid as i32 => {
                info!("Reaped orphaned child PID {pid} ({comm}): {}", describe(pid, status));
            }
            // -1: raced with another waiter (ECHILD) or a transient error.
            _ => {}
        }
    }
}

/// Human-readable exit description for a raw `waitpid` status word.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn describe(pid: u32, status: i32) -> String {
    use nix::sys::wait::WaitStatus;
    let pid = nix::unistd::Pid::from_raw(pid as i32);
    match WaitStatus::from_raw(pid, status) {
        Ok(WaitStatus::Exited(_, code)) => sysa::l10n::fmt(
            sysa::l10n::t_("exit code {code}"),
            &[("code", &code.to_string())],
        ),
        Ok(WaitStatus::Signaled(_, sig, _)) => sysa::l10n::fmt(
            sysa::l10n::t_("killed by {sig}"),
            &[("sig", &sig.to_string())],
        ),
        Ok(other) => format!("{other:?}"),
        Err(_) => sysa::l10n::fmt(
            sysa::l10n::t_("raw status {status}"),
            &[("status", &status.to_string())],
        ),
    }
}

/// PIDs (with `comm`) whose parent is us, read straight from `/proc`.
///
/// `/proc/<pid>/stat` is `pid (comm) state ppid ...`; `comm` may itself
/// contain spaces and parentheses, so it is delimited by the first `(` and
/// the last `)`.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn scan_children() -> Vec<(u32, String)> {
    let me = std::process::id();
    let mut out = Vec::new();

    let Ok(dir) = std::fs::read_dir("/proc") else {
        warn!("orphan reaper: cannot read /proc");
        return out;
    };
    for entry in dir.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue; // vanished (or not ours) between readdir and open
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
                out.push((pid, comm.to_string()));
            }
        }
    }
    out
}

/// Split `/proc/<pid>/stat` into `(comm, remainder)` around the parenthesised
/// command name.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn split_stat(stat: &str) -> Option<(&str, &str)> {
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    if close < open {
        return None;
    }
    Some((&stat[open + 1..close], stat[close + 1..].trim_start()))
}

#[cfg(all(test, any(target_os = "linux", target_os = "android")))]
mod tests {
    use super::*;

    #[test]
    fn split_stat_plain() {
        let (comm, rest) = split_stat("1234 (plymountd) S 1000 1000 1000 0 -1").unwrap();
        assert_eq!(comm, "plymountd");
        assert!(rest.starts_with("S 1000"));
    }

    #[test]
    fn split_stat_comm_with_parens_and_space() {
        // comm may contain spaces and parentheses; the delimiter is the
        // *last* ')'.
        let (comm, rest) = split_stat("7 (a b (c)) R 1 1 1 0 -1").unwrap();
        assert_eq!(comm, "a b (c)");
        assert!(rest.starts_with("R 1"));
    }

    #[test]
    fn split_stat_rejects_garbage() {
        assert!(split_stat("not a stat file").is_none());
        assert!(split_stat("1 ) 2 ( 3").is_none());
    }

    /// A child that dies and is *not* waited for must be picked up by a
    /// sweep; a child a unit claims (tracked) must be left alone.
    #[tokio::test]
    async fn reaps_untracked_zombie_and_skips_tracked() {
        use nix::sys::signal::kill;
        use nix::unistd::Pid;

        // Our own registry, as `main` builds it.
        let shared = crate::ipc::shared_registry();
        *shared.lock().await = Some(crate::state::new_registry());

        // Child A: untracked (no unit owns it) → must be reaped.
        let a = spawn_exiter(0);
        // Child B: registered as some unit's main_pid → must survive.
        let b = spawn_exiter(0);
        // Child C: registered but the unit is already `Stopping` → its
        // monitor has left the scene, nobody waits → must be reaped.
        let c = spawn_exiter(0);
        {
            let guard = shared.lock().await;
            let reg = guard.as_ref().unwrap();
            let mut reg = reg.lock();
            let inst = reg.entry("test.service".to_string()).or_default();
            inst.state = crate::state::ServiceState::Running;
            inst.main_pid = Some(b);
            let inst = reg.entry("stopping.service".to_string()).or_default();
            inst.state = crate::state::ServiceState::Stopping;
            inst.main_pid = Some(c);
        }

        // Wait until all are zombies (exit is async).
        for _ in 0..100 {
            if is_zombie(a) && is_zombie(b) && is_zombie(c) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(is_zombie(a), "child A should have exited by now");

        sweep(&shared).await;

        // A is gone: kill() on a reaped pid fails (ESRCH).
        assert!(
            kill(Pid::from_raw(a as i32), None).is_err(),
            "untracked zombie A must be reaped"
        );
        assert!(
            kill(Pid::from_raw(c as i32), None).is_err(),
            "Stopping-state zombie C must be reaped (no waiter exists)"
        );
        // B is still ours, still a zombie: a unit waits for its status.
        assert!(
            kill(Pid::from_raw(b as i32), None).is_ok(),
            "tracked zombie B must NOT be reaped"
        );
        assert!(is_zombie(b), "tracked zombie B must still be a zombie");

        // Release B: clear the tracking pid so the sweep may collect it.
        // Scoped so the guards are gone before the next await.
        {
            let guard = shared.lock().await;
            let mut registry = guard.as_ref().unwrap().lock();
            registry.get_mut("test.service").unwrap().main_pid = None;
        }
        sweep(&shared).await;
        assert!(
            kill(Pid::from_raw(b as i32), None).is_err(),
            "B must be reaped once untracked"
        );
    }

    /// Fork a child that immediately exits and stays a zombie (we never
    /// wait on it directly).
    fn spawn_exiter(code: i32) -> u32 {
        let pid = unsafe { nix::libc::fork() };
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            // Child: drop straight out of the runtime's view.
            unsafe { nix::libc::_exit(code) };
        }
        pid as u32
    }

    fn is_zombie(pid: u32) -> bool {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        let Some((_, rest)) = split_stat(&stat) else {
            return false;
        };
        rest.split_whitespace().next() == Some("Z")
    }
}
