//! Runtime state policy for System P.

use std::collections::HashMap;
use std::sync::Arc;

use clap::ValueEnum;
use parking_lot::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum StateMode {
    Follow,
    Active,
    Inactive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum UnitState {
    Active,
    Inactive,
}

impl StateMode {
    pub fn state_after_action(self, starting: bool) -> UnitState {
        match self {
            Self::Follow if starting => UnitState::Active,
            Self::Follow => UnitState::Inactive,
            Self::Active => UnitState::Active,
            Self::Inactive => UnitState::Inactive,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PlaceholderInstance {
    pub state: UnitState,
}

impl PlaceholderInstance {
    pub fn new(state: UnitState) -> Self {
        Self { state }
    }
}

pub type PlaceholderRegistry = Arc<Mutex<HashMap<String, PlaceholderInstance>>>;

pub fn new_registry() -> PlaceholderRegistry {
    Arc::new(Mutex::new(HashMap::new()))
}

#[cfg(test)]
mod tests {
    use super::{StateMode, UnitState};

    #[test]
    fn state_modes_resolve_start_and_stop_as_configured() {
        assert_eq!(
            StateMode::Follow.state_after_action(true),
            UnitState::Active
        );
        assert_eq!(
            StateMode::Follow.state_after_action(false),
            UnitState::Inactive
        );
        for mode in [StateMode::Active, StateMode::Inactive] {
            assert_eq!(
                mode.state_after_action(true),
                mode.state_after_action(false)
            );
        }
        assert_eq!(
            StateMode::Active.state_after_action(false),
            UnitState::Active
        );
        assert_eq!(
            StateMode::Inactive.state_after_action(true),
            UnitState::Inactive
        );
    }
}
