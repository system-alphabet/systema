//! Background scheduling engine.
//!
//! A single tick loop shared by every `.timer` unit managed by System C:
//! every 250 ms it scans the registry for due elapses, fires them (advancing
//! the schedule and asking System A to activate the target unit), and
//! publishes `unit.state_update` messages.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use sysa::controller::UnitStatus;
use sysa::proto::TimerFired;
use sysa::worker_ipc::EventPublisher;
use tracing::{debug, info, warn};

use crate::schedule::{boot_epoch, compute_next_elapse, epoch_now};
use crate::state::{TimerInstance, TimerRegistry, TimerState};

/// State shared between the engine task and the IPC controller.
pub struct EngineShared {
    pub registry: TimerRegistry,
    /// Latest `EventPublisher` from the current IPC connection (None while
    /// disconnected; publishes are then skipped).
    pub event_pub: Mutex<Option<EventPublisher>>,
    /// Epoch anchors for monotonic triggers.
    pub boot_epoch: u64,
    pub started_epoch: u64,
}

impl EngineShared {
    pub fn new() -> Arc<Self> {
        Arc::new(EngineShared {
            registry: crate::state::new_registry(),
            event_pub: Mutex::new(None),
            boot_epoch: boot_epoch(),
            started_epoch: epoch_now(),
        })
    }
}

/// Spawn the engine tick loop on the current runtime.
pub fn spawn_engine(shared: Arc<EngineShared>) {
    tokio::spawn(async move {
        debug!("Timer engine started (tick=250ms)");
        loop {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let now = epoch_now();
            let due: Vec<(String, u64)> = {
                let reg = shared.registry.read();
                reg.iter()
                    .filter(|(_, i)| i.state == TimerState::Waiting)
                    .filter_map(|(n, i)| i.next_elapse.map(|e| (n.clone(), e)))
                    .filter(|(_, e)| *e <= now)
                    .collect()
            };
            for (unit, elapse) in due {
                fire(&shared, &unit, elapse);
            }
        }
    });
}

/// Advance a due timer and trigger its target unit.
fn fire(shared: &Arc<EngineShared>, unit_name: &str, elapse: u64) {
    let target_unit = {
        let mut reg = shared.registry.write();
        let inst = match reg.get_mut(unit_name) {
            Some(i) => i,
            None => return,
        };
        if inst.state != TimerState::Waiting {
            return;
        }
        inst.state = TimerState::Running;
        inst.last_elapse = Some(elapse);
        inst.n_fired += 1;
        inst.last_error = None;

        let now = epoch_now();
        inst.next_elapse = compute_next_elapse(
            &inst.config,
            inst.last_elapse,
            now,
            shared.boot_epoch,
            shared.started_epoch,
            inst.activated_epoch,
        )
        .map(|(e, _)| e);
        if inst.next_elapse.is_none() {
            inst.state = TimerState::Elapsed;
        } else {
            inst.state = TimerState::Waiting;
        }
        inst.target_unit.clone()
    };

    info!("Timer '{unit_name}' fired at epoch {elapse} — triggering '{target_unit}'");
    publish_timer(shared, unit_name);
    send_fired(shared, unit_name, &target_unit, elapse);
}

/// Notify System A that a timer fired, asking it to start the target unit.
pub(crate) fn send_fired(
    shared: &Arc<EngineShared>,
    timer_unit: &str,
    target_unit: &str,
    elapse: u64,
) {
    let guard = shared.event_pub.lock();
    let Some(pub_) = guard.as_ref() else {
        warn!("No IPC connection to System A; cannot trigger '{target_unit}'");
        return;
    };
    pub_.send_envelope(
        "timer.fired",
        TimerFired {
            timer_unit: timer_unit.to_string(),
            target_unit: target_unit.to_string(),
            elapse_epoch: elapse,
        },
    );
}

/// Publish the current runtime state of one timer unit.
pub(crate) fn publish_timer(shared: &Arc<EngineShared>, unit_name: &str) {
    let status = {
        let reg = shared.registry.read();
        match reg.get(unit_name) {
            Some(inst) => status_of(unit_name, inst),
            None => return,
        }
    };
    let guard = shared.event_pub.lock();
    if let Some(pub_) = guard.as_ref() {
        pub_.publish_unit_state_update(vec![status], false);
    }
}

/// Build the standardised `UnitStatus` for a timer instance.
pub(crate) fn status_of(unit_name: &str, inst: &TimerInstance) -> UnitStatus {
    let mut extensions = HashMap::new();
    extensions.insert("target_unit".to_string(), inst.target_unit.clone());
    if let Some(e) = inst.next_elapse {
        extensions.insert("next_elapse".to_string(), e.to_string());
    }
    if let Some(e) = inst.last_elapse {
        extensions.insert("last_elapse".to_string(), e.to_string());
    }
    extensions.insert("n_fired".to_string(), inst.n_fired.to_string());
    extensions.insert("n_missed".to_string(), inst.n_missed.to_string());
    if let Some(err) = &inst.last_error {
        extensions.insert("last_error".to_string(), err.clone());
    }
    UnitStatus {
        unit_name: unit_name.to_string(),
        active_state: inst.state.active_state().to_string(),
        sub_state: inst.state.as_str().to_string(),
        main_pid: 0,
        invocation_id: inst.invocation_id.clone().unwrap_or_default(),
        extensions,
    }
}