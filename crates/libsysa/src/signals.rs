//! Signal handling shared by System A and the Workers.
//!
//! System Init (SysAInit) stops its supervised processes by sending
//! SIGTERM; SIGINT (e.g. Ctrl-C on an interactive console, or a
//! debugger-attached process) is normalised to the same semantics.  Both
//! are translated into an orderly unwinding of the async runtime so sockets
//! and other resources are dropped cleanly instead of the default
//! terminate-on-signal action.
//!
//! The stray-signal guard ([`install_signal_guard`]) protects every
//! component against signals System A has no protocol for: each stray
//! delivery is logged at debug level and then swallowed.  Fatal
//! bug-class signals (SIGSEGV &c.) keep their default action, and SIGPIPE
//! keeps the Rust-runtime default so writes still surface `EPIPE`.

use tokio::signal::unix::{signal, SignalKind};
use tracing::{debug, info};

use crate::worker_ipc::ShutdownHandle;

/// Wait for a graceful-shutdown request and return the signal that arrived.
///
/// Installs both handlers at registration time, so the process behaves the
/// same whether the parent sends SIGTERM (the supervisor norm) or the
/// console sends SIGINT.  Resolves once, with the name of the first signal
/// received, for logging.
pub async fn shutdown_signal() -> &'static str {
    let mut term = signal(SignalKind::terminate()).expect("cannot install SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("cannot install SIGINT handler");
    tokio::select! {
        _ = term.recv() => {
            info!("SIGTERM received; shutting down gracefully");
            "SIGTERM"
        }
        _ = int.recv() => {
            info!("SIGINT received; shutting down gracefully");
            "SIGINT"
        }
    }
}

/// Forward SIGTERM/SIGINT to a [`ShutdownHandle`] in a background task.
///
/// A worker's IPC loop deliberately watches no process signals
/// ([`sysa_ipc::worker_ipc::WorkerIpc::run`] only leaves when asked to), so
/// binaries call this right before `run` to turn the signal into
/// [`ShutdownHandle::shutdown`].  The loop then leaves between iterations
/// instead of the signal killing the process mid-frame.
pub fn forward_shutdown(handle: ShutdownHandle) {
    tokio::spawn(async move {
        let name = shutdown_signal().await;
        handle.shutdown(name);
    });
}

/// Signals System A has no protocol for, in reception order stable enough
/// for logs.  A stray delivery of any of these must never terminate a
/// component: each gets a listener that records the receipt at debug level
/// and then swallows the signal.
///
/// The first observed offender in the wild was the Xorg display-manager
/// handshake: an Xorg started through a chain that left SIGUSR1 ignored
/// `kill(getppid(), SIGUSR1)`s once it is ready to accept connections, and
/// an orphaned server re-parents onto the nearest living subreaper — which
/// is exactly System S (the random father problem, in both boots on
/// Deepin).
const STRAY_SIGNALS: &[(SignalKind, &'static str)] = &[
    (SignalKind::hangup(), "SIGHUP"),
    (SignalKind::quit(), "SIGQUIT"),
    (SignalKind::user_defined1(), "SIGUSR1"),
    (SignalKind::user_defined2(), "SIGUSR2"),
    (SignalKind::alarm(), "SIGALRM"),
    (SignalKind::window_change(), "SIGWINCH"),
    (SignalKind::from_raw(libc::SIGVTALRM), "SIGVTALRM"),
    (SignalKind::from_raw(libc::SIGPROF), "SIGPROF"),
    (SignalKind::from_raw(libc::SIGTSTP), "SIGTSTP"),
    (SignalKind::from_raw(libc::SIGTTIN), "SIGTTIN"),
    (SignalKind::from_raw(libc::SIGTTOU), "SIGTTOU"),
    (SignalKind::from_raw(libc::SIGCONT), "SIGCONT"),
    (SignalKind::from_raw(libc::SIGXCPU), "SIGXCPU"),
    (SignalKind::from_raw(libc::SIGXFSZ), "SIGXFSZ"),
];

/// Install the stray-signal protection for one System A component.
///
/// One listener is spawned on the ambient tokio runtime per stray signal;
/// every receipt is logged at debug level and then ignored.  This must be
/// called early in a binary's `main`, inside the runtime context.
///
/// The process keeps its other semantics untouched: `SIGTERM`/`SIGINT` go
/// through [`shutdown_signal`], `SIGCHLD` stays with the process reaper,
/// fatal bug-class signals (SIGSEGV, SIGABRT, …) keep their default action
/// because swallowing them only hides corruption, and SIGPIPE keeps the
/// Rust-runtime default so writes surface `EPIPE`.
pub fn install_signal_guard() {
    debug!(
        "signal guard: installing {count} stray-signal listeners",
        count = STRAY_SIGNALS.len()
    );
    for (kind, name) in STRAY_SIGNALS {
        tokio::spawn(guard_one(*kind, *name));
    }
}

/// Log-and-ignore loop for one stray signal.
///
/// A registration failure leaves that signal at its default action — log
/// it and keep the process usable on the remaining listeners.
async fn guard_one(kind: SignalKind, name: &'static str) {
    let mut stream = match signal(kind) {
        Ok(stream) => stream,
        Err(e) => {
            debug!("signal guard: cannot install listener for {name}: {e}");
            return;
        }
    };
    while stream.recv().await.is_some() {
        debug!("received {name}; ignored (not a System A signal)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shutdown_signal_resolves_on_sigterm() {
        // Install the handler first, then raise; otherwise a pre-registration
        // delivery would terminate the test binary outright.
        let waiting = tokio::spawn(shutdown_signal());
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        unsafe { libc::raise(libc::SIGTERM) };
        assert_eq!(waiting.await.unwrap(), "SIGTERM");
    }

    #[tokio::test]
    #[ignore = "spurious delivery is possible when other tests in the same binary raise SIGINT"]
    async fn shutdown_signal_resolves_on_sigint() {
        let waiting = tokio::spawn(shutdown_signal());
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        unsafe { libc::raise(libc::SIGINT) };
        assert_eq!(waiting.await.unwrap(), "SIGINT");
    }
}
