use std::collections::HashMap;

use anyhow::Result;
use sysa::controller::{UnitController, UnitStatus};

use crate::state::{PlaceholderInstance, PlaceholderRegistry, StateMode, UnitState};

#[derive(Clone)]
pub struct PlaceholderController {
    registry: PlaceholderRegistry,
    state_mode: StateMode,
    default_state: UnitState,
}

impl PlaceholderController {
    pub fn new(
        registry: PlaceholderRegistry,
        state_mode: StateMode,
        default_state: UnitState,
    ) -> Self {
        Self {
            registry,
            state_mode,
            default_state,
        }
    }

    fn build_status(&self, unit_name: &str, state: UnitState) -> UnitStatus {
        let (active_state, sub_state) = match state {
            UnitState::Active => ("active", "active"),
            UnitState::Inactive => ("inactive", "dead"),
        };
        UnitStatus {
            unit_name: unit_name.to_string(),
            active_state: active_state.to_string(),
            sub_state: sub_state.to_string(),
            main_pid: 0,
            invocation_id: String::new(),
            extensions: HashMap::new(),
        }
    }

    fn set_action_state(&self, unit_name: &str, starting: bool) {
        let state = self.state_mode.state_after_action(starting);
        let mut registry = self.registry.lock();
        registry
            .entry(unit_name.to_string())
            .or_insert_with(|| PlaceholderInstance::new(self.default_state))
            .state = state;
    }
}

#[async_trait::async_trait]
impl UnitController for PlaceholderController {
    async fn status(&self, unit_name: &str) -> Result<UnitStatus> {
        let registry = self.registry.lock();
        let state = registry
            .get(unit_name)
            .map(|instance| instance.state)
            .unwrap_or(self.default_state);
        Ok(self.build_status(unit_name, state))
    }

    async fn start(&self, unit_name: &str, _config: &[u8], _invocation_id: &str) -> Result<()> {
        self.set_action_state(unit_name, true);
        Ok(())
    }

    async fn stop(&self, unit_name: &str) -> Result<()> {
        self.set_action_state(unit_name, false);
        Ok(())
    }

    async fn restart(&self, unit_name: &str, config: &[u8], invocation_id: &str) -> Result<()> {
        self.start(unit_name, config, invocation_id).await
    }

    async fn reload(&self, _unit_name: &str, _config: &[u8]) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::PlaceholderController;
    use crate::state::{new_registry, StateMode, UnitState};
    use sysa::controller::UnitController;

    #[tokio::test]
    async fn status_uses_configured_default_until_an_action_occurs() {
        let controller =
            PlaceholderController::new(new_registry(), StateMode::Follow, UnitState::Active);

        assert_eq!(
            controller
                .status("example.service")
                .await
                .unwrap()
                .active_state,
            "active"
        );
        controller.stop("example.service").await.unwrap();
        assert_eq!(
            controller
                .status("example.service")
                .await
                .unwrap()
                .active_state,
            "inactive"
        );
    }
}
