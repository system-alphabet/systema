//! Service state machine for System S.
//!
//! Each managed service has its own `ServiceStateMachine` that transitions
//! through the states: DEAD → START_PRE → STARTING → RUNNING → STOP_PRE →
//! STOPPING → DEAD | FAILED.

use std::collections::HashMap;
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
    /// Invocation ID (UUID v4) of the current activation, as provided by
    /// System A.  Cleared when the service exits or is stopped.
    pub invocation_id: Option<String>,
}

impl ServiceInstance {
    pub fn new() -> Self {
        ServiceInstance {
            state: ServiceState::Dead,
            main_pid: None,
            last_exit_code: None,
            timeout_stop_secs: None,
            remain_after_exit: false,
            invocation_id: None,
        }
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
