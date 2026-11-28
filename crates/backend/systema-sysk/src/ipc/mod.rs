use std::sync::Arc;

use anyhow::Result;
use parking_lot::RwLock;
use prost::Message as ProstMessage;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use sysa::proto::{Envelope, UnitResourceEvent};
use sysa::worker_ipc::{EventPublisher, WorkerIpc};

use crate::activation::ActivationRegistry;
use crate::controller::SocketController;
use crate::socket::{self};

const WORKER_ID: &str = "system-k-1";
const WORKER_UNIT_TYPES: &[&str] = &["socket"];

pub async fn run() -> Result<()> {
    let socket_manager = socket::new_manager();
    let accepted = socket::new_accepted_connections();
    let fdpass: Arc<Mutex<Option<tokio::net::UnixStream>>> = Arc::new(Mutex::new(None));

    // Socket-activation registry + the publisher slot it fires into.  Both
    // are connection-independent: monitors survive IPC reconnects, and the
    // forwarders below stamp events onto whichever publisher is live.
    let (activation, mut fired_rx) = ActivationRegistry::new();
    let publisher_slot: Arc<RwLock<Option<EventPublisher>>> = Arc::new(RwLock::new(None));
    {
        let slot = publisher_slot.clone();
        tokio::spawn(async move {
            while let Some(unit) = fired_rx.recv().await {
                let ep = slot.read().clone();
                match ep {
                    Some(ep) => ep.send_envelope_bytes("socket.fired", unit.into_bytes()),
                    None => debug!("Dropping activation fire for '{unit}': not connected"),
                }
            }
        });
    }

    // `Accept=yes` connections: the accept loops park each connection fd in
    // `accepted` and queue `<socket unit>\n<instance>` here.  System A starts
    // the instance, which then collects the fd through `socket.request_fd`.
    let (accepted_tx, mut accepted_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    {
        let slot = publisher_slot.clone();
        let accepted_for_drop = accepted.clone();
        tokio::spawn(async move {
            while let Some(payload) = accepted_rx.recv().await {
                let ep = slot.read().clone();
                match ep {
                    Some(ep) => ep.send_envelope_bytes("socket.accepted", payload.into_bytes()),
                    None => {
                        // Without a manager the instance will never ask for
                        // the connection: close it instead of leaking it.
                        if let Some((_, instance)) = payload.split_once('\n') {
                            socket::take_accepted_connection(&accepted_for_drop, instance);
                        }
                        debug!("Dropping accepted connection: not connected");
                    }
                }
            }
        });
    }

    // Connect to fdpass socket (non-fatal if unavailable).
    {
        match tokio::net::UnixStream::connect(sysa::paths::instance().systema_fdpass_sock).await {
            Ok(mut s) => {
                use tokio::io::AsyncWriteExt;
                let ident = format!("{WORKER_ID}\n");
                if let Err(e) = s.write_all(ident.as_bytes()).await {
                    tracing::warn!("Failed to send id on fdpass channel: {}", e);
                }
                *fdpass.lock().await = Some(s);
            }
            Err(e) => {
                tracing::warn!(
                    "Cannot connect to fdpass socket ({}): {}",
                    sysa::paths::instance().systema_fdpass_sock,
                    e,
                );
            }
        }
    }

    let custom = {
        let fdpass = fdpass.clone();
        let socket_manager = socket_manager.clone();
        let activation = activation.clone();
        let accepted = accepted.clone();
        move |env: &Envelope, _ep: &EventPublisher| -> Result<bool> {
            if env.method.as_str() == "socket.request_fd" {
                let unit_name = String::from_utf8(env.payload.clone()).unwrap_or_default();
                let fdpass = fdpass.clone();
                let socket_manager = socket_manager.clone();
                let accepted = accepted.clone();
                tokio::spawn(async move {
                    let guard = fdpass.lock().await;
                    if let Some(ref fdpass) = *guard {
                        // A plain socket unit yields its listening socket;
                        // an `Accept=yes` instance yields the connection the
                        // accept loop parked for it.
                        if let Some(fd) = socket::get_listener_fd(&socket_manager, &unit_name) {
                            info!("Sending fd for '{}' via SCM_RIGHTS", unit_name);
                            if let Err(e) = sysa::ipc::send_fd(fdpass, fd).await {
                                tracing::warn!("Failed to send fd: {}", e);
                            }
                        } else if let Some(fd) =
                            socket::take_accepted_connection(&accepted, &unit_name)
                        {
                            use std::os::fd::AsRawFd;
                            info!(
                                "Sending accepted connection for '{}' via SCM_RIGHTS",
                                unit_name
                            );
                            if let Err(e) = sysa::ipc::send_fd(fdpass, fd.as_raw_fd()).await {
                                tracing::warn!("Failed to send accepted connection: {}", e);
                            }
                        } else {
                            tracing::warn!(
                                "No listener or accepted connection found for '{}'",
                                unit_name
                            );
                        }
                    } else {
                        tracing::warn!("No fdpass channel available");
                    }
                });
                Ok(true)
            } else if env.method.as_str() == "event.publish" {
                // Service state feedback from System A: suppress activation
                // monitors while their service runs, re-arm when it stops.
                match UnitResourceEvent::decode(env.payload.as_slice()) {
                    Ok(ev) => activation.apply_service_state(&ev.unit_name, &ev.active_state),
                    Err(e) => warn!("Cannot decode UnitResourceEvent from System A: {e}"),
                }
                Ok(true)
            } else {
                Ok(false)
            }
        }
    };

    let ipc = WorkerIpc::new(WORKER_ID, WORKER_UNIT_TYPES)
        .with_socket_path(sysa::paths::instance().ipc_socket_path);
    sysa::signals::forward_shutdown(ipc.shutdown_handle());
    ipc.run(
        |event_pub| {
            *publisher_slot.write() = Some(event_pub.clone());
            SocketController::new(
                socket_manager.clone(),
                activation.clone(),
                event_pub,
                accepted.clone(),
                accepted_tx.clone(),
            )
        },
        custom,
    )
    .await
}
