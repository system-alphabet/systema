//! Process supervision for `systema-sysu`.
//!
//! The user-domain counterpart of SysAInit's three-phase recipe
//! (`systema-sysi/src/supervise.rs`), trimmed to the P1 worker subset:
//!
//! 1. Bind the notify listener (`<notify-dir>/init.sock`) **before**
//!    spawning anything, so no early allocator event is lost.
//! 2. Spawn System A and wait for `MANAGER_READY` on the notify channel.
//! 3. Spawn the workers **serially**: System S (`system-s-1`), then
//!    System T (`system-t-1`), then System W (`system-w-1`).  W's ready
//!    event is broadcast by System A when the bridge's control
//!    `manager.hello` arrives — D-Bus availability never gates startup.
//! 4. Control phase: `manager.daemon_reload` (System A spawns System F to
//!    discover and commit unit files from the *user* unit path), then
//!    `manager.list_units` + `manager.start_units`.  `register_power_units`
//!    is deliberately absent: power transitions are system-domain actions.
//! 5. Steady state: reap children, forward signals, log notify events.
//!
//! All sockets live under the per-user runtime dir (see [`crate::envs`]),
//! so this instance never touches the system domain's `/run/systema`.

use std::collections::HashMap;
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
    DaemonReloadRequest, DaemonReloadResult, ListUnitsRequest, ListUnitsResult,
    ManagerHelloRequest, StartUnitsRequest, StartUnitsResult,
};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::workers::ResolvedProcess;

/// The notify directory of this instance (`SYSTEMA_NOTIFY_DIR`, resolved
/// by [`sysa::paths`] from the launcher's environment matrix).
fn notify_dir() -> String {
    sysa::paths::instance().notify_dir.clone()
}

/// This instance's notify listener, `<notify-dir>/init.sock`.
///
/// Receiving runs on a dedicated thread that feeds the channel drained by
/// [`WaitCtx`], so every readiness wait still sees events that arrive
/// before it starts polling.
///
/// Every exit path calls [`NotifyListener::shutdown`] so the socket FD —
/// not just its path — is released before the instance goes away.
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
    /// Idempotent, so shutdown paths do not have to track whether it
    /// already ran.
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
/// is dropped (one call per connection, like `systemctl`).
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

    // IPC version exchange precedes hello: the allocator pins one wire
    // version; a mismatch gets an "incompatible" verdict and an immediate
    // close, surfaced here as an ordinary caller error.
    let ipc_version = sysa::ipc::exchange_version(&mut framed, "system-sysu")
        .await
        .map_err(|e| {
            anyhow::anyhow!(sysa::l10n::fmt(
                sysa::l10n::t_("IPC version exchange with System A failed: {e}"),
                &[("e", &e.to_string())]
            ))
        })?;
    debug!("IPC version {ipc_version} negotiated with System A");

    // Handshake: the control socket closes the session if the first
    // envelope is anything but `manager.hello`.
    let hello_env = sysa::ipc::make_envelope(
        0,
        "system-sysu",
        "system-a",
        "manager.hello",
        ManagerHelloRequest {
            flavor: "sysu".to_string(),
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

    let req = sysa::ipc::make_envelope(request_id, "system-sysu", "system-a", method, req)?;
    sysa::ipc::send_envelope(&mut framed, &req).await?;
    loop {
        let reply = sysa::ipc::recv_envelope(&mut framed)
            .await?
            .ok_or_else(|| anyhow::anyhow!(sysa::l10n::t_("System A closed the connection")))?;
        if reply.method == format!("{method}.result") {
            let decoded = Res::decode(reply.payload.as_slice())?;
            // One-shot session: FIN before dropping so the server is not left
            // with queued events at close (which it would see as ECONNRESET).
            sysa::ipc::finish_control_session(&mut framed, sysa::ipc::CONTROL_DRAIN_TIMEOUT).await;
            return Ok(decoded);
        }
        // Otherwise it is a pushed event envelope; keep waiting.
    }
}

/// A supervised child process.  Every process in the user domain is
/// long-running: its unexpected exit terminates the instance.
struct Spawned {
    name: &'static str,
    worker_id: Option<&'static str>,
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
                        sysa::l10n::t_("systema-sysu reaper error: {e}"),
                        &[("e", &e.to_string())]
                    )
                );
                thread::sleep(Duration::from_millis(100));
            }
        }
    })
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
/// The instance is expected to outlive the terminal that launched it
/// (stop happens via `--logout` / SIGTERM), so a hangup from a closing
/// session must not kill it.  A real handler (not `SIG_IGN`) is used so
/// that exec resets it to the default disposition for spawned children,
/// which must still react to terminal hangup normally.  Only `write(2)` is
/// called here because it is the one async-signal-safe way to emit output
/// from a signal handler.
extern "C" fn handle_sighup(_sig: i32) {
    let msg = b"systema-sysu: SIGHUP received; ignoring\n";
    let _ = unsafe { nix::libc::write(nix::libc::STDERR_FILENO, msg.as_ptr().cast(), msg.len()) };
}

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

/// Handle one reaped child.  Returns `Some(code)` when the instance must
/// exit now (a supervised process died), `None` to keep going.
fn handle_reaped(procs: &[Spawned], pid: i32, status: &WaitStatus) -> Option<i32> {
    match procs.iter().find(|p| p.pid == pid) {
        Some(p) => {
            let code = status_code(status);
            error!(
                "Supervised process {} (pid={pid}, worker_id={:?}) exited: {status:?}; aborting with code {code}",
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

/// Signal every child with `SIGTERM`, wait `grace`, then SIGKILL the rest
/// and exit with `code`.
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
    log_target: &sysa::logging::LogTarget,
) -> Option<i32> {
    let mut cmd = std::process::Command::new(&rp.path);
    if debug {
        cmd.arg("--debug");
    } else {
        cmd.arg("--log-level").arg(log_level);
    }
    cmd.args(rp.spec.args);

    // The bus domain is the launcher's knowledge, not the workers'
    // (plan 备注 11): sysw serves systemd1 on — and syss polls `Type=dbus`
    // names on — *this instance's* bus, so point both at it explicitly.
    // The system domain never runs through here and never passes the flag,
    // where both keep zbus's system-bus default.
    if matches!(rp.spec.name, "sysw" | "syss") {
        cmd.arg("--bus-address")
            .arg(crate::envs::session_bus_address());
    }

    // Workers log like the launcher does — stderr plus their own file,
    // which they open themselves.  Point it at the same directory as ours
    // (argv beats any inherited SYSTEMA_LOG_FILE); "-" propagates a
    // stderr-only parent.
    cmd.arg("--log-file")
        .arg(log_target.child_file(rp.spec.binary));

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
/// the instance must exit with `code` (signal → 0, dead supervised
/// process → its code, timeout → 1).  Every `Ok(Some(_))` return has
/// already run [`shutdown`], so callers exit as they are.
async fn wait_ready(
    ctx: &mut WaitCtx,
    procs: &[Spawned],
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

/// Control phase: `daemon_reload` to discover unit files, then start every
/// enabled unit plus `default.target`.
///
/// Unlike SysI this phase **does not** call `manager.register_power_units`
/// (reboot/halt are system-domain actions with no user-domain entry) and
/// does not treat per-unit start failures as fatal: a user session may
/// legitimately have no `default.target` yet, and the instance must stay
/// up to load units the user adds later.  RPC-level failures (connect /
/// handshake) still abort the whole instance.
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
    let mut exchange = Box::pin(async move {
        // Trigger daemon-reload: System A spawns System F which discovers
        // and commits all unit files from the *user* unit path.
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

        // One request per connection: each `manager_call` drops its control
        // session after the reply, like `systemctl`'s one-call-per-connection
        // model.
        let list = manager_call::<ListUnitsRequest, ListUnitsResult>(
            &sock_path,
            1,
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
            2,
            "manager.start_units",
            StartUnitsRequest { names },
        )
        .await?;
        if !start.success {
            // Enqueue-level failures only (e.g. no default.target in this
            // session yet): report them, keep the instance running.
            for result in &start.results {
                if !result.success {
                    warn!(
                        "Control phase: cannot start '{}': {}",
                        result.name, result.message
                    );
                }
            }
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

/// Create the processes (phased) and supervise them until shutdown.
pub async fn run(
    resolved: &[ResolvedProcess],
    debug: bool,
    log_level: &str,
    grace: Duration,
    ready_timeout: Duration,
    log_target: &sysa::logging::LogTarget,
) -> Result<i32> {
    // Partition the resolved set: System A first, then the workers.
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
    let Some(mut notify) = NotifyListener::bind(&notify_dir(), notify_tx) else {
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

    // --- Phase 1: System A, then wait for MANAGER_READY. ---
    let code = if let Some(rp) = allocator_specs.first() {
        if let Some(code) = spawn_process(rp, &mut procs, debug, log_level, log_target) {
            Some(shutdown(&procs, code, grace).await)
        } else {
            wait_ready(
                &mut ctx,
                &procs,
                grace,
                ready_timeout,
                // `what` is interpolated only into `info!`/`error!` templates,
                // which are not translated — keep it as a plain label.
                "System Allocator (user instance)",
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
        if let Some(code) = spawn_process(rp, &mut procs, debug, log_level, log_target) {
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
        if let Some(code) = wait_ready(&mut ctx, &procs, grace, ready_timeout, &what, move |kv| {
            kv.get("WORKER_READY") == Some(&expected_id)
        })
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
                for (key, value) in &kv {
                    info!("notify: {key}={value}");
                }
                // Power units are never registered in this domain, so this
                // should not occur; if it ever does, refuse it loudly —
                // reboot/halt are not a user instance's business.
                if let Some(action) = kv.get("POWER") {
                    warn!("Ignoring POWER={action}: power transitions are system-domain only");
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

    /// Scratch directory for socket tests; removed first so a leftover from
    /// a previous run cannot mask a stale socket.
    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("systema-sysu-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// The listener must release the socket FD on shutdown (not just unlink
    /// the path), and shutdown must be idempotent.
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
        // which is exactly the FD release the instance's exit depends on.
        assert!(rx.blocking_recv().is_none());
        assert!(!sock.exists());

        // Idempotent: shutdown paths may call it again.
        listener.shutdown();

        let _ = std::fs::remove_dir_all(&dir);
    }
}
