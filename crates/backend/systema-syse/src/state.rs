//! Scope state machine for System E.
//!
//! Each managed scope has its own `ScopeInstance` that transitions through
//! the states: DEAD → STARTING → RUNNING → (ABANDONED) | STOPPING → DEAD |
//! FAILED, mirroring systemd's `ScopeState` (`src/core/scope.h`).

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;

/// The lifecycle state of a managed scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // `Failed` mirrors systemd's SCOPE_FAILED for completeness.
pub enum ScopeState {
    Dead,
    Starting,
    Running,
    /// The scope's controller gave up on it: the wrapped processes keep
    /// running but the scope will not be stopped or killed by us anymore.
    Abandoned,
    Stopping,
    Failed,
}

impl ScopeState {
    /// systemd `sub_state` strings (`scope_state_to_string`).
    pub fn as_str(&self) -> &str {
        match self {
            ScopeState::Dead => "dead",
            ScopeState::Starting => "start",
            ScopeState::Running => "running",
            ScopeState::Abandoned => "abandoned",
            ScopeState::Stopping => "stop-sigterm",
            ScopeState::Failed => "failed",
        }
    }
}

/// Runtime state for a single scope instance.
pub struct ScopeInstance {
    pub state: ScopeState,
    /// The PIDs wrapped by the scope (from the transient `PIDs=` property).
    pub pids: Vec<u32>,
    /// Stop timeout in seconds (from ScopeConfig).
    pub timeout_stop_secs: u32,
    /// Maximum runtime in seconds (0 = unlimited).
    pub runtime_max_secs: u32,
    /// Signal to send when stopping (`KillSignal=`, empty = SIGTERM).
    pub kill_signal: String,
    /// Whether to send SIGHUP before SIGKILL (`SendSIGHUP=`).
    pub send_sighup: bool,
    /// Parent slice cgroup path for the scope (computed on start).
    pub cgroup_path: Option<String>,
    /// Invocation ID (UUID v4) of the current activation, as provided by
    /// System A.  Cleared when the scope dies.
    pub invocation_id: Option<String>,
}

impl ScopeInstance {
    pub fn new() -> Self {
        ScopeInstance {
            state: ScopeState::Dead,
            pids: Vec::new(),
            timeout_stop_secs: 90,
            runtime_max_secs: 0,
            kill_signal: String::new(),
            send_sighup: false,
            cgroup_path: None,
            invocation_id: None,
        }
    }
}

impl Default for ScopeInstance {
    fn default() -> Self {
        Self::new()
    }
}

/// Global scope instance registry.
pub type ScopeRegistry = Arc<Mutex<HashMap<String, ScopeInstance>>>;

pub fn new_registry() -> ScopeRegistry {
    Arc::new(Mutex::new(HashMap::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_strings_match_systemd() {
        assert_eq!(ScopeState::Dead.as_str(), "dead");
        assert_eq!(ScopeState::Starting.as_str(), "start");
        assert_eq!(ScopeState::Running.as_str(), "running");
        assert_eq!(ScopeState::Abandoned.as_str(), "abandoned");
        assert_eq!(ScopeState::Stopping.as_str(), "stop-sigterm");
        assert_eq!(ScopeState::Failed.as_str(), "failed");
    }
}
