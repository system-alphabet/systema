//! `Type=dbus` support: the start job completes only once the service's
//! `BusName=` is owned on the system bus.
//!
//! systemd subscribes to `NameOwnerChanged` on the bus (`service_setup_bus_name()`
//! / `unit_watch_bus_name()`, `src/core/service.c`); when the name gains an
//! owner while the service is in `SERVICE_START`, the start job completes
//! (`service_bus_name_owner_change()` → `service_enter_start_post()`).
//!
//! This module replicates that for systema-syss with an equivalent poll:
//! `NameHasOwner` is queried repeatedly until it returns true or the
//! `TimeoutStartSec` deadline elapses.  A poll cannot observe the name too
//! early — the service's process cannot own the name before it is spawned —
//! and systemd likewise does not require the owner to be the service's main
//! PID for `Type=dbus` (only a non-empty owner).
//!
//! The bus connection is created lazily on first use and reused across
//! starts.  During the start of the bus daemon itself (`dbus.service`,
//! `BusName=org.freedesktop.DBus`) the connection attempt fails until the
//! daemon accepts connections; the wait loop retries, and once the daemon
//! is up the driver name is already owned, so there is no deadlock.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use parking_lot::Mutex;
use tracing::{debug, warn};
use zbus::Connection;

use crate::process::{is_alive, pid_is_zombie};

/// Lazily-established system bus connection, shared across waits.
#[derive(Clone, Default)]
pub struct DbusWaiter {
    connection: Arc<Mutex<Option<Connection>>>,
}

impl DbusWaiter {
    /// Wait until `bus_name` is owned on the system bus, polling
    /// `NameHasOwner` every 100 ms.
    ///
    /// Fails with an error when the process exits or the `timeout_secs`
    /// deadline elapses first.
    pub async fn wait_name_owned(&self, bus_name: &str, pid: u32, timeout_secs: u64) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(timeout_secs);
        loop {
            // The service died without acquiring its name: the monitor
            // reaps it, so fail the start immediately (systemd fails the
            // start job as soon as the process exits in SERVICE_START).
            if !is_alive(pid) || pid_is_zombie(pid) {
                return Err(anyhow!(sysa::l10n::fmt(sysa::l10n::t_("Service process (PID {pid}) exited before acquiring D-Bus name '{bus_name}'."), &[("pid", &pid.to_string()), ("bus_name", &bus_name.to_string())])));
            }

            let conn = match self.connection().await {
                Some(conn) => conn,
                None => {
                    if Instant::now() >= deadline {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };

            let owned = conn
                .call_method(
                    Some("org.freedesktop.DBus"),
                    "/org/freedesktop/DBus",
                    Some("org.freedesktop.DBus"),
                    "NameHasOwner",
                    &(bus_name,),
                )
                .await
                .ok()
                .and_then(|msg| msg.body().deserialize::<bool>().ok())
                .unwrap_or(false);
            if owned {
                return Ok(());
            }
            if Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Err(anyhow!(sysa::l10n::fmt(
            sysa::l10n::t_("TimeoutStartSec exceeded waiting for D-Bus name '{bus_name}'."),
            &[("bus_name", &bus_name.to_string())]
        )))
    }

    /// The cached connection, connecting to the system bus on first use.
    /// Failed attempts clear the cache so the next call retries.
    async fn connection(&self) -> Option<Connection> {
        if let Some(conn) = self.connection.lock().as_ref() {
            return Some(conn.clone());
        }
        match Connection::system().await {
            Ok(conn) => {
                debug!("Connected to the system bus for Type=dbus readiness");
                *self.connection.lock() = Some(conn.clone());
                Some(conn)
            }
            Err(e) => {
                // The bus may still be starting (e.g. dbus.service itself):
                // drop the failed attempt and let the caller retry.
                warn!("Type=dbus: system bus connection failed: {}", e);
                *self.connection.lock() = None;
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wait_fails_quickly_when_process_exits() {
        // The service dies without acquiring its name: the wait must abort
        // immediately instead of spinning until the deadline.
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("sleep 0.3")
            .spawn()
            .expect("spawn helper process");
        let err = DbusWaiter::default()
            .wait_name_owned("org.example.NeverOwned", child.id(), 30)
            .await
            .expect_err("process death must abort the wait");
        assert!(
            err.to_string().contains("exited before acquiring"),
            "unexpected: {err}"
        );
        let _ = child.wait();
    }

    #[tokio::test]
    async fn wait_times_out_when_name_never_owned() {
        // The test process itself stays alive; a name nobody owns can never
        // appear, so the wait must end at the TimeoutStartSec deadline.
        let err = DbusWaiter::default()
            .wait_name_owned("org.example.NeverOwned", std::process::id(), 1)
            .await
            .expect_err("unowned name must time out");
        assert!(
            err.to_string().contains("TimeoutStartSec"),
            "unexpected: {err}"
        );
    }
}
