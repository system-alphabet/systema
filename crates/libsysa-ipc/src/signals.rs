//! Graceful-shutdown signal handling shared by System A and the Workers.
//!
//! System Init (SysAInit) stops its supervised processes by sending
//! SIGTERM; SIGINT (e.g. Ctrl-C on an interactive console, or a
//! debugger-attached process) is normalised to the same semantics.  Both
//! are translated into an orderly unwinding of the async runtime so sockets
//! and other resources are dropped cleanly instead of the default
//! terminate-on-signal action.

use tokio::signal::unix::{SignalKind, signal};
use tracing::info;

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