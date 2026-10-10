//! Background path-triggering engine.
//!
//! A single task consumes changes from the [`PathBackend`] and re-evaluates
//! every waiting path unit's conditions from the live filesystem.  When a
//! condition is satisfied the unit is triggered: it moves to `Running`, its
//! watches are disarmed (matching systemd — the target unit runs while the
//! path unit waits), and System A is asked to activate the target.  When the
//! target becomes inactive again the unit is re-armed and level-triggered
//! conditions (`PathExists=`, `PathExistsGlob=`, `DirectoryNotEmpty=`) are
//! re-checked immediately, so a condition that is still true re-fires.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use sysa::controller::UnitStatus;
use sysa::proto::PathFired;
use sysa::worker_ipc::EventPublisher;
use tracing::{debug, info, warn};

use crate::backend::{FsEvent, PathBackend};
use crate::spec::{evaluate_spec, stat_signature, PathSpecKind};
use crate::state::{PathInstance, PathRegistry, PathState};

/// State shared between the engine task, the IPC layer, and the controller.
pub struct EngineShared {
    pub registry: PathRegistry,
    /// Latest `EventPublisher` from the current IPC connection (None while
    /// disconnected; publishes are then skipped).
    pub event_pub: Mutex<Option<EventPublisher>>,
    /// Platform filesystem watch backend.
    pub backend: Box<dyn PathBackend>,
}

impl EngineShared {
    pub fn new(backend: Box<dyn PathBackend>) -> Arc<Self> {
        Arc::new(EngineShared {
            registry: crate::state::new_registry(),
            event_pub: Mutex::new(None),
            backend,
        })
    }
}

/// Spawn the engine loop on the current runtime.
pub fn spawn_engine(shared: Arc<EngineShared>) {
    tokio::spawn(async move {
        loop {
            let changes = shared.backend.changes().await;
            for change in changes {
                process_change(&shared, &change.unit, change.event);
            }
        }
    });
}

/// React to a filesystem change observed for a path unit.
pub fn process_change(shared: &Arc<EngineShared>, unit: &str, event: FsEvent) {
    let should_trigger = {
        let reg = shared.registry.read();
        let Some(inst) = reg.get(unit) else {
            return;
        };
        if inst.state != PathState::Waiting {
            return;
        }
        evaluate_instance(inst, event)
    };
    if should_trigger {
        trigger(shared, unit);
    }
}

/// Re-arm a `Running` path unit whose target has become inactive, then
/// re-check level-triggered conditions so a still-satisfied condition
/// immediately re-fires (systemd behaviour).
pub fn on_target_inactive(shared: &Arc<EngineShared>, path_unit: &str) {
    let should_rearm = {
        let mut reg = shared.registry.write();
        let inst = match reg.get_mut(path_unit) {
            Some(i) => i,
            None => return,
        };
        if inst.state != PathState::Running {
            return;
        }
        inst.state = PathState::Waiting;
        inst.last_error = None;
        true
    };
    if !should_rearm {
        return;
    }

    debug!("Path '{path_unit}' re-arming (target became inactive)");
    match arm_unit(shared, path_unit) {
        Ok(()) => recheck_level(shared, path_unit),
        Err(e) => {
            let mut reg = shared.registry.write();
            if let Some(inst) = reg.get_mut(path_unit) {
                inst.state = PathState::Failed;
                inst.last_error = Some(sysa::l10n::fmt(
                    sysa::l10n::t_("Failed to re-arm: {e}"),
                    &[("e", &e.to_string())],
                ));
            }
            publish_path(shared, path_unit);
        }
    }
}

/// Trigger a waiting path unit: rate-limit check, then move to `Running`,
/// disarm, and ask System A to activate the target.
pub fn trigger(shared: &Arc<EngineShared>, unit: &str) {
    let (target, fired) = {
        let mut reg = shared.registry.write();
        let inst = match reg.get_mut(unit) {
            Some(i) => i,
            None => return,
        };
        if inst.state != PathState::Waiting {
            return;
        }
        let now = epoch_now();
        if rate_limited(inst, now) {
            inst.state = PathState::Failed;
            inst.last_error = Some(sysa::l10n::fmt(sysa::l10n::t_("Trigger rate limit exceeded (interval={trigger_limit_interval_sec}s burst={trigger_limit_burst})"), &[("trigger_limit_interval_sec", &(inst.config.trigger_limit_interval_sec).to_string()), ("trigger_limit_burst", &(inst.config.trigger_limit_burst).to_string())]));
            (String::new(), false)
        } else {
            inst.state = PathState::Running;
            inst.n_fired += 1;
            inst.last_fired_epoch = Some(now);
            inst.trigger_times.push_back(now);
            inst.last_error = None;
            (inst.target_unit.clone(), true)
        }
    };

    publish_path(shared, unit);
    if fired {
        shared.backend.disarm(unit);
        let now = epoch_now();
        info!("Path '{unit}' fired at epoch {now} — triggering '{target}'");
        send_fired(shared, unit, &target, now);
    } else {
        info!("Path '{unit}' failed its trigger rate limit; entering failed state");
    }
}

/// Arm the backend for a unit and record fresh stat signatures for its
/// edge-triggered specs.
pub(crate) fn arm_unit(shared: &Arc<EngineShared>, unit: &str) -> anyhow::Result<()> {
    let specs = {
        let reg = shared.registry.read();
        reg.get(unit).map(|i| i.specs.clone()).ok_or_else(|| {
            anyhow::anyhow!(sysa::l10n::fmt(
                sysa::l10n::t_("Unknown path unit '{unit}'"),
                &[("unit", &unit.to_string())]
            ))
        })?
    };
    shared.backend.arm(unit, &specs)?;

    let signatures: Vec<_> = specs
        .iter()
        .map(|s| {
            if s.kind.is_edge() {
                stat_signature(Path::new(&s.path))
            } else {
                None
            }
        })
        .collect();
    let mut reg = shared.registry.write();
    if let Some(inst) = reg.get_mut(unit) {
        inst.spec_signatures = signatures;
    }
    Ok(())
}

/// Re-check level-triggered conditions immediately after (re)arming.
pub(crate) fn recheck_level(shared: &Arc<EngineShared>, unit: &str) {
    let should = {
        let reg = shared.registry.read();
        reg.get(unit)
            .map(|inst| {
                inst.state == PathState::Waiting
                    && inst
                        .specs
                        .iter()
                        .any(|s| !s.kind.is_edge() && evaluate_spec(s))
            })
            .unwrap_or(false)
    };
    if should {
        info!("Path '{unit}' condition already satisfied — triggering");
        trigger(shared, unit);
    }
}

/// Decide whether any condition of a waiting instance is satisfied by the
/// given wake-up event.
fn evaluate_instance(inst: &PathInstance, event: FsEvent) -> bool {
    inst.specs
        .iter()
        .enumerate()
        .any(|(idx, spec)| match spec.kind {
            PathSpecKind::Exists | PathSpecKind::ExistsGlob | PathSpecKind::DirectoryNotEmpty => {
                evaluate_spec(spec)
            }
            PathSpecKind::Changed => {
                event.satisfies_changed() || signature_changed(inst, idx, &spec.path)
            }
            PathSpecKind::Modified => {
                event.satisfies_modified() || signature_changed(inst, idx, &spec.path)
            }
        })
}

/// True when the live stat signature of a spec path differs from the one
/// recorded at arm time.
fn signature_changed(inst: &PathInstance, idx: usize, path: &str) -> bool {
    let before = inst.spec_signatures.get(idx).copied().flatten();
    let after = stat_signature(Path::new(path));
    match (before, after) {
        (None, Some(_)) => true,
        (Some(b), Some(a)) => b != a,
        _ => false,
    }
}

/// Sliding-window trigger rate limiter (`TriggerLimitIntervalSec=` /
/// `TriggerLimitBurst=`).  Returns true when the burst budget is exhausted.
fn rate_limited(inst: &mut PathInstance, now: u64) -> bool {
    let interval = inst.config.trigger_limit_interval_sec as u64;
    let burst = inst.config.trigger_limit_burst as u64;
    if interval == 0 || burst == 0 {
        return false;
    }
    while let Some(&t) = inst.trigger_times.front() {
        if t.saturating_add(interval) < now {
            inst.trigger_times.pop_front();
        } else {
            break;
        }
    }
    inst.trigger_times.len() as u64 >= burst
}

/// Notify System A that a path condition fired, asking it to start the
/// target unit.
pub(crate) fn send_fired(shared: &Arc<EngineShared>, path_unit: &str, target_unit: &str, epoch: u64) {
    let guard = shared.event_pub.lock();
    let Some(pub_) = guard.as_ref() else {
        warn!("No IPC connection to System A; cannot trigger '{target_unit}'");
        return;
    };
    pub_.send_envelope(
        "path.fired",
        PathFired {
            path_unit: path_unit.to_string(),
            target_unit: target_unit.to_string(),
            fired_epoch: epoch,
        },
    );
}

/// Publish the current runtime state of one path unit.
pub(crate) fn publish_path(shared: &Arc<EngineShared>, unit_name: &str) {
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

/// Build the standardised `UnitStatus` for a path instance.
pub(crate) fn status_of(unit_name: &str, inst: &PathInstance) -> UnitStatus {
    let mut extensions = HashMap::new();
    extensions.insert("target_unit".to_string(), inst.target_unit.clone());
    extensions.insert("n_fired".to_string(), inst.n_fired.to_string());
    if let Some(e) = inst.last_fired_epoch {
        extensions.insert("last_fired_epoch".to_string(), e.to_string());
    }
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

/// Current unix epoch in seconds.
pub(crate) fn epoch_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use crate::backend::{PathBackend, PathChange};
    use crate::spec::{PathSpec, PathSpecKind};
    use crate::state::PathInstance;
    use std::time::Duration;
    use sysa::proto::PathConfig;

    /// Backend that never reports changes; unit tests drive the engine
    /// directly through `process_change`/`on_target_inactive`.
    struct NoopBackend;

    #[async_trait]
    impl PathBackend for NoopBackend {
        fn arm(&self, _unit: &str, _specs: &[PathSpec]) -> anyhow::Result<()> {
            Ok(())
        }
        fn disarm(&self, _unit: &str) {}
        fn armed_units(&self) -> Vec<String> {
            Vec::new()
        }
        async fn changes(&self) -> Vec<PathChange> {
            tokio::time::sleep(Duration::from_millis(10)).await;
            Vec::new()
        }
    }

    fn test_shared() -> Arc<EngineShared> {
        EngineShared::new(Box::new(NoopBackend))
    }

    fn temp_watch_file() -> std::path::PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!("sysn-engine-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(format!(
            "watch-{}.txt",
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::write(&file, b"one").unwrap();
        file
    }

    fn insert_instance(shared: &Arc<EngineShared>, unit: &str, specs: Vec<PathSpec>, cfg: PathConfig) {
        let inst = PathInstance {
            state: PathState::Dead,
            config: cfg,
            specs,
            target_unit: "foo.service".to_string(),
            spec_signatures: Vec::new(),
            last_fired_epoch: None,
            n_fired: 0,
            trigger_times: Default::default(),
            last_error: None,
            invocation_id: None,
        };
        shared.registry.write().insert(unit.to_string(), inst);
    }

    fn state_of(shared: &Arc<EngineShared>, unit: &str) -> PathState {
        let reg = shared.registry.read();
        reg.get(unit).map(|i| i.state).unwrap()
    }

    fn n_fired_of(shared: &Arc<EngineShared>, unit: &str) -> u64 {
        let reg = shared.registry.read();
        reg.get(unit).map(|i| i.n_fired).unwrap()
    }

    #[tokio::test]
    async fn changed_spec_triggers_on_signature_change() {
        let file = temp_watch_file();
        let shared = test_shared();
        let spec = PathSpec {
            kind: PathSpecKind::Changed,
            path: file.display().to_string(),
        };
        insert_instance(
            &shared,
            "foo.path",
            vec![spec],
            PathConfig {
                path_changed: vec![file.display().to_string()],
                ..Default::default()
            },
        );
        arm_unit(&shared, "foo.path").unwrap();
        shared.registry.write().get_mut("foo.path").unwrap().state = PathState::Waiting;

        // No-op event with unchanged signature must not trigger.
        process_change(&shared, "foo.path", FsEvent::default());
        assert_eq!(state_of(&shared, "foo.path"), PathState::Waiting);

        // Modify the file; a wake-up with no event bits is enough to trigger
        // via the signature comparison.
        std::fs::write(&file, b"much-longer-content-now").unwrap();
        process_change(&shared, "foo.path", FsEvent::default());
        assert_eq!(state_of(&shared, "foo.path"), PathState::Running);
        assert_eq!(n_fired_of(&shared, "foo.path"), 1);
    }

    #[tokio::test]
    async fn changed_spec_ignores_attrib_only_event_without_change() {
        let file = temp_watch_file();
        let shared = test_shared();
        let spec = PathSpec {
            kind: PathSpecKind::Changed,
            path: file.display().to_string(),
        };
        insert_instance(&shared, "foo.path", vec![spec], PathConfig::default());
        arm_unit(&shared, "foo.path").unwrap();
        shared.registry.write().get_mut("foo.path").unwrap().state = PathState::Waiting;

        // An attrib event (chmod) that does not alter the content signature.
        process_change(&shared, "foo.path", FsEvent { attrib: true, ..Default::default() });
        assert_eq!(state_of(&shared, "foo.path"), PathState::Waiting);
    }

    #[tokio::test]
    async fn modified_spec_triggers_on_write_event() {
        let file = temp_watch_file();
        let shared = test_shared();
        let spec = PathSpec {
            kind: PathSpecKind::Modified,
            path: file.display().to_string(),
        };
        insert_instance(
            &shared,
            "foo.path",
            vec![spec],
            PathConfig {
                path_modified: vec![file.display().to_string()],
                ..Default::default()
            },
        );
        arm_unit(&shared, "foo.path").unwrap();
        shared.registry.write().get_mut("foo.path").unwrap().state = PathState::Waiting;

        // A bare in-place write event satisfies PathModified but not
        // PathChanged.
        process_change(
            &shared,
            "foo.path",
            FsEvent { written: true, ..Default::default() },
        );
        assert_eq!(state_of(&shared, "foo.path"), PathState::Running);
    }

    #[tokio::test]
    async fn rate_limit_exhaustion_fails_unit() {
        let file = temp_watch_file();
        let shared = test_shared();
        let spec = PathSpec {
            kind: PathSpecKind::Exists,
            path: file.display().to_string(),
        };
        insert_instance(
            &shared,
            "foo.path",
            vec![spec],
            PathConfig {
                path_exists: vec![file.display().to_string()],
                trigger_limit_interval_sec: 60,
                trigger_limit_burst: 1,
                ..Default::default()
            },
        );
        arm_unit(&shared, "foo.path").unwrap();
        shared.registry.write().get_mut("foo.path").unwrap().state = PathState::Waiting;

        // First trigger passes the rate limiter.
        recheck_level(&shared, "foo.path");
        assert_eq!(state_of(&shared, "foo.path"), PathState::Running);

        // Back to waiting; a second trigger within the window is blocked and
        // moves the unit to failed.
        shared.registry.write().get_mut("foo.path").unwrap().state = PathState::Waiting;
        recheck_level(&shared, "foo.path");
        assert_eq!(state_of(&shared, "foo.path"), PathState::Failed);
        let reg = shared.registry.read();
        assert!(reg.get("foo.path").unwrap().last_error.as_ref().unwrap().contains("rate limit"));
    }

    #[tokio::test]
    async fn exists_refires_after_target_inactive() {
        let file = temp_watch_file();
        let shared = test_shared();
        let spec = PathSpec {
            kind: PathSpecKind::Exists,
            path: file.display().to_string(),
        };
        insert_instance(
            &shared,
            "foo.path",
            vec![spec],
            PathConfig {
                path_exists: vec![file.display().to_string()],
                ..Default::default()
            },
        );
        arm_unit(&shared, "foo.path").unwrap();
        shared.registry.write().get_mut("foo.path").unwrap().state = PathState::Waiting;

        // Condition already true on start → immediate trigger.
        recheck_level(&shared, "foo.path");
        assert_eq!(state_of(&shared, "foo.path"), PathState::Running);
        assert_eq!(n_fired_of(&shared, "foo.path"), 1);

        // Target terminates → unit re-arms and, because the file still
        // exists, immediately re-fires (systemd behaviour).
        on_target_inactive(&shared, "foo.path");
        assert_eq!(state_of(&shared, "foo.path"), PathState::Running);
        assert_eq!(n_fired_of(&shared, "foo.path"), 2);
    }
}
