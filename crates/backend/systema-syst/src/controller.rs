use crate::state::{TargetRegistry, TargetState};
use anyhow::Result;
use std::collections::HashMap;
use sysa::controller::{UnitController, UnitStatus};

#[derive(Clone)]
pub struct TargetController {
    registry: TargetRegistry,
}

impl TargetController {
    pub fn new(registry: TargetRegistry) -> Self {
        TargetController { registry }
    }
}

#[async_trait::async_trait]
impl UnitController for TargetController {
    async fn status(&self, unit_name: &str) -> Result<UnitStatus> {
        let reg = self.registry.lock();
        match reg.get(unit_name) {
            Some(inst) => {
                let (active_state, sub_state) = match inst.state {
                    TargetState::Dead => ("inactive", "dead"),
                    TargetState::Active => ("active", "active"),
                };
                Ok(UnitStatus {
                    unit_name: unit_name.to_string(),
                    active_state: active_state.to_string(),
                    sub_state: sub_state.to_string(),
                    main_pid: 0,
                    invocation_id: String::new(),
                    extensions: HashMap::new(),
                })
            }
            None => Ok(UnitStatus {
                unit_name: unit_name.to_string(),
                active_state: "inactive".to_string(),
                sub_state: "dead".to_string(),
                main_pid: 0,
                invocation_id: String::new(),
                extensions: HashMap::new(),
            }),
        }
    }

    async fn start(&self, unit_name: &str, _config: &[u8], _invocation_id: &str) -> Result<()> {
        let mut reg = self.registry.lock();
        let inst = reg
            .entry(unit_name.to_string())
            .or_insert_with(|| crate::state::TargetInstance::new(unit_name.to_string()));
        inst.state = TargetState::Active;
        Ok(())
    }

    async fn stop(&self, unit_name: &str) -> Result<()> {
        let mut reg = self.registry.lock();
        if let Some(inst) = reg.get_mut(unit_name) {
            inst.state = TargetState::Dead;
        }
        Ok(())
    }

    async fn restart(&self, unit_name: &str, config: &[u8], invocation_id: &str) -> Result<()> {
        self.start(unit_name, config, invocation_id).await
    }

    async fn reload(&self, _unit_name: &str, _config: &[u8]) -> Result<()> {
        Ok(())
    }
}
