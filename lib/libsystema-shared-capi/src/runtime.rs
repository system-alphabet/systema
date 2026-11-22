//! The library-owned runtime that drives synchronous C calls.
//!
//! A single dedicated thread owns a `current_thread` tokio runtime.  Every
//! blocking entry point spawns its future onto that runtime and waits on a
//! standard channel, so callers may be arbitrary C threads and never need to
//! know about tokio.  Calls made from inside the runtime thread itself (for
//! example from a callback that runs there) are rejected instead of
//! deadlocking.

use std::cell::Cell;
use std::future::Future;
use std::sync::{mpsc, OnceLock};
use std::time::Duration;

use tokio::runtime::{Builder, Handle};

use crate::status::SysaStatus;

/// Default budget for a synchronous request when the caller has none.
pub(crate) const DEFAULT_WAIT: Duration = Duration::from_secs(30);

thread_local! {
    static IN_LIBRARY_RUNTIME: Cell<bool> = const { Cell::new(false) };
    static CALLBACK_DEPTH: Cell<usize> = const { Cell::new(0) };
}

struct ClientRuntime {
    handle: Handle,
}

static CLIENT: OnceLock<Result<ClientRuntime, String>> = OnceLock::new();

/// Mark the current thread as one that drives library futures.
pub(crate) fn mark_runtime_thread() {
    IN_LIBRARY_RUNTIME.with(|flag| flag.set(true));
}

/// Whether the current thread is a library runtime thread.
pub(crate) fn in_library_runtime() -> bool {
    IN_LIBRARY_RUNTIME.with(Cell::get)
}

/// Whether the current thread is running a C controller callback.
pub(crate) fn in_callback() -> bool {
    CALLBACK_DEPTH.with(|depth| depth.get() > 0)
}

/// Restores the callback depth when dropped.
pub(crate) struct CallbackGuard;

impl CallbackGuard {
    pub(crate) fn enter() -> Self {
        CALLBACK_DEPTH.with(|depth| depth.set(depth.get() + 1));
        CallbackGuard
    }
}

impl Drop for CallbackGuard {
    fn drop(&mut self) {
        CALLBACK_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

fn client() -> Result<&'static ClientRuntime, String> {
    match CLIENT.get_or_init(spawn_client) {
        Ok(runtime) => Ok(runtime),
        Err(message) => Err(message.clone()),
    }
}

fn spawn_client() -> Result<ClientRuntime, String> {
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("systema-shared-capi".to_string())
        .spawn(move || {
            let runtime = match Builder::new_current_thread().enable_all().build() {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = ready_tx.send(Err(error.to_string()));
                    return;
                }
            };
            let handle = runtime.handle().clone();
            let _ = ready_tx.send(Ok(handle));
            mark_runtime_thread();
            runtime.block_on(std::future::pending::<()>());
        })
        .map_err(|error| error.to_string())?;

    match ready_rx.recv() {
        Ok(Ok(handle)) => Ok(ClientRuntime { handle }),
        Ok(Err(message)) => Err(message),
        Err(error) => Err(error.to_string()),
    }
}

/// Drive `future` on the client runtime and wait for its result.
pub(crate) fn block_on<F, T>(future: F, wait: Duration) -> Result<T, SysaStatus>
where
    F: Future<Output = anyhow::Result<T>> + Send + 'static,
    T: Send + 'static,
{
    if in_library_runtime() || in_callback() {
        return Err(crate::error::fail(
            SysaStatus::Reentrant,
            "a blocking call was made from a library runtime thread or a callback",
        ));
    }
    let client = match client() {
        Ok(client) => client,
        Err(message) => {
            return Err(crate::error::fail(
                SysaStatus::Internal,
                format!("client runtime unavailable: {message}"),
            ))
        }
    };

    let (result_tx, result_rx) = mpsc::sync_channel(1);
    client.handle.spawn(async move {
        let _ = result_tx.send(future.await);
    });

    match result_rx.recv_timeout(wait) {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(crate::error::report(error)),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(crate::error::fail(
            SysaStatus::Timeout,
            "the request timed out",
        )),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(crate::error::fail(
            SysaStatus::Internal,
            "the client runtime stopped",
        )),
    }
}
