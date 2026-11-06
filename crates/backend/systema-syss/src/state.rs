//! Service state machine for System S.
//!
//! Each managed service has its own `ServiceStateMachine` that transitions
//! through the states: DEAD → START_PRE → STARTING → RUNNING → STOP_PRE →
//! STOPPING → DEAD | FAILED.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use parking_lot::Mutex;

/// The lifecycle state of a managed service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    Dead,
    Starting,
    Running,
    Stopping,
    Failed,
}

impl ServiceState {
    pub fn as_str(&self) -> &str {
        match self {
            ServiceState::Dead => "dead",
            ServiceState::Starting => "start",
            ServiceState::Running => "running",
            ServiceState::Stopping => "stop",
            ServiceState::Failed => "failed",
        }
    }
}

/// Runtime state for a single service instance.
pub struct ServiceInstance {
    pub state: ServiceState,
    pub main_pid: Option<u32>,
    pub last_exit_code: Option<i32>,
    /// Stop timeout in seconds (from ServiceConfig).
    pub timeout_stop_secs: Option<u32>,
    /// RemainAfterExit=yes: the service stays "active" after its main
    /// process exits successfully (oneshot style) instead of going
    /// inactive.
    pub remain_after_exit: bool,
    /// `ExecStart=-...`: the main process's non-zero exit status is still
    /// recorded in `last_exit_code`, but does not fail the unit.  Set from
    /// the parsed `-` prefix on every activation, so a restart cannot
    /// inherit a stale verdict.
    pub ignore_failure: bool,
    /// PIDs of `ExecStartPre=`/`ExecStartPost=` helper processes the worker
    /// is currently waiting for.  They are not the unit's main process, but
    /// the orphan reaper must leave their exit status to the waiter (it
    /// reaps every child that is absent from the tracked set).
    pub transient_pids: HashSet<u32>,
    /// Invocation ID (UUID v4) of the current activation, as provided by
    /// System A.  Cleared when the service exits or is stopped.
    pub invocation_id: Option<String>,
    /// Absolute cgroup path the service runs in, learned once from a
    /// `unit.cgroup` lookup and reused for later stops (the path is stable
    /// for a unit).  `None` on platforms without a cgroup backend or while
    /// the lookup has not succeeded yet — killing then falls back to the
    /// main process group.
    pub cgroup_path: Option<String>,
}

impl ServiceInstance {
    pub fn new() -> Self {
        ServiceInstance {
            state: ServiceState::Dead,
            main_pid: None,
            last_exit_code: None,
            timeout_stop_secs: None,
            remain_after_exit: false,
            ignore_failure: false,
            transient_pids: HashSet::new(),
            invocation_id: None,
            cgroup_path: None,
        }
    }

    /// Whether a main-process exit with the given success flag counts as a
    /// successful activation.  `ExecStart=-` turns a non-zero exit into a
    /// success for the unit, exactly like systemd's `ignore_failure`
    /// handling in `service_sigchld_event()`.
    pub fn exit_succeeded(&self, status_success: bool) -> bool {
        status_success || self.ignore_failure
    }
}

impl Default for ServiceInstance {
    fn default() -> Self {
        Self::new()
    }
}

/// Global service instance registry.
pub type ServiceRegistry = Arc<Mutex<HashMap<String, ServiceInstance>>>;

pub fn new_registry() -> ServiceRegistry {
    Arc::new(Mutex::new(HashMap::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_exit_always_succeeds() {
        assert!(ServiceInstance::new().exit_succeeded(true));
    }

    #[test]
    fn non_zero_exit_fails_by_default() {
        assert!(!ServiceInstance::new().exit_succeeded(false));
    }

    #[test]
    fn ignored_failure_turns_non_zero_exit_into_success() {
        let mut inst = ServiceInstance::new();
        inst.ignore_failure = true;
        assert!(inst.exit_succeeded(false));
    }
}
