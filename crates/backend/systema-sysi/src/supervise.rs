//! Process supervision for SysAInit.
//!
//! Startup is phase-driven:
//! 1. Bind the notify listener socket (`<notify-dir>/init.sock`) **before**
//!    spawning anything, so no early allocator event is lost.
//! 2. Spawn System A and wait for `MANAGER_READY` on the notify channel.
//! 3. Spawn the workers **serially**: each worker is spawned, then
//!    SysAInit waits for `WORKER_READY=<worker_id>` before spawning the
//!    next one.
//! 4. Control phase: call `daemon_reload` on System A (which spawns
//!    System F to discover and commit unit files), then ask System A
//!    to start the enabled units and `default.target`.
//! 5. Steady state: reap children, forward signals, and log notify events.
//!
//! The Finder (System F) is no longer a supervised process.  System A
//! spawns it on-demand when a daemon-reload is requested (via IPC or D-Bus).

use std::collections::{HashMap, HashSet};
use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::Result;
use nix::errno::Errno;
use nix::sys::signal::{kill, signal, SigHandler, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use sysa::proto::{
    DaemonReloadRequest, DaemonReloadResult, ListUnitsRequest, ListUnitsResult, ManagerHelloRequest,
    RegisterPowerUnitsRequest, RegisterPowerUnitsResult, StartUnitsRequest, StartUnitsResult,
};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::mount_setup;
use crate::power;
use crate::workers::{ProcessKind, ResolvedProcess};

/// Default notify directory: `<runstatedir>/systema/notify` (`/run` by
/// default), overridable with `SYSTEMA_NOTIFY_DIR`.  Derived from the
/// runtime state dir rather than hardcoded.
pub fn default_notify_dir() -> String {
    sysa::paths::instance().notify_dir.clone()
}

/// SysAInit's notify listener, `<notify-dir>/init.sock`.
///
/// Receiving runs on a dedicated thread that feeds the channel drained by
/// [`WaitCtx`], so every early-boot wait still sees events that arrive
/// before it starts polling.
///
/// The socket lives under the runstatedir, which means the runstatedir
/// cannot be unmounted while this FD is open — unlinking `init.sock` is not
/// enough, the descriptor itself has to go.  Every exit path therefore
/// calls [`NotifyListener::shutdown`], and so does the power transition
/// before it attempts the unmount.
struct NotifyListener {
    sock_path: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl NotifyListener {
    /// How often the receive thread wakes to re-check [`Self::stop`].
    ///
    /// A read timeout is what lets the thread notice a shutdown request
    /// without a wakeup datagram, so [`NotifyListener::shutdown`] cannot
    /// block on the join forever.
    const POLL: Duration = Duration::from_millis(100);

    /// Bind `<dir>/init.sock` and start forwarding datagrams to `tx`.
    ///
    /// Returns `None` after logging when the socket cannot be set up, which
    /// the caller treats as fatal — readiness waits depend on this channel.
    fn bind(dir: &str, tx: mpsc::UnboundedSender<String>) -> Option<Self> {
        let sock_path = PathBuf::from(dir).join("init.sock");
        if let Err(e) = std::fs::create_dir_all(dir) {
            error!("Cannot create notify directory {dir}: {e}");
        }
        let _ = std::fs::remove_file(&sock_path);
        let listener = match UnixDatagram::bind(&sock_path) {
            Ok(l) => l,
            Err(e) => {
                error!("Cannot bind notify listener {}: {e}", sock_path.display());
                return None;
            }
        };
        if let Err(e) = listener.set_read_timeout(Some(Self::POLL)) {
            error!(
                "Cannot time out notify listener {}: {e}",
                sock_path.display()
            );
            return None;
        }
        info!("Notify listener bound at {}", sock_path.display());

        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while !stopped.load(Ordering::SeqCst) {
                match listener.recv(&mut buf) {
                    Ok(n) => {
                        // Re-check: a shutdown requested while `recv` was
                        // blocked must not forward a late datagram.
                        if stopped.load(Ordering::SeqCst) {
                            break;
                        }
                        let _ = tx.send(String::from_utf8_lossy(&buf[..n]).into_owned());
                    }
                    // Read timeout: nothing arrived, just re-check `stop`.
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(e) => {
                        warn!("notify listener error: {e}");
                        thread::sleep(Duration::from_millis(100));
                    }
                }
            }
        });

        Some(Self {
            sock_path,
            stop,
            thread: Some(thread),
        })
    }

    /// Stop receiving, release the socket FD and unlink `init.sock`.
    ///
    /// Closing the FD — not unlinking the path — is what makes the
    /// runstatedir unmountable.  Idempotent, so shutdown paths may not
    /// have to track whether it already ran.
    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let _ = std::fs::remove_file(&self.sock_path);
    }
}

/// One request-reply exchange over a fresh control-port connection.
///
/// The control bus requires a `manager.hello` handshake as the first
/// envelope of every session, so each call performs it before sending the
/// real request.  The server also pushes event envelopes (`unit.new`,
/// `unit.changed`, `job.*`, …) onto the same session; those are skipped
/// until the matching `{method}.result` reply arrives, then the connection
/// is dropped.
async fn manager_call<Req, Res>(
    sock_path: &str,
    request_id: u64,
    method: &str,
    req: Req,
) -> Result<Res>
where
    Req: prost::Message,
    Res: prost::Message + Default,
{
    let stream = tokio::net::UnixStream::connect(sock_path).await?;
    let mut framed = sysa::ipc::frame_stream(stream);

    // Handshake: the control socket closes the session if the first
    // envelope is anything but `manager.hello`.
    let hello_env = sysa::ipc::make_envelope(
        0,
        "system-sysi",
        "system-a",
        "manager.hello",
        ManagerHelloRequest {
            flavor: "sysi".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            worker_id: String::new(),
        },
    )?;
    sysa::ipc::send_envelope(&mut framed, &hello_env).await?;
    let hello_reply = sysa::ipc::recv_envelope(&mut framed)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(sysa::l10n::t_(
                "System A closed the connection during hello"
            ))
        })?;
    if hello_reply.method != "manager.hello.result" {
        anyhow::bail!(sysa::l10n::fmt(
            sysa::l10n::t_("unexpected reply '{method}' to manager.hello"),
            &[("method", &(hello_reply.method).to_string())]
        ));
    }

    let req = sysa::ipc::make_envelope(
        request_id,
        "system-sysi",
        "system-a",
        method,
        req,
    )?;
    sysa::ipc::send_envelope(&mut framed, &req).await?;
    loop {
        let reply = sysa::ipc::recv_envelope(&mut framed)
            .await?
            .ok_or_else(|| anyhow::anyhow!(sysa::l10n::t_("System A closed the connection")))?;
        if reply.method == format!("{method}.result") {
            return Ok(Res::decode(reply.payload.as_slice())?);
        }
        // Otherwise it is a pushed event envelope; keep waiting.
    }
}

struct Spawned {
    name: &'static str,
    worker_id: Option<&'static str>,
    one_shot: bool,
    pid: i32,
}

type ReaperEvent = (i32, WaitStatus);

/// Channels shared by the readiness waits and the steady-state loop.
struct WaitCtx {
    notify_rx: mpsc::UnboundedReceiver<String>,
    reaper_rx: mpsc::UnboundedReceiver<ReaperEvent>,
    terminate: tokio::signal::unix::Signal,
    interrupt: tokio::signal::unix::Signal,
}

/// Reap children in a dedicated thread and report exits to `tx`.
///
/// `waitpid(-1)` also reaps orphaned grandchildren when SysAInit runs as
/// PID 1; otherwise they belong to the real init.
fn spawn_reaper(tx: mpsc::UnboundedSender<ReaperEvent>) -> thread::JoinHandle<()> {
    thread::spawn(move || loop {
        match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Exited(pid, code)) => {
                let _ = tx.send((pid.as_raw(), WaitStatus::Exited(pid, code)));
            }
            Ok(WaitStatus::Signaled(pid, sig, core)) => {
                let _ = tx.send((pid.as_raw(), WaitStatus::Signaled(pid, sig, core)));
            }
            Ok(_) => thread::sleep(Duration::from_millis(50)),
            Err(Errno::ECHILD) => thread::sleep(Duration::from_millis(100)),
            Err(e) => {
                eprintln!(
                    "{}",
                    sysa::l10n::fmt(
                        sysa::l10n::t_("SysAInit reaper error: {e}"),
                        &[("e", &e.to_string())]
                    )
                );
                thread::sleep(Duration::from_millis(100));
            }
        }
    })
}

/// How long to wait for a SIGKILLed child to actually be reaped.
///
/// SIGKILL is immediate for a runnable process; the bound only exists so a
/// process wedged in uninterruptible sleep cannot stall the shutdown
/// forever.
const KILL_CONFIRM_GRACE: Duration = Duration::from_secs(1);

/// Drain `rx` until every pid in `want` has been reaped, or `deadline`.
///
/// Reaps for pids outside `want` are still removed from `pending`, so one
/// phase waiting on its own group cannot swallow another group's exit
/// event: a later phase recognises an already-reaped pid by probing it
/// with `kill(2)` rather than by remembering it.
async fn wait_for_reap(
    rx: &mut mpsc::UnboundedReceiver<ReaperEvent>,
    pending: &mut HashSet<i32>,
    want: &HashMap<i32, String>,
    deadline: tokio::time::Instant,
) {
    while tokio::time::Instant::now() < deadline && want.keys().any(|pid| pending.contains(pid)) {
        tokio::select! {
            Some((pid, _status)) = rx.recv() => { pending.remove(&pid); }
            _ = tokio::time::sleep_until(deadline) => {}
        }
    }
}

/// SIGKILL whatever in `want` is still running, then wait for those reaps.
///
/// Confirming the reaps is the point: unmounting the runstatedir straight
/// after SIGKILL races a process that has not finished tearing down, and
/// its open files would keep the mount busy.
async fn kill_stragglers(
    rx: &mut mpsc::UnboundedReceiver<ReaperEvent>,
    pending: &mut HashSet<i32>,
    want: &HashMap<i32, String>,
) {
    let stragglers: Vec<i32> = want
        .keys()
        .filter(|pid| pending.contains(pid))
        .copied()
        .collect();
    if stragglers.is_empty() {
        return;
    }
    for pid in &stragglers {
        warn!("{} did not exit in time; SIGKILL", want[pid]);
        let _ = kill(Pid::from_raw(*pid), Signal::SIGKILL);
    }
    let deadline = tokio::time::Instant::now() + KILL_CONFIRM_GRACE;
    wait_for_reap(rx, pending, want, deadline).await;
    for pid in &stragglers {
        if pending.contains(pid) {
            error!(
                "{} survived SIGKILL; it may keep the runstatedir busy",
                want[pid]
            );
        }
    }
}

/// Map a reaped status to an exit code: pass through normal exits, signals
/// map to `1`.
fn status_code(status: &WaitStatus) -> i32 {
    match status {
        WaitStatus::Exited(_, code) => *code,
        _ => 1,
    }
}

/// Async-signal-safe SIGHUP handler body: log one line and keep running.
///
/// A real handler (not `SIG_IGN`) is used so that exec resets it to the
/// default disposition for spawned children, which must still react to
/// terminal hangup normally.  Only `write(2)` is called here because it is
/// the one async-signal-safe way to emit output from a signal handler.
extern "C" fn handle_sighup(_sig: i32) {
    let msg = b"systema-sysi: SIGHUP received; ignoring\n";
    let _ = unsafe {
        nix::libc::write(
            nix::libc::STDERR_FILENO,
            msg.as_ptr().cast(),
            msg.len(),
        )
    };
}

/// Make SIGHUP non-fatal: a getty taking over the boot console
/// (`autovt@ttyS0` doing `TIOCSCTTY`) hangs up init's session and delivers
/// SIGHUP, which must not kill PID 1.
fn install_sighup_handler() -> Result<()> {
    // SAFETY: `handle_sighup` is a plain `extern "C"` function calling only
    // async-signal-safe `write(2)`.
    unsafe { signal(Signal::SIGHUP, SigHandler::Handler(handle_sighup)) }.map_err(|e| {
        anyhow::anyhow!(sysa::l10n::fmt(
            sysa::l10n::t_("cannot install SIGHUP handler: {e}"),
            &[("e", &e.to_string())]
        ))
    })?;
    Ok(())
}

/// Handle one reaped child.  Returns `Some(code)` when SysAInit must exit
/// now (a long-running process died), `None` to keep going.
fn handle_reaped(procs: &[Spawned], pid: i32, status: &WaitStatus) -> Option<i32> {
    match procs.iter().find(|p| p.pid == pid) {
        Some(p) if p.one_shot => {
            info!("One-shot {} (pid={pid}) exited: {status:?}", p.name);
            None
        }
        Some(p) => {
            let code = status_code(status);
            error!(
                "Long-running process {} (pid={pid}, worker_id={:?}) exited: {status:?}; aborting with code {code}",
                p.name, p.worker_id
            );
            Some(code)
        }
        None => {
            debug!("Reaped unknown pid {pid}: {status:?}");
            None
        }
    }
}

/// Signal every child with `sig`, wait `grace`, then SIGKILL the rest and
/// exit with `code`.
async fn shutdown(procs: &[Spawned], code: i32, grace: Duration) -> i32 {
    info!(
        "Forwarding SIGTERM to {count} process(es), exiting with code {code}",
        count = procs.len()
    );
    for p in procs {
        let _ = kill(Pid::from_raw(p.pid), Signal::SIGTERM);
    }
    tokio::time::sleep(grace).await;
    for p in procs {
        let _ = kill(Pid::from_raw(p.pid), Signal::SIGKILL);
    }
    code
}

/// Ordered graceful shutdown driven by a `POWER=` notify datagram.
///
/// Consumes the datagram System A sends when a `.power` unit starts.  The
/// sequence mirrors PID 1 semantics:
///
/// 1. SIGTERM every supervised worker and wait (up to `grace`) for each to
///    be reaped, SIGKILLing stragglers.  That SIGTERM is what makes each
///    worker perform the `worker.exit` handshake — System A closes the
///    connection, and only then does the worker exit — so once this step
///    completes every worker is disconnected from System A.
/// 2. SIGTERM System A last, so the allocator keeps reporting state while
///    the system is draining, and wait for it as well.
/// 3. Release SysAInit's own references to the runstatedir — the notify
///    listener socket lives under it and would otherwise keep the mount
///    busy.
/// 4. Unmount the runstatedir (best-effort; failure is logged but does not
///    abort the transition).
/// 5. Execute the transition **in-process** (System Init manages the
///    `power` unit type directly).  Terminal transitions never return from
///    this call.  When `--powerctl=never`, the transition is skipped and
///    System Init exits cleanly instead.
///
/// Every step that can leave a process behind waits for its reap before the
/// next one runs, because step 4 cannot succeed while anything still holds
/// the mount open.
async fn power_down(
    procs: &[Spawned],
    grace: Duration,
    action: &str,
    reaper_rx: &mut mpsc::UnboundedReceiver<ReaperEvent>,
    power_ctl: power::PowerCtl,
    notify: &mut NotifyListener,
) -> Result<i32> {
    let power_action = power::PowerAction::from_unit_name(action).ok_or_else(|| {
        anyhow::anyhow!(sysa::l10n::fmt(
            sysa::l10n::t_("System Init received an unknown power action: {action}"),
            &[("action", &format!("{:?}", action))]
        ))
    })?;

    // Phase 1: stop every supervised worker (everything except System A) and
    // wait for them all to exit.  A worker now leaves only *after* the
    // `worker.exit` handshake (System A has closed its connection), so
    // "exited" implies "disconnected from System A" — the precondition for
    // touching System A in phase 2.
    let workers: Vec<&Spawned> = procs.iter().filter(|p| p.name != "sysa").collect();
    let want: HashMap<i32, String> = workers
        .iter()
        .map(|p| (p.pid, format!("Worker {}", p.name)))
        .collect();
    let mut pending: HashSet<i32> = HashSet::new();
    let deadline = tokio::time::Instant::now() + grace;
    info!(
        "Power transition '{power_action}': stopping {} worker(s) gracefully",
        workers.len()
    );
    for p in &workers {
        // ESRCH: the worker died before we got here — its connection to
        // System A died with it, so treat it as already disconnected instead
        // of spending the whole grace period waiting for a reap event that
        // will never come.
        match kill(Pid::from_raw(p.pid), Signal::SIGTERM) {
            Err(Errno::ESRCH) => {
                info!(
                    "Worker {} (pid={}) already exited; already disconnected",
                    p.name, p.pid
                );
            }
            _ => {
                pending.insert(p.pid);
            }
        }
    }
    wait_for_reap(reaper_rx, &mut pending, &want, deadline).await;
    kill_stragglers(reaper_rx, &mut pending, &want).await;

    // Phase 2: stop System A last — only now that every worker has
    // disconnected (phase 1 waits for each worker to exit, and a worker
    // exits after System A closed its `worker.exit` handshake).
    let allocator: Vec<&Spawned> = procs.iter().filter(|p| p.name == "sysa").collect();
    let want: HashMap<i32, String> = allocator
        .iter()
        .map(|p| (p.pid, "System A".to_string()))
        .collect();
    // A fresh set probed with kill(2) rather than carried over from phase 1:
    // an exit event drained while waiting for the workers would otherwise be
    // lost and System A would look alive until the grace period expired.
    let mut pending: HashSet<i32> = HashSet::new();
    let deadline = tokio::time::Instant::now() + grace;
    info!("Power transition '{power_action}': stopping System A gracefully");
    for p in &allocator {
        match kill(Pid::from_raw(p.pid), Signal::SIGTERM) {
            Err(Errno::ESRCH) => {
                debug!("System A (pid={}) already exited", p.pid);
            }
            _ => {
                pending.insert(p.pid);
            }
        }
    }
    wait_for_reap(reaper_rx, &mut pending, &want, deadline).await;
    kill_stragglers(reaper_rx, &mut pending, &want).await;

    // Phase 3: release SysAInit's own references to the runstatedir.  The
    // notify listener socket lives under it, and unlinking the path would not
    // free the mount — the FD itself has to be closed first.
    notify.shutdown();

    // Phase 4: unmount the runstatedir (best-effort).
    let runstatedir = sysa::paths::instance().runstatedir;
    info!("Power transition '{power_action}': unmounting {runstatedir}");
    mount_setup::umount_runstatedir(runstatedir);

    // Phase 5: execute the transition or exit cleanly.
    if power_ctl.enabled(std::process::id() == 1) {
        info!(
            "Power transition '{power_action}': executing in-process (System Init)"
        );
        power::execute_action(power_action, power_ctl)?;
        // Terminal transitions never return.  Reaching this point means a
        // resumable transition (suspend/hibernate) completed and the machine
        // came back; the supervisors are already gone, so the best we can do
        // is log prominently and exit.
        warn!(
            "Power transition '{power_action}' returned after the drain; supervisors have exited (reboot the machine manually)"
        );
        Ok(0)
    } else {
        info!(
            "Power transition '{power_action}': power control disabled (--powerctl={power_ctl}); exiting cleanly"
        );
        Ok(0)
    }
}

/// Parse a notify datagram into its `key=value` lines.
fn parse_notify(body: &str) -> HashMap<String, String> {
    let mut kv = HashMap::new();
    for line in body.lines() {
        let mut it = line.splitn(2, '=');
        if let (Some(key), Some(value)) = (it.next(), it.next()) {
            kv.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    kv
}

/// Spawn one resolved process, recording it in `procs`.
///
/// On spawn failure this returns `Some(code)` so the caller can shut down;
/// `None` means the spawn succeeded.
fn spawn_process(
    rp: &ResolvedProcess,
    procs: &mut Vec<Spawned>,
    debug: bool,
    log_level: &str,
    log_dir: &std::path::Path,
    extra_flags: &HashMap<&'static str, Vec<String>>,
) -> Option<i32> {
    let mut cmd = std::process::Command::new(&rp.path);
    if debug {
        cmd.arg("--debug");
    } else {
        cmd.arg("--log-level").arg(log_level);
    }
    cmd.args(rp.spec.args);
    if let Some(flags) = extra_flags.get(rp.spec.name) {
        cmd.args(flags);
    }

    // Workers resolve SYSTEMA_LOG_DIR themselves ("-" means stderr) and
    // write tracing output into <log-dir>/<binary>.log.  SysAInit only
    // redirects the inherited stdio for pre-logging output; with "-" there
    // is nothing to redirect.
    let log_dir_str = log_dir.to_string_lossy().into_owned();
    cmd.env("SYSTEMA_LOG_DIR", &log_dir_str);

    if log_dir_str != "-" {
        let log_path = log_dir.join(format!("{}.log", rp.spec.binary));
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
        {
            Ok(file) => {
                let stdout = file.try_clone().ok();
                cmd.stdout(std::process::Stdio::from(file));
                cmd.stderr(match stdout {
                    Some(f) => std::process::Stdio::from(f),
                    None => std::process::Stdio::inherit(),
                });
                info!(
                    "Redirecting {} logs to {}",
                    rp.spec.name,
                    log_path.display()
                );
            }
            Err(e) => {
                warn!(
                    "Cannot open log file {} ({}); {} inherits stderr",
                    log_path.display(),
                    e,
                    rp.spec.name
                );
            }
        }
    }

    match cmd.spawn() {
        Ok(child) => {
            let pid = child.id() as i32;
            info!(
                "Spawned {} (pid={pid}, worker_id={:?}) from {}",
                rp.spec.name,
                rp.spec.worker_id,
                rp.path.display()
            );
            procs.push(Spawned {
                name: rp.spec.name,
                worker_id: rp.spec.worker_id,
                one_shot: rp.spec.kind == ProcessKind::OneShot,
                pid,
            });
            None
        }
        Err(e) => {
            error!(
                "Failed to spawn {} ({}): {e}",
                rp.spec.name,
                rp.path.display()
            );
            Some(1)
        }
    }
}

/// Wait until `is_ready` matches an incoming notify event, a child dies, a
/// signal arrives, or `ready_timeout` elapses.
///
/// Returns `Ok(None)` when the predicate matched; `Ok(Some(code))` when
/// SysAInit must exit with `code` (signal → 0, dead long-running child →
/// its code, timeout → 1).
///
/// Every `Ok(Some(_))` return has already run [`shutdown`], so callers exit
/// as they are.  The timeout branch included: children spawned before the
/// deadline would otherwise survive SysAInit's exit and be reparented to
/// init, still looping forever.
async fn wait_ready(
    ctx: &mut WaitCtx,
    procs: &[Spawned],
    bootlog: &mut Vec<String>,
    grace: Duration,
    ready_timeout: Duration,
    what: &str,
    is_ready: impl Fn(&HashMap<String, String>) -> bool,
) -> Result<Option<i32>> {
    let deadline = tokio::time::Instant::now() + ready_timeout;
    loop {
        tokio::select! {
            _ = ctx.terminate.recv() => {
                info!("SIGTERM received; shutting down");
                return Ok(Some(shutdown(procs, 0, grace).await));
            }
            _ = ctx.interrupt.recv() => {
                info!("SIGINT received; shutting down");
                return Ok(Some(shutdown(procs, 0, grace).await));
            }
            Some((pid, status)) = ctx.reaper_rx.recv() => {
                if let Some(code) = handle_reaped(procs, pid, &status) {
                    return Ok(Some(shutdown(procs, code, grace).await));
                }
            }
            Some(body) = ctx.notify_rx.recv() => {
                let kv = parse_notify(&body);
                bootlog.push(body);
                for (key, value) in &kv {
                    info!("notify: {key}={value}");
                }
                if is_ready(&kv) {
                    info!("{what} is ready");
                    return Ok(None);
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                error!(
                    "Timed out waiting for {what} to become ready after {ready_timeout:?}"
                );
                return Ok(Some(shutdown(procs, 1, grace).await));
            }
        }
    }
}

/// Wait for a one-shot child process to exit.
///
/// Returns `Ok(None)` when the process exits successfully (code 0),
/// `Ok(Some(code))` when the caller must shut down (signal, other
/// long-running child death, non-zero exit, or timeout).
/// Phase 3 — the control plane: call daemon-reload to discover unit files,
/// then ask System A to start every enabled unit plus `default.target`.
async fn control_phase(
    ctx: &mut WaitCtx,
    procs: &[Spawned],
    grace: Duration,
    ready_timeout: Duration,
) -> Result<Option<i32>> {
    if !procs.iter().any(|p| p.name == "sysa") {
        warn!("System A is not in the process set; skipping control phase");
        return Ok(None);
    }

    let deadline = tokio::time::Instant::now() + ready_timeout;
    // Control-plane RPCs live on the control socket; the allocator socket is
    // pure workload (worker/finder/staging traffic only).
    let sock_path = sysa::paths::instance().control_socket_path.to_string();
    let mut exchange = Box::pin(async {
        // Trigger daemon-reload: System A spawns System F which discovers
        // and commits all unit files.  This replaces the old one-shot
        // Finder chain that SysAInit used to run directly.
        info!("Control phase: triggering daemon-reload (unit file rescan)");
        let reload = manager_call::<DaemonReloadRequest, DaemonReloadResult>(
            &sock_path,
            0,
            "manager.daemon_reload",
            DaemonReloadRequest {},
        )
        .await?;
        if !reload.success {
            anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_("manager.daemon_reload failed: {message}"),
                &[("message", &(reload.message).to_string())]
            ));
        }
        info!("Control phase: daemon-reload complete");

        // Register the built-in `.power` unit definitions with System A.
        // System Init owns the `power` unit type; System A merges the
        // synthesized definitions idempotently and never needs a `power`
        // worker.  Registration is unconditional — the policy only gates
        // the actual transition in execute_action.
        {
            let units_json = serde_json::to_vec(&power::all_power_definitions())?;
            let register =
                manager_call::<RegisterPowerUnitsRequest, RegisterPowerUnitsResult>(
                    &sock_path,
                    1,
                    "manager.register_power_units",
                    RegisterPowerUnitsRequest { units_json },
                )
                .await?;
            if !register.success {
                anyhow::bail!(sysa::l10n::fmt(
                    sysa::l10n::t_("manager.register_power_units failed: {message}"),
                    &[("message", &(register.message).to_string())]
                ));
            }
            info!(
                "Control phase: registered {created} new, {updated} updated .power unit(s)",
                created = register.created,
                updated = register.updated
            );
        }

        // One request per connection: each `manager_call` drops its control
        // session after the reply, like `systemctl`'s one-call-per-connection
        // model.
        let list = manager_call::<ListUnitsRequest, ListUnitsResult>(
            &sock_path,
            2,
            "manager.list_units",
            ListUnitsRequest { enabled_only: true },
        )
        .await?;
        if !list.success {
            anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_("manager.list_units failed: {message}"),
                &[("message", &(list.message).to_string())]
            ));
        }

        let mut names: Vec<String> = list.units.into_iter().map(|u| u.name).collect();
        if !names.iter().any(|n| n == "default.target") {
            names.push("default.target".to_string());
        }
        info!(
            "Control phase: starting {} unit(s): {:?}",
            names.len(),
            names
        );

        let start = manager_call::<StartUnitsRequest, StartUnitsResult>(
            &sock_path,
            3,
            "manager.start_units",
            StartUnitsRequest { names },
        )
        .await?;
        if !start.success {
            anyhow::bail!(sysa::l10n::t_("manager.start_units failed"));
        }
        Ok(start)
    });

    loop {
        tokio::select! {
            _ = ctx.terminate.recv() => {
                info!("SIGTERM received during control phase; shutting down");
                return Ok(Some(shutdown(procs, 0, grace).await));
            }
            _ = ctx.interrupt.recv() => {
                info!("SIGINT received during control phase; shutting down");
                return Ok(Some(shutdown(procs, 0, grace).await));
            }
            Some((pid, status)) = ctx.reaper_rx.recv() => {
                if let Some(code) = handle_reaped(procs, pid, &status) {
                    return Ok(Some(shutdown(procs, code, grace).await));
                }
            }
            r = exchange.as_mut() => {
                return match r {
                    Ok(start) => {
                        for result in &start.results {
                            if result.success {
                                info!(
                                    "Control phase: enqueued start for '{}' ({})",
                                    result.name, result.message
                                );
                            } else {
                                error!(
                                    "Control phase: cannot start '{}': {}",
                                    result.name, result.message
                                );
                            }
                        }
                        Ok(None)
                    }
                    Err(e) => {
                        error!("Control phase failed: {e:#}");
                        Ok(Some(shutdown(procs, 1, grace).await))
                    }
                };
            }
            _ = tokio::time::sleep_until(deadline) => {
                error!("Control phase timed out after {ready_timeout:?}");
                return Ok(Some(shutdown(procs, 1, grace).await));
            }
        }
    }
}

/// Spawn all resolved processes (phased) and supervise them until shutdown.
pub async fn run(
    resolved: &[ResolvedProcess],
    debug: bool,
    log_level: &str,
    grace: Duration,
    ready_timeout: Duration,
    log_dir: &std::path::Path,
    extra_flags: &HashMap<&'static str, Vec<String>>,
    power_ctl: power::PowerCtl,
) -> Result<i32> {
    // Partition the resolved set: the allocator (System A) and the
    // long-running workers.  The Finder (System F) is no longer run as
    // a supervised process; System A spawns it on-demand via daemon-reload.
    let mut allocator_specs: Vec<&ResolvedProcess> = Vec::new();
    let mut workers: Vec<&ResolvedProcess> = Vec::new();
    for rp in resolved {
        if rp.spec.name == "sysa" {
            allocator_specs.push(rp);
        } else {
            workers.push(rp);
        }
    }

    // --- Bind the notify listener BEFORE spawning anything. ---
    let (notify_tx, notify_rx) = mpsc::unbounded_channel::<String>();
    let Some(mut notify) = NotifyListener::bind(&default_notify_dir(), notify_tx) else {
        return Ok(1);
    };

    let (reaper_tx, reaper_rx) = mpsc::unbounded_channel::<ReaperEvent>();
    let reaper = spawn_reaper(reaper_tx);

    let terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

    install_sighup_handler()?;

    let mut ctx = WaitCtx {
        notify_rx,
        reaper_rx,
        terminate,
        interrupt,
    };

    let mut procs: Vec<Spawned> = Vec::with_capacity(resolved.len());
    let mut bootlog: Vec<String> = Vec::new();

    // --- Phase 1: System A, then wait for MANAGER_READY. ---
    let code = if let Some(rp) = allocator_specs.first() {
        if let Some(code) = spawn_process(rp, &mut procs, debug, log_level, log_dir, extra_flags) {
            Some(shutdown(&procs, code, grace).await)
        } else {
            wait_ready(
                &mut ctx,
                &procs,
                &mut bootlog,
                grace,
                ready_timeout,
                // `what` is interpolated only into `info!`/`error!` templates,
                // which are not translated — keep it as a plain label.
                "System Allocator",
                |kv| kv.get("MANAGER_READY").is_some(),
            )
            .await?
        }
    } else {
        warn!("System A is not in the process set; skipping allocator readiness wait");
        None
    };
    if let Some(code) = code {
        notify.shutdown();
        drop(reaper);
        return Ok(code);
    }

    // --- Phase 2: workers, serially, each gated on WORKER_READY. ---
    for rp in &workers {
        if let Some(code) = spawn_process(rp, &mut procs, debug, log_level, log_dir, extra_flags) {
            let code = shutdown(&procs, code, grace).await;
            notify.shutdown();
            drop(reaper);
            return Ok(code);
        }
        let Some(worker_id) = rp.spec.worker_id else {
            continue;
        };
        let what = format!("worker '{}' ({worker_id})", rp.spec.name);
        let expected_id = worker_id.to_string();
        if let Some(code) = wait_ready(
            &mut ctx,
            &procs,
            &mut bootlog,
            grace,
            ready_timeout,
            &what,
            move |kv| kv.get("WORKER_READY") == Some(&expected_id),
        )
        .await?
        {
            notify.shutdown();
            drop(reaper);
            return Ok(code);
        }
    }

    // --- Phase 3: control plane (daemon-reload + start enabled units). ---
    if let Some(code) = control_phase(&mut ctx, &procs, grace, ready_timeout).await? {
        notify.shutdown();
        drop(reaper);
        return Ok(code);
    }

    // --- Steady state. ---
    let code = loop {
        let mut power_requested: Option<String> = None;
        tokio::select! {
            _ = ctx.terminate.recv() => {
                info!("SIGTERM received; shutting down");
                break shutdown(&procs, 0, grace).await;
            }
            _ = ctx.interrupt.recv() => {
                info!("SIGINT received; shutting down");
                break shutdown(&procs, 0, grace).await;
            }
            Some((pid, status)) = ctx.reaper_rx.recv() => {
                if let Some(code) = handle_reaped(&procs, pid, &status) {
                    break shutdown(&procs, code, grace).await;
                }
            }
            Some(body) = ctx.notify_rx.recv() => {
                let kv = parse_notify(&body);
                bootlog.push(body);
                for (key, value) in &kv {
                    info!("notify: {key}={value}");
                }
                // System A dispatches `.power` unit starts as `POWER=<action>`
                // datagrams.  The ordered shutdown (drain workers, then
                // allocator, then the in-process power transition) runs
                // outside the select, where `ctx.reaper_rx` is free to be
                // borrowed again.
                power_requested = kv.get("POWER").cloned();
            }
        }
        if let Some(action) = power_requested {
            match power_down(
                &procs,
                grace,
                &action,
                &mut ctx.reaper_rx,
                power_ctl,
                &mut notify,
            )
            .await
            {
                Ok(code) => break code,
                Err(e) => {
                    error!("Power transition '{action}' aborted: {e:#}");
                    break 1;
                }
            }
        }
    };

    notify.shutdown();
    drop(reaper);
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::sys::signal::Signal;

    #[test]
    fn status_code_passes_through_normal_exits() {
        let status = WaitStatus::Exited(Pid::from_raw(42), 7);
        assert_eq!(status_code(&status), 7);
    }

    #[test]
    fn status_code_maps_signals_to_one() {
        let status = WaitStatus::Signaled(Pid::from_raw(42), Signal::SIGTERM, false);
        assert_eq!(status_code(&status), 1);
    }

    #[test]
    fn parse_notify_handles_multi_line_events() {
        let kv = parse_notify("UNIT_STARTED=sshd.service\nRESULT=success\n");
        assert_eq!(
            kv.get("UNIT_STARTED").map(String::as_str),
            Some("sshd.service")
        );
        assert_eq!(kv.get("RESULT").map(String::as_str), Some("success"));
    }

    #[test]
    fn parse_notify_ignores_blank_lines() {
        let kv = parse_notify("\n\n");
        assert!(kv.is_empty());
    }

    #[test]
    fn sighup_does_not_kill_the_process() {
        install_sighup_handler().unwrap();
        let pid = Pid::from_raw(std::process::id() as i32);
        kill(pid, Signal::SIGHUP).unwrap();
        std::thread::sleep(Duration::from_millis(200));
    }

    // =========================================================================
    // Notify listener lifecycle
    // =========================================================================

    /// Scratch directory for socket tests; removed first so a leftover from a
    /// previous run cannot mask a stale socket.
    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("systema-sysi-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// The bug this guards against: SysAInit used to unlink `init.sock` and
    /// detach the receive thread, leaving the socket FD open — enough to make
    /// `umount /run` fail with EBUSY until SysAInit itself exited.
    #[test]
    fn notify_listener_shutdown_closes_the_socket() {
        let dir = scratch_dir("notify");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut listener =
            NotifyListener::bind(dir.to_str().expect("utf-8 temp path"), tx).expect("bind");
        let sock = dir.join("init.sock");
        assert!(sock.exists());

        let client = UnixDatagram::unbound().expect("client socket");
        client.send_to(b"MANAGER_READY=1", &sock).expect("send");
        assert_eq!(rx.blocking_recv().as_deref(), Some("MANAGER_READY=1"));

        listener.shutdown();

        // The receive thread has ended, so its end of the channel is closed —
        // which is exactly the FD release the unmount depends on.
        assert!(rx.blocking_recv().is_none());
        assert!(!sock.exists());

        // Idempotent: shutdown paths may call it again.
        listener.shutdown();

        let _ = std::fs::remove_dir_all(&dir);
    }

    // =========================================================================
    // Reap confirmation
    // =========================================================================

    fn want_set(pids: &[i32], label: &str) -> HashMap<i32, String> {
        pids.iter().map(|pid| (*pid, label.to_string())).collect()
    }

    #[tokio::test]
    async fn wait_for_reap_records_exits_it_was_not_asked_to_wait_for() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let want = want_set(&[101], "Worker a");
        let mut pending = HashSet::from([101, 202]);

        // 202 belongs to a later phase.  Draining it here must not swallow
        // it, or that phase would wait out its whole grace period.
        tx.send((202, WaitStatus::Exited(Pid::from_raw(202), 0)))
            .expect("send");
        tx.send((101, WaitStatus::Exited(Pid::from_raw(101), 0)))
            .expect("send");

        wait_for_reap(
            &mut rx,
            &mut pending,
            &want,
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await;

        assert!(!pending.contains(&101));
        assert!(!pending.contains(&202));
    }

    #[tokio::test]
    async fn wait_for_reap_returns_immediately_when_everything_exited() {
        let (_tx, mut rx) = mpsc::unbounded_channel();
        let want = want_set(&[101], "Worker a");
        let mut pending = HashSet::new();

        let started = std::time::Instant::now();
        wait_for_reap(
            &mut rx,
            &mut pending,
            &want,
            tokio::time::Instant::now() + Duration::from_secs(30),
        )
        .await;

        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn wait_for_reap_gives_up_at_the_deadline() {
        let (_tx, mut rx) = mpsc::unbounded_channel();
        let want = want_set(&[101], "Worker a");
        let mut pending = HashSet::from([101]);
        let deadline = tokio::time::Instant::now() + Duration::from_millis(50);

        wait_for_reap(&mut rx, &mut pending, &want, deadline).await;

        assert!(pending.contains(&101));
    }

    #[tokio::test]
    async fn kill_stragglers_does_nothing_when_nothing_is_left() {
        let (_tx, mut rx) = mpsc::unbounded_channel();
        let want = want_set(&[101], "Worker a");
        let mut pending = HashSet::new();

        let started = std::time::Instant::now();
        kill_stragglers(&mut rx, &mut pending, &want).await;

        assert!(pending.is_empty());
        // No SIGKILL was issued, so the confirmation wait must not have run.
        assert!(started.elapsed() < KILL_CONFIRM_GRACE);
    }
}
