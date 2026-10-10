//! IPC entry point for the System N worker.
//!
//! Registers with System A as the `path` worker and owns every `.path` unit.
//! The worker subscribes to unit events for **all** units so it can observe
//! when a triggered target unit terminates: `event.publish` envelopes carry a
//! [`UnitResourceEvent`] whose `active_state` tells System N to re-arm the
//! path unit (and immediately re-check level-triggered conditions).
//!
//! On every connection the worker opens a short reconciliation window while
//! System A replays the currently active units; any path unit that was
//! `Running` across the disconnection and whose target is no longer active is
//! re-armed, so a target that stopped while the worker was offline is picked
//! up without waiting for another state change.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use prost::Message as ProstMessage;
use sysa::proto::{Envelope, UnitResourceEvent};
use sysa::worker_ipc::WorkerIpc;
use tracing::{debug, warn};

use crate::controller::PathController;
use crate::engine::{on_target_inactive, spawn_engine, EngineShared};
use crate::state::PathState;

const WORKER_ID: &str = "system-n-1";
const WORKER_UNIT_TYPES: &[&str] = &["path"];
/// How long to collect the replay of active units sent by System A right
/// after (re)subscribing before reconciling `Running` path units.
const RECONCILE_WINDOW: Duration = Duration::from_secs(1);

/// Reconnect reconciliation state: the set of active units observed while the
/// replay window is open.
struct Reconcile {
    deadline: Instant,
    active: HashSet<String>,
}

impl Reconcile {
    fn begin() -> Self {
        Reconcile {
            deadline: Instant::now() + RECONCILE_WINDOW,
            active: HashSet::new(),
        }
    }

    fn is_open(&self) -> bool {
        Instant::now() < self.deadline
    }
}

/// Run the System N worker IPC loop (reconnecting on failure) until the
/// process is stopped.
pub async fn run() -> Result<()> {
    let backend = crate::backend::default_backend()?;
    let shared = EngineShared::new(backend);
    spawn_engine(shared.clone());

    let reconcile: Arc<Mutex<Option<Reconcile>>> = Arc::new(Mutex::new(None));

    let shared_factory = shared.clone();
    let shared_handler = shared.clone();
    let reconcile_factory = reconcile.clone();
    let reconcile_handler = reconcile.clone();

    WorkerIpc::new(WORKER_ID, WORKER_UNIT_TYPES)
        .run(
            move |event_pub| {
                // Refresh the publisher used by the background engine on
                // every (re)connection.
                *shared_factory.event_pub.lock() = Some(event_pub.clone());
                // Subscribe to events for every unit so target terminations
                // are observed; System A replays active units on subscribe.
                event_pub.subscribe_units(&[]);
                debug!("System N subscribed to unit events (all)");
                *reconcile_factory.lock().unwrap() = Some(Reconcile::begin());
                PathController::new(shared_factory.clone())
            },
            move |env, _event_pub| {
                if env.method == "event.publish" {
                    handle_unit_event(env, &shared_handler, &reconcile_handler);
                    return Ok(true);
                }
                Ok(false)
            },
        )
        .await
}

/// Decode an `event.publish` envelope and react to target unit terminations.
fn handle_unit_event(
    env: &Envelope,
    shared: &Arc<EngineShared>,
    reconcile: &Arc<Mutex<Option<Reconcile>>>,
) {
    let event = match UnitResourceEvent::decode(env.payload.as_slice()) {
        Ok(e) => e,
        Err(e) => {
            warn!("Cannot decode UnitResourceEvent from System A: {e}");
            return;
        }
    };

    // Reconnect reconciliation: while the window is open, collect the active
    // units replayed by System A; once it closes, re-arm every Running path
    // unit whose target is no longer active.
    {
        let mut guard = reconcile.lock().unwrap();
        if let Some(rec) = guard.as_mut() {
            if rec.is_open() {
                if event.active_state == "active" {
                    rec.active.insert(event.unit_name.clone());
                }
                return;
            }
            let rec = guard.take().unwrap();
            reconcile_running_units(shared, &rec.active);
        }
    }

    if event.active_state == "inactive" || event.active_state == "failed" {
        let targets: Vec<String> = {
            let reg = shared.registry.read();
            reg.iter()
                .filter(|(_, inst)| inst.state == PathState::Running)
                .filter(|(_, inst)| inst.target_unit == event.unit_name)
                .map(|(name, _)| name.clone())
                .collect()
        };
        for unit in targets {
            debug!(
                "Path unit '{unit}': target '{}' became {} — re-arming",
                event.unit_name, event.active_state
            );
            on_target_inactive(shared, &unit);
        }
    }
}

/// Re-arm every `Running` path unit whose target is not among the active
/// units replayed after a (re)connection.
fn reconcile_running_units(shared: &Arc<EngineShared>, active: &HashSet<String>) {
    let running: Vec<String> = {
        let reg = shared.registry.read();
        reg.iter()
            .filter(|(_, inst)| inst.state == PathState::Running)
            .map(|(name, _)| name.clone())
            .collect()
    };
    for unit in running {
        let target_active = {
            let reg = shared.registry.read();
            reg.get(&unit)
                .map(|i| active.contains(&i.target_unit))
                .unwrap_or(true)
        };
        if !target_active {
            debug!("Path unit '{unit}': target not active after reconnect — re-arming");
            on_target_inactive(shared, &unit);
        }
    }
}
