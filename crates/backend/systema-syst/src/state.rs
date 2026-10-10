//! Target state machine for System T.
//!
//! Targets are simpler than services — they have no external processes.
//! The state machine is: DEAD ↔ ACTIVE.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;

/// The lifecycle state of a managed target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetState {
    Dead,
    Active,
}

/// Runtime state for a single target instance.
pub struct TargetInstance {
    #[allow(dead_code)]
    pub unit_name: String,
    pub state: TargetState,
}

impl TargetInstance {
    pub fn new(unit_name: String) -> Self {
        TargetInstance {
            unit_name,
            state: TargetState::Dead,
        }
    }
}

/// Global target instance registry.
pub type TargetRegistry = Arc<Mutex<HashMap<String, TargetInstance>>>;

pub fn new_registry() -> TargetRegistry {
    Arc::new(Mutex::new(HashMap::new()))
}
