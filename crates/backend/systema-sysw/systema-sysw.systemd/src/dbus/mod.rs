//! D-Bus server for the System Wrapper systemd bridge.
//!
//! Implements `org.freedesktop.systemd1` exactly like System A's in-process
//! D-Bus layer did, but reading all state from the local [`UnitMirror`]
//! (populated via the control-port bus) and translating every mutation to a
//! `manager.*` control RPC.  The bridge never touches System A's allocator.

pub mod activator;
pub mod manager;
pub mod mount_obj;
pub mod properties;
pub mod scope_obj;
pub mod service_obj;
pub mod slice_obj;
pub mod socket_obj;
pub mod unit_obj;

use std::sync::Arc;
use std::time::Duration;

use once_cell::sync::OnceCell;
use tracing::{info, warn};
use zbus::connection::Builder;

/// Budget for connecting to the system bus.
///
/// At boot the system bus (`dbus.service`) is on-demand: the socket exists
/// (bound by the socket-activation layer) but no daemon accepts yet.  zbus's
/// `connect()` succeeds against the listening backlog and then blocks in the
/// SASL handshake forever — with no timeout a missing bus would stall the
/// reconnect loop in `main`, so the bridge would never claim
/// `org.freedesktop.systemd1`.
const BUS_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

use systema_sysw_common::ControlClient;

use crate::mirror::MirrorHandle;

/// The well-known D-Bus bus name we claim.
pub const BUS_NAME: &str = "org.freedesktop.systemd1";

/// Everything a bridge object needs to serve a call without allocator
/// access: the mirror plus the control-port client for mutations.
pub struct BridgeContext {
    pub mirror: MirrorHandle,
    pub client: Arc<ControlClient>,
    pub conn: Arc<OnceCell<zbus::Connection>>,
}

impl BridgeContext {
    pub fn new(mirror: MirrorHandle, client: Arc<ControlClient>) -> Arc<Self> {
        Arc::new(Self {
            mirror,
            client,
            conn: Arc::new(OnceCell::new()),
        })
    }
}

/// The D-Bus connection, if the server has finished booting.
pub(super) fn connection(ctx: &BridgeContext) -> Option<&zbus::Connection> {
    ctx.conn.get()
}

/// Register the per-unit D-Bus object (and its type-specific interface) for
/// `unit_name` on the connection's object server.  Idempotent: re-registering
/// an already registered interface is a no-op, and the wiring is re-attempted
/// so type interfaces can be added once a snapshot (with `kind`) arrives.
pub async fn register_unit_object(
    conn: &zbus::Connection,
    ctx: &Arc<BridgeContext>,
    unit_name: &str,
) {
    let kind = ctx
        .mirror
        .read()
        .get(unit_name)
        .map(|s| s.kind.clone())
        .unwrap_or_default();
    let path = manager::unit_object_path(unit_name);

    let unit = unit_obj::UnitObject {
        ctx: ctx.clone(),
        unit_name: unit_name.to_string(),
    };
    if let Err(e) = conn.object_server().at(path.clone(), unit).await {
        warn!("Failed to register Unit interface for {}: {}", unit_name, e);
    }

    match kind.as_str() {
        "service" => {
            let obj = service_obj::ServiceObject {
                ctx: ctx.clone(),
                unit_name: unit_name.to_string(),
            };
            if let Err(e) = conn.object_server().at(path.clone(), obj).await {
                warn!("Failed to register Service interface for {}: {}", unit_name, e);
            }
        }
        "socket" => {
            let obj = socket_obj::SocketObject {
                ctx: ctx.clone(),
                unit_name: unit_name.to_string(),
            };
            if let Err(e) = conn.object_server().at(path.clone(), obj).await {
                warn!("Failed to register Socket interface for {}: {}", unit_name, e);
            }
        }
        "slice" => {
            let obj = slice_obj::SliceObject {
                unit_name: unit_name.to_string(),
            };
            if let Err(e) = conn.object_server().at(path.clone(), obj).await {
                warn!("Failed to register Slice interface for {}: {}", unit_name, e);
            }
        }
        "scope" => {
            let obj = scope_obj::ScopeObject {
                ctx: ctx.clone(),
                unit_name: unit_name.to_string(),
            };
            if let Err(e) = conn.object_server().at(path.clone(), obj).await {
                warn!("Failed to register Scope interface for {}: {}", unit_name, e);
            }
        }
        "mount" => {
            let obj = mount_obj::MountObject {
                ctx: ctx.clone(),
                unit_name: unit_name.to_string(),
            };
            if let Err(e) = conn.object_server().at(path.clone(), obj).await {
                warn!("Failed to register Mount interface for {}: {}", unit_name, e);
            }
        }
        _ => {}
    }

    // Replace zbus's built-in Properties with our empty-GetAll-accepting
    // implementation (systemd extension), after all other interfaces are up.
    let custom_props = properties::Properties {
        ctx: ctx.clone(),
        unit_name: unit_name.to_string(),
    };
    if let Err(e) = conn
        .object_server()
        .remove::<zbus::fdo::Properties, _>(path.clone())
        .await
    {
        warn!("Failed to remove default Properties for {}: {}", unit_name, e);
    }
    if let Err(e) = conn.object_server().at(path, custom_props).await {
        warn!("Failed to register custom Properties for {}: {}", unit_name, e);
    }

    info!("Registered D-Bus unit object for {}", unit_name);
}

/// Build the D-Bus connection: claim `org.freedesktop.systemd1`, serve the
/// Manager and its custom Properties interface, then register per-unit
/// objects for every unit currently in the mirror.
pub async fn run_dbus(ctx: &Arc<BridgeContext>) -> zbus::Result<zbus::Connection> {
    // Safety check: verify no other process already owns our bus name, so we
    // never conflict with a real systemd init.
    {
        let probe = tokio::time::timeout(BUS_CONNECT_TIMEOUT, zbus::Connection::system())
            .await
            .map_err(|_| {
                zbus::Error::Handshake(
                    sysa::l10n::t_("timed out connecting to the system bus").to_string(),
                )
            })??;
        let has_owner: bool = probe
            .call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "NameHasOwner",
                &(BUS_NAME,),
            )
            .await?
            .body()
            .deserialize()?;
        if has_owner {
            warn!(
                "D-Bus name '{BUS_NAME}' is already owned by another process. Refusing to run to avoid conflicting with an existing init system."
            );
            drop(probe);
        }
    }

    let manager = manager::ManagerInterface::new(ctx.clone());

    let conn = tokio::time::timeout(
        BUS_CONNECT_TIMEOUT,
        Builder::system()?
            .name(BUS_NAME)?
            .serve_at("/org/freedesktop/systemd1", manager)?
            .build(),
    )
    .await
    .map_err(|_| {
        zbus::Error::Handshake(sysa::l10n::t_("timed out connecting to the system bus").to_string())
    })??;

    // Replace zbus's built-in org.freedesktop.DBus.Properties on the manager
    // with our systemd-flavoured custom implementation.
    if let Err(e) = conn
        .object_server()
        .remove::<zbus::fdo::Properties, _>("/org/freedesktop/systemd1")
        .await
    {
        warn!("Failed to remove default Properties interface from manager: {}", e);
    }
    let props = properties::ManagerProperties {
        ctx: ctx.clone(),
    };
    match conn.object_server().at("/org/freedesktop/systemd1", props).await {
        Ok(_) => {}
        Err(e) => {
            warn!("Failed to register custom Properties on manager: {}", e);
        }
    }

    let _ = ctx.conn.set(conn.clone());
    info!("System Wrapper D-Bus server running");

    // Register per-unit objects for all units already mirrored.
    let initial: Vec<String> = {
        let mirror = ctx.mirror.read();
        mirror.names()
    };
    for unit_name in initial {
        register_unit_object(&conn, ctx, &unit_name).await;
    }

    Ok(conn)
}

/// Best-effort helper: resolve a possibly-alias unit name to its canonical
/// mirror name.  When the name is unknown to the mirror, ask System A for the
/// (alias-resolved) snapshot and upsert it, falling back to the input name
/// unchanged if the server cannot resolve it.
pub async fn resolve_canonical(ctx: &BridgeContext, name: &str) -> String {
    if let Some(snap) = ctx.mirror.read().get(name) {
        return snap.name.clone();
    }
    match systema_sysw_common::client::calls::unit_snapshot(&ctx.client, name).await {
        Ok(snap) => {
            ctx.mirror.write().upsert(snap.clone());
            snap.name
        }
        Err(_) => name.to_string(),
    }
}

/// Ensure a per-unit D-Bus object is registered, registering it if needed.
/// Units appear in the mirror either via the initial seed, the `unit.new`
/// events, or after an RPC loaded them; this closes the small window between
/// the RPC reply and the event loop.
pub async fn ensure_unit_object(ctx: &Arc<BridgeContext>, name: &str) {
    let Some(conn) = connection(ctx) else {
        return;
    };
    let canonical = resolve_canonical(ctx, name).await;
    register_unit_object(conn, ctx, &canonical).await;
}

/// Convenience: borrow a snapshot from the mirror.
pub fn snapshot_of(ctx: &BridgeContext, name: &str) -> Option<sysa::proto::UnitSnapshot> {
    ctx.mirror.read().get(name).cloned()
}