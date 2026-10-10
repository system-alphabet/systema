use async_trait::async_trait;
use prost::Message as ProstMessage;
use sysa::controller::UnitStatus;
use sysa::event_bus::{Event, EventSubscriber, EventTopic};
use sysa::proto::Envelope;
use tracing::{debug, info, warn};

use crate::scheduler::{schedule_automatic_restart, should_restart_service};
use crate::state::{AllocatorHandle, AllocatorState};
use crate::unit::types::ExitKind;

// ---------------------------------------------------------------------------
// RestartHandler
// ---------------------------------------------------------------------------

/// True when the unit still has a live instance per the cached runtime
/// state: it is either `active`/`activating`, or a main PID is tracked.
///
/// Guards automatic restarts from being scheduled on top of an instance
/// that is still running (e.g. the failure report raced with a fresh start
/// that is already active/activating). Service workers clear `main_pid` on
/// exit, so a genuine `failed` report is never blocked by this check.
fn has_live_instance(state: &AllocatorState, unit_name: &str) -> bool {
    state
        .unit_states
        .get(unit_name)
        .map(|c| matches!(c.active_state.as_str(), "active" | "activating") || c.main_pid != 0)
        .unwrap_or(false)
}

/// Listens for unit state changes, evaluates the unit's `RestartPolicy`
/// on failure, and schedules an automatic restart if needed.
///
/// Workers report failures through the unified `unit.state_update` protocol
/// (`active_state == "failed"`), carrying the process exit status in the
/// `last_exit_code` extension; the per-unit restart policy is only ever
/// consulted for units that have a `[Service]` section.
pub struct RestartHandler {
    allocator: AllocatorHandle,
}

impl RestartHandler {
    pub fn new(allocator: AllocatorHandle) -> Self {
        RestartHandler { allocator }
    }
}

#[async_trait]
impl EventSubscriber for RestartHandler {
    fn topics(&self) -> Vec<EventTopic> {
        vec![EventTopic::UnitStateChange]
    }

    async fn on_event(&self, event: &Event) {
        if event.topic != EventTopic::UnitStateChange {
            return;
        }
        let unit_name = &event.unit_name;

        // The event payload is the protobuf-encoded UnitStatus that the
        // worker published in its `unit.state_update`.
        let status = match UnitStatus::decode_from(&event.data) {
            Some(status) => status,
            None => {
                warn!("EventBus: failed to decode UnitStatus for {}", unit_name);
                return;
            }
        };
        let Some(exit_kind) = restart_decision(&status) else {
            return;
        };

        let should = {
            let state = self.allocator.read();
            state
                .units
                .get(unit_name)
                .and_then(|u| u.service.as_ref())
                .map(|svc| should_restart_service(&svc.restart, &exit_kind))
                .unwrap_or(false)
        };

        if should {
            let state = self.allocator.read();
            if has_live_instance(&state, unit_name) {
                let cached = state.unit_states.get(unit_name).unwrap();
                warn!(
                    "EventBus: restart of {} suppressed: live instance present (active_state={}, main_pid={})",
                    unit_name, cached.active_state, cached.main_pid
                );
                return;
            }
            info!(
                "EventBus: restart triggered for {} (sub_state={:?}, exit_kind={:?})",
                unit_name, status.sub_state, exit_kind
            );
            schedule_automatic_restart(self.allocator.clone(), unit_name);
        }
    }
}

// ---------------------------------------------------------------------------
// NotifyBroadcaster
// ---------------------------------------------------------------------------

/// Forwards unit state changes to the notify channel for the bootlog and
/// the boot animation.
///
/// Each `unit.state_update` that reaches the event bus becomes one notify
/// event.  Transitions are derived from the new state alone — no previous
/// state is needed: `active` is a successful start, `failed` is a failed
/// start, `inactive` is a completed stop.  The "about to start / stop"
/// events come from the job-enqueue hooks in the scheduler.
#[derive(Default)]
pub struct NotifyBroadcaster {}

impl NotifyBroadcaster {
    pub fn new() -> Self {
        NotifyBroadcaster {}
    }
}

#[async_trait]
impl EventSubscriber for NotifyBroadcaster {
    fn topics(&self) -> Vec<EventTopic> {
        vec![EventTopic::UnitStateChange]
    }

    async fn on_event(&self, event: &Event) {
        if event.topic != EventTopic::UnitStateChange {
            return;
        }
        let Some(status) = UnitStatus::decode_from(&event.data) else {
            warn!(
                "NotifyBroadcaster: failed to decode UnitStatus for {}",
                event.unit_name
            );
            return;
        };
        match status.active_state.as_str() {
            "active" => {
                sysa::notify::broadcast(&[
                    ("UNIT_STARTED", &status.unit_name),
                    ("RESULT", "success"),
                ]);
            }
            "failed" => {
                sysa::notify::broadcast(&[
                    ("UNIT_STARTED", &status.unit_name),
                    ("RESULT", "failed"),
                ]);
            }
            "inactive" => {
                sysa::notify::broadcast(&[
                    ("UNIT_STOPPED", &status.unit_name),
                    ("RESULT", "success"),
                ]);
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// WorkerEventForwarder
// ---------------------------------------------------------------------------

/// Forwards unit events from the in-process event bus to a specific System
/// Worker over its IPC connection.
///
/// SysA re-dispatches every `unit.state_update` as an
/// [`EventTopic::UnitStateChange`].  A worker that subscribes to specific
/// units registers one forwarder per connection; the forwarder is
/// registered against `EventTopic::Unit(name)` topics (or
/// `UnitStateChange` when the worker wants every unit) so it only receives
/// matching events, and re-emits them as `event.publish` envelopes carrying
/// a protobuf-encoded [`UnitResourceEvent`](sysa::proto::UnitResourceEvent)
/// — the UnitIR projection System R applies to cgroups.
pub struct WorkerEventForwarder {
    worker_id: String,
    tx: tokio::sync::mpsc::Sender<bytes::Bytes>,
    all: bool,
    units: Vec<String>,
    allocator: AllocatorHandle,
}

impl WorkerEventForwarder {
    pub fn new(
        worker_id: &str,
        tx: tokio::sync::mpsc::Sender<bytes::Bytes>,
        all: bool,
        units: Vec<String>,
        allocator: AllocatorHandle,
    ) -> Self {
        WorkerEventForwarder {
            worker_id: worker_id.to_string(),
            tx,
            all,
            units,
            allocator,
        }
    }

    fn matches(&self, event: &Event) -> bool {
        self.all || self.units.iter().any(|u| u == &event.unit_name)
    }
}

#[async_trait]
impl EventSubscriber for WorkerEventForwarder {
    fn topics(&self) -> Vec<EventTopic> {
        if self.all {
            vec![EventTopic::UnitStateChange]
        } else {
            self.units.iter().cloned().map(EventTopic::Unit).collect()
        }
    }

    async fn on_event(&self, event: &Event) {
        if !self.matches(event) {
            return;
        }

        // The event payload is the protobuf-encoded UnitStatus; its
        // active_state and main_pid are stamped onto the resource event so
        // System R knows whether to apply the limits or release the entry,
        // and which process to move into the unit's cgroup.
        let decoded = UnitStatus::decode_from(&event.data);
        let active_state = decoded
            .as_ref()
            .map(|s| s.active_state.clone())
            .unwrap_or_default();
        let main_pid = decoded.as_ref().map(|s| s.main_pid).unwrap_or(0);
        let Some(resource_event) =
            build_unit_resource_event(&self.allocator, &event.unit_name, &active_state, main_pid)
        else {
            debug!(
                "EventBus: no UnitResourceEvent for '{}' (not in runtime cache)",
                event.unit_name
            );
            return;
        };

        let mut payload = bytes::BytesMut::with_capacity(resource_event.encoded_len());
        if let Err(e) = resource_event.encode(&mut payload) {
            warn!(
                "EventBus: failed to encode UnitResourceEvent for '{}': {}",
                event.unit_name, e
            );
            return;
        }

        let envelope = Envelope {
            request_id: 0,
            source: "system-a".to_string(),
            target: self.worker_id.clone(),
            method: "event.publish".to_string(),
            payload: payload.to_vec(),
        };

        let mut buf = bytes::BytesMut::with_capacity(envelope.encoded_len());
        if let Err(e) = envelope.encode(&mut buf) {
            warn!(
                "EventBus: failed to encode event.publish for '{}': {}",
                self.worker_id, e
            );
            return;
        }

        if let Err(e) = self.tx.try_send(buf.freeze()) {
            warn!(
                "EventBus: failed to forward '{}' to '{}': {}",
                event.unit_name, self.worker_id, e
            );
        }
    }
}

/// Build the [`UnitResourceEvent`](sysa::proto::UnitResourceEvent) pushed to
/// System R from a unit's runtime IR.
///
/// This is the UnitIR projection: only the `[Unit] Slice=` parent and the
/// resource-control limits cross the wire.  Returns `None` when the unit is
/// not in the runtime cache (nothing to project).
pub fn build_unit_resource_event(
    allocator: &AllocatorHandle,
    unit_name: &str,
    active_state: &str,
    main_pid: u32,
) -> Option<sysa::proto::UnitResourceEvent> {
    let state = allocator.read();
    let unit = state.units.get(unit_name)?;
    let resource = match &unit.kind {
        crate::unit::types::UnitKind::Service => unit.service.as_ref().map(|s| &s.rc),
        crate::unit::types::UnitKind::Slice => unit.slice.as_ref().map(|s| &s.rc),
        crate::unit::types::UnitKind::Scope => unit.scope.as_ref().map(|s| &s.rc),
        _ => None,
    }
    .map(sd_resource_control_to_proto);

    Some(sysa::proto::UnitResourceEvent {
        unit_name: unit_name.to_string(),
        active_state: active_state.to_string(),
        slice: unit.unit.slice.clone(),
        resource,
        main_pid,
        pids: state
            .unit_states
            .get(unit_name)
            .map(|s| s.pids.clone())
            .unwrap_or_default(),
    })
}

/// Push a `event.publish` envelope carrying a [`UnitResourceEvent`] for every
/// currently active unit matching a subscription, so the worker converges
/// immediately on (re)subscribe without waiting for the next transition.
///
/// `all` matches every unit; otherwise only the names in `units` match.
/// Best-effort: a full send queue aborts the remaining replay.
pub fn replay_active_units(
    allocator: &AllocatorHandle,
    worker_id: &str,
    forward_tx: &tokio::sync::mpsc::Sender<bytes::Bytes>,
    all: bool,
    units: &std::collections::HashSet<String>,
) {
    let active: Vec<String> = {
        let state = allocator.read();
        state
            .unit_states
            .iter()
            .filter(|(name, cached)| {
                let matched = all || units.contains(*name);
                matched && cached.active_state == "active"
            })
            .map(|(name, _)| name.clone())
            .collect()
    };

    for name in active {
        let main_pid = {
            let state = allocator.read();
            state
                .unit_states
                .get(&name)
                .map(|c| c.main_pid)
                .unwrap_or(0)
        };
        let Some(resource_event) = build_unit_resource_event(allocator, &name, "active", main_pid)
        else {
            continue;
        };
        let mut payload = bytes::BytesMut::with_capacity(resource_event.encoded_len());
        if resource_event.encode(&mut payload).is_err() {
            continue;
        }
        let envelope = Envelope {
            request_id: 0,
            source: "system-a".to_string(),
            target: worker_id.to_string(),
            method: "event.publish".to_string(),
            payload: payload.to_vec(),
        };
        let mut buf = bytes::BytesMut::with_capacity(envelope.encoded_len());
        if envelope.encode(&mut buf).is_err() {
            continue;
        }
        if forward_tx.try_send(buf.freeze()).is_err() {
            break;
        }
    }
}

/// Convert a parsed systemd resource-control block into the protobuf
/// projection sent to System R.
fn sd_resource_control_to_proto(
    rc: &crate::unit::types::ResourceControl,
) -> sysa::proto::ResourceConfig {
    sysa::proto::ResourceConfig {
        cpu_quota: rc.cpu_quota.clone(),
        cpu_quota_period: rc.cpu_quota_period.clone(),
        cpu_weight: rc.cpu_weight,
        startup_cpu_weight: rc.startup_cpu_weight,
        cpu_set_cpus: rc.cpu_set_cpus.clone(),
        cpu_set_memory_nodes: rc.cpu_set_memory_nodes.clone(),
        memory_min: rc.memory_min.clone(),
        memory_low: rc.memory_low.clone(),
        memory_high: rc.memory_high.clone(),
        memory_max: rc.memory_max.clone(),
        memory_swap_max: rc.memory_swap_max.clone(),
        io_weight: rc.io_weight,
        startup_io_weight: rc.startup_io_weight,
        io_device_weight: rc.io_device_weight.clone(),
        io_read_bandwidth_max: rc.io_read_bandwidth_max.clone(),
        io_write_bandwidth_max: rc.io_write_bandwidth_max.clone(),
        tasks_max: rc.tasks_max,
        allowed_cpus: rc.allowed_cpus.clone(),
        allowed_memory_nodes: rc.allowed_memory_nodes.clone(),
    }
}

/// Map a worker-reported unit status to a restart-relevant exit kind.
///
/// Only `active_state == "failed"` counts as a failure; the `last_exit_code`
/// extension is otherwise ignored so that a stale code carried over into
/// later (e.g. `activating`) status updates cannot retrigger a restart.
fn restart_decision(status: &UnitStatus) -> Option<ExitKind> {
    if status.active_state != "failed" {
        return None;
    }
    Some(
        status
            .extensions
            .get("last_exit_code")
            .and_then(|code| code.parse::<i32>().ok())
            .map(ExitKind::ExitCode)
            .unwrap_or(ExitKind::ExitCode(-1)),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use crate::state::CachedUnitState;

    use super::*;

    fn unit_event(unit_name: &str) -> Event {
        Event {
            topic: EventTopic::UnitStateChange,
            unit_name: unit_name.to_string(),
            worker_id: "worker".to_string(),
            timestamp: tokio::time::Instant::now(),
            data: bytes::Bytes::new(),
        }
    }

    #[test]
    fn replay_active_units_includes_the_root_slice() {
        use crate::state::Allocator;
        use sysa::proto::UnitResourceEvent;

        let allocator = Allocator::handle();
        // The synthesized root slice is already active; add a service.
        allocator.write().unit_states.insert(
            "demo.service".to_string(),
            CachedUnitState {
                active_state: "active".to_string(),
                sub_state: "running".to_string(),
                main_pid: 42,
                invocation_id: String::new(),
                active_enter_timestamp: 0,
                inactive_enter_timestamp: 0,
                extensions: HashMap::new(),
                pids: Vec::new(),
                controller: String::new(),
            },
        );
        allocator.write().units.insert(
            "demo.service".to_string(),
            crate::unit::types::UnitFile::new("demo.service"),
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(8);
        replay_active_units(
            &allocator,
            "system-r-1",
            &tx,
            true,
            &std::collections::HashSet::new(),
        );

        let mut names = std::collections::HashSet::new();
        while let Ok(buf) = rx.try_recv() {
            let env = Envelope::decode(&buf[..]).expect("envelope decodes");
            let event = UnitResourceEvent::decode(env.payload.as_slice()).expect("event decodes");
            names.insert(event.unit_name);
        }
        assert!(names.contains("-.slice"), "root slice replayed: {names:?}");
        assert!(
            names.contains("demo.service"),
            "service replayed: {names:?}"
        );
    }

    fn status(active: &str, last_exit_code: Option<i32>) -> UnitStatus {
        let mut extensions = HashMap::new();
        if let Some(code) = last_exit_code {
            extensions.insert("last_exit_code".to_string(), code.to_string());
        }
        UnitStatus {
            unit_name: "test.service".to_string(),
            active_state: active.to_string(),
            sub_state: String::new(),
            main_pid: 0,
            invocation_id: String::new(),
            extensions,
        }
    }

    #[test]
    fn failed_with_exit_code_triggers_restart_eval() {
        assert_eq!(
            restart_decision(&status("failed", Some(7))),
            Some(ExitKind::ExitCode(7))
        );
    }

    #[test]
    fn failed_without_exit_code_falls_back_to_unknown() {
        assert_eq!(
            restart_decision(&status("failed", None)),
            Some(ExitKind::ExitCode(-1))
        );
    }

    #[test]
    fn stale_exit_code_on_active_state_does_not_trigger() {
        assert_eq!(restart_decision(&status("active", Some(7))), None);
        assert_eq!(restart_decision(&status("activating", Some(7))), None);
        assert_eq!(restart_decision(&status("inactive", Some(7))), None);
    }

    #[test]
    fn forwarder_matches_only_subscribed_units() {
        use crate::state::Allocator;
        let allocator = Allocator::handle();
        let (tx, _rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(8);
        let scoped = WorkerEventForwarder::new(
            "system-s-1",
            tx,
            false,
            vec!["a.service".to_string(), "b.service".to_string()],
            allocator.clone(),
        );
        assert!(scoped.matches(&unit_event("a.service")));
        assert!(scoped.matches(&unit_event("b.service")));
        assert!(!scoped.matches(&unit_event("c.service")));

        let (tx2, _rx2) = tokio::sync::mpsc::channel::<bytes::Bytes>(8);
        let all = WorkerEventForwarder::new("system-s-2", tx2, true, vec![], allocator);
        assert!(all.matches(&unit_event("anything.service")));
    }

    #[test]
    fn live_instance_detected_for_active_states() {
        let alloc = Arc::new(parking_lot::RwLock::new(AllocatorState::new()));
        for (name, active_state, pid) in [
            ("a.service", "active", 42u32),
            ("b.service", "activating", 0u32),
        ] {
            alloc.write().unit_states.insert(
                name.to_string(),
                CachedUnitState {
                    active_state: active_state.to_string(),
                    sub_state: String::new(),
                    main_pid: pid,
                    invocation_id: String::new(),
                    active_enter_timestamp: 0,
                    inactive_enter_timestamp: 0,
                    extensions: HashMap::new(),
                    pids: Vec::new(),
                    controller: String::new(),
                },
            );
        }
        let state = alloc.read();
        assert!(has_live_instance(&state, "a.service"));
        assert!(has_live_instance(&state, "b.service"));
    }

    #[test]
    fn live_instance_absent_for_failed_and_inactive() {
        let alloc = Arc::new(parking_lot::RwLock::new(AllocatorState::new()));
        for (name, active_state) in [("a.service", "failed"), ("b.service", "inactive")] {
            alloc.write().unit_states.insert(
                name.to_string(),
                CachedUnitState {
                    active_state: active_state.to_string(),
                    sub_state: String::new(),
                    main_pid: 0,
                    invocation_id: String::new(),
                    active_enter_timestamp: 0,
                    inactive_enter_timestamp: 0,
                    extensions: HashMap::new(),
                    pids: Vec::new(),
                    controller: String::new(),
                },
            );
        }
        let state = alloc.read();
        assert!(!has_live_instance(&state, "a.service"));
        assert!(!has_live_instance(&state, "b.service"));
    }

    #[test]
    fn live_instance_absent_for_unknown_unit() {
        let alloc = Arc::new(parking_lot::RwLock::new(AllocatorState::new()));
        let state = alloc.read();
        assert!(!has_live_instance(&state, "missing.service"));
    }
}
