use std::time::Duration;

use anyhow::Result;
use prost::Message;
use sysa::proto::ScopeAbandon;
use sysa::worker_ipc::{EventPublisher, WorkerIpc};
use tracing::{debug, info, warn};

use crate::cgroup;
use crate::controller::{stop_and_wait, ScopeController};
use crate::state::{new_registry, ScopeRegistry, ScopeState};

const WORKER_ID: &str = "system-e-1";
const WORKER_UNIT_TYPES: &[&str] = &["scope"];

pub async fn run() -> Result<()> {
    let registry = new_registry();
    WorkerIpc::new(WORKER_ID, WORKER_UNIT_TYPES)
        .run(
            |event_pub| ScopeController::new(registry.clone(), event_pub),
            |env, event_pub| handle_custom(env, event_pub, registry.clone()),
        )
        .await
}

/// Handle System A → worker envelopes that are not `method.call`:
/// currently `scope.abandon` (the scope's controller gave up on it).
fn handle_custom(
    env: &sysa::proto::Envelope,
    event_pub: &EventPublisher,
    registry: ScopeRegistry,
) -> Result<bool> {
    if env.method == "scope.abandon" {
        let abandon = ScopeAbandon::decode(env.payload.as_slice())?;
        abandon_scope(&registry, event_pub, &abandon.unit_name);
        return Ok(true);
    }
    Ok(false)
}

/// Transition a scope to `abandoned` (systemd `scope_abandon`): the wrapped
/// processes keep running but the scope is no longer managed or killed by
/// us, and the cgroup is not watched for emptiness anymore.
fn abandon_scope(registry: &ScopeRegistry, event_pub: &EventPublisher, unit_name: &str) {
    let mut reg = registry.lock();
    let Some(inst) = reg.get_mut(unit_name) else {
        debug!("scope.abandon for unknown scope {unit_name}");
        return;
    };
    if !matches!(inst.state, ScopeState::Running | ScopeState::Abandoned) {
        warn!("Cannot abandon scope {unit_name} in state {:?}", inst.state);
        return;
    }
    inst.state = ScopeState::Abandoned;
    let main_pid = inst.pids.first().copied().unwrap_or(0);
    let invocation_id = inst.invocation_id.clone().unwrap_or_default();
    drop(reg);

    let status = sysa::controller::UnitStatus {
        unit_name: unit_name.to_string(),
        active_state: "active".to_string(),
        sub_state: "abandoned".to_string(),
        main_pid,
        invocation_id,
        extensions: std::collections::HashMap::new(),
    };
    event_pub.publish_unit_state_update(vec![status], false);
    info!("Scope {unit_name} abandoned");
}

/// Background monitor for a running scope.
///
/// Watches the scope's cgroup `cgroup.events` for `populated 0` — when the
/// wrapped processes all exited, the scope dies (systemd
/// `scope_notify_cgroup_empty_event`).  Also enforces `RuntimeMaxSec=` by
/// killing the scope once the runtime budget is exhausted (systemd
/// `scope_dispatch_timer`).
pub(crate) async fn monitor_scope(
    registry: ScopeRegistry,
    event_pub: EventPublisher,
    unit_name: String,
    cgroup_path: String,
) {
    let start = std::time::Instant::now();
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;

        let (state, runtime_max_secs, timeout_stop_secs) = {
            let reg = registry.lock();
            match reg.get(&unit_name) {
                None => break,
                Some(inst) => (
                    inst.state,
                    inst.runtime_max_secs,
                    inst.timeout_stop_secs,
                ),
            }
        };

        // The scope was abandoned or is being stopped elsewhere — stop
        // monitoring (abandoned scopes are not watched for emptiness).
        if !matches!(state, ScopeState::Running) {
            break;
        }

        // The scope's cgroup emptied: every wrapped process exited.  Skip
        // while System R has not materialised the cgroup yet.
        if cgroup::cgroup_exists(&cgroup_path) && cgroup::is_cgroup_empty(&cgroup_path) {
            dead_and_publish(&registry, &event_pub, &unit_name);
            break;
        }

        // RuntimeMaxSec= elapsed: kill the scope.
        if runtime_max_secs > 0
            && start.elapsed() >= Duration::from_secs(u64::from(runtime_max_secs))
        {
            warn!("Scope {unit_name} exceeded RuntimeMaxSec, stopping");
            stop_and_wait(
                &registry,
                &event_pub,
                &unit_name,
                &cgroup_path,
                timeout_stop_secs,
            )
            .await;
            break;
        }
    }
}

fn dead_and_publish(registry: &ScopeRegistry, event_pub: &EventPublisher, unit_name: &str) {
    {
        let mut reg = registry.lock();
        if let Some(inst) = reg.get_mut(unit_name) {
            inst.state = ScopeState::Dead;
            inst.invocation_id = None;
        }
    }
    let status = sysa::controller::UnitStatus {
        unit_name: unit_name.to_string(),
        active_state: "inactive".to_string(),
        sub_state: "dead".to_string(),
        main_pid: 0,
        invocation_id: String::new(),
        extensions: std::collections::HashMap::new(),
    };
    event_pub.publish_unit_state_update(vec![status], false);
    info!("Scope {unit_name} cgroup empty — dead");
}
