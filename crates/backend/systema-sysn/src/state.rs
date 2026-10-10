//! Runtime state of `.path` units.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use parking_lot::RwLock;
use sysa::proto::PathConfig;

use crate::spec::{PathSignature, PathSpec};

/// State of a `.path` unit (mirrors systemd's path unit lifecycle).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathState {
    /// Not active.
    Dead,
    /// Armed and watching the filesystem for its conditions.
    Waiting,
    /// A condition fired and the target unit is being activated.
    Running,
    /// Armed but unable to trigger (e.g. trigger rate limit exceeded).
    Failed,
}

impl PathState {
    /// Sub-state used in `UnitStatus.sub_state`.
    pub fn as_str(&self) -> &'static str {
        match self {
            PathState::Dead => "dead",
            PathState::Waiting => "waiting",
            PathState::Running => "running",
            PathState::Failed => "failed",
        }
    }

    /// Systemd-style active state used in `UnitStatus.active_state`.
    pub fn active_state(&self) -> &'static str {
        match self {
            PathState::Waiting | PathState::Running => "active",
            PathState::Dead => "inactive",
            PathState::Failed => "failed",
        }
    }
}

/// One `.path` unit instance managed by System N.
pub struct PathInstance {
    pub state: PathState,
    /// Active configuration from System A.
    pub config: PathConfig,
    /// Watch conditions derived from the configuration.
    pub specs: Vec<PathSpec>,
    /// Unit activated when a condition fires (defaults to the sibling
    /// `.service` with the same base name).
    pub target_unit: String,
    /// stat() signatures recorded at arm time, aligned with `specs`.
    /// Edge-triggered specs compare against these to detect changes.
    pub spec_signatures: Vec<Option<PathSignature>>,
    /// Epoch of the most recent firing.
    pub last_fired_epoch: Option<u64>,
    /// Number of times the unit has fired.
    pub n_fired: u64,
    /// Recent firing epochs for trigger rate limiting (sliding window).
    pub trigger_times: VecDeque<u64>,
    /// Failure message of the last (re)arm or trigger attempt.
    pub last_error: Option<String>,
    /// Invocation ID of the current activation.
    pub invocation_id: Option<String>,
}

pub type PathRegistry = Arc<RwLock<HashMap<String, PathInstance>>>;

pub fn new_registry() -> PathRegistry {
    Arc::new(RwLock::new(HashMap::new()))
}
