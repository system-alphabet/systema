use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use sysa::proto::TimerConfig;

/// Runtime state of a `.timer` unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimerState {
    Dead,
    Waiting,
    Running,
    Elapsed,
    Failed,
}

impl TimerState {
    /// Sub-state used in `UnitStatus.sub_state`.
    pub fn as_str(&self) -> &'static str {
        match self {
            TimerState::Dead => "dead",
            TimerState::Waiting => "waiting",
            TimerState::Running => "running",
            TimerState::Elapsed => "elapsed",
            TimerState::Failed => "failed",
        }
    }

    /// Systemd-style active state used in `UnitStatus.active_state`.
    pub fn active_state(&self) -> &'static str {
        match self {
            TimerState::Waiting | TimerState::Running | TimerState::Elapsed => "active",
            TimerState::Dead => "inactive",
            TimerState::Failed => "failed",
        }
    }
}

/// One `.timer` unit instance managed by System C.
pub struct TimerInstance {
    pub state: TimerState,
    /// Active schedule configuration.
    pub config: TimerConfig,
    /// Unit activated when this timer fires (defaults to the sibling
    /// `.service` with the same base name).
    pub target_unit: String,
    /// Next elapse (unix epoch seconds), None when no schedule remains.
    pub next_elapse: Option<u64>,
    /// Most recent elapse (unix epoch seconds).
    pub last_elapse: Option<u64>,
    /// Epoch at which the timer unit was activated (`OnActiveSec=` anchor).
    pub activated_epoch: u64,
    /// Number of elapses fired.
    pub n_fired: u64,
    /// Number of elapses caught up via `Persistent=`.
    pub n_missed: u64,
    /// Failure message of the last (re)arm attempt.
    pub last_error: Option<String>,
    /// Invocation ID of the current activation.
    pub invocation_id: Option<String>,
}

pub type TimerRegistry = Arc<RwLock<HashMap<String, TimerInstance>>>;

pub fn new_registry() -> TimerRegistry {
    Arc::new(RwLock::new(HashMap::new()))
}