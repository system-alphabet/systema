//! Unit controller for `.device` units.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use sysa::controller::{decode_unit_config, UnitController, UnitStatus};
use sysa::proto::DeviceConfig;

use crate::discovery::enumerate_devices;
use crate::engine::{publish_unit, status_of, EngineShared};
use crate::matchrule::{device_matches, has_rules};
use crate::naming::node_from_unit_name;
use crate::state::{DeviceInstance, DeviceState};

/// Device controller: owns every `.device` unit registered with System D.
pub struct DeviceController {
    shared: Arc<EngineShared>,
}

impl DeviceController {
    pub fn new(shared: Arc<EngineShared>) -> Self {
        DeviceController { shared }
    }

    /// Find the real device a unit should bind to right now.
    /// Precedence: canonical name match, then explicit match rules.
    fn resolve_unit(&self, unit_name: &str, cfg: &DeviceConfig) -> Option<crate::discovery::DeviceMeta> {
        let devices = enumerate_devices();
        if let Some(node) = node_from_unit_name(unit_name) {
            if let Some(dev) = devices.iter().find(|d| d.node == node) {
                return Some(dev.clone());
            }
        }
        if has_rules(cfg) {
            return devices.into_iter().find(|d| device_matches(cfg, d));
        }
        None
    }

    /// Persist an instance and publish it. `present` decides Active/Inactive.
    fn commit_instance(
        &self,
        unit_name: &str,
        cfg: Option<DeviceConfig>,
        dev: Option<crate::discovery::DeviceMeta>,
        invocation_id: Option<String>,
        last_error: Option<String>,
    ) {
        let state = if dev.is_some() {
            DeviceState::Active
        } else {
            DeviceState::Inactive
        };
        {
            let mut reg = self.shared.registry.write();
            let inst = reg.entry(unit_name.to_string()).or_insert_with(|| {
                DeviceInstance {
                    state: DeviceState::Dead,
                    config: None,
                    dev: None,
                    invocation_id: None,
                    last_error: None,
                }
            });
            inst.state = state;
            inst.config = cfg;
            inst.dev = dev;
            inst.invocation_id = invocation_id;
            inst.last_error = last_error;
        }
        publish_unit(&self.shared, unit_name);
    }
}

#[async_trait]
impl UnitController for DeviceController {
    async fn status(&self, unit_name: &str) -> Result<UnitStatus> {
        let reg = self.shared.registry.read();
        match reg.get(unit_name) {
            Some(inst) => Ok(status_of(unit_name, inst)),
            // A unit System A knows about but that has not been activated or
            // discovered yet — report it as not loaded.
            None => Ok(UnitStatus {
                unit_name: unit_name.to_string(),
                active_state: "inactive".to_string(),
                sub_state: "unloaded".to_string(),
                ..Default::default()
            }),
        }
    }

    async fn start(&self, unit_name: &str, config: &[u8], invocation_id: &str) -> Result<()> {
        if !unit_name.ends_with(".device") {
            anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_("'{unit_name}' is not a .device unit"),
                &[("unit_name", &unit_name.to_string())]
            ));
        }
        let cfg = decode_unit_config(config)?;
        let device_cfg = cfg.device.clone().ok_or_else(|| {
            anyhow!(sysa::l10n::fmt(
                sysa::l10n::t_("No DeviceConfig for '{unit_name}'"),
                &[("unit_name", &unit_name.to_string())]
            ))
        })?;

        let dev = self.resolve_unit(unit_name, &device_cfg);
        let Some(dev) = dev else {
            // Remember the request (unit exists, device just not there yet);
            // the engine will activate it when the hardware appears.
            self.commit_instance(
                unit_name,
                Some(device_cfg),
                None,
                (!invocation_id.is_empty()).then(|| invocation_id.to_string()),
                Some(sysa::l10n::t_("Device is not present").to_string()),
            );
            anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_("Device for '{unit_name}' is not present"),
                &[("unit_name", &unit_name.to_string())]
            ));
        };

        self.commit_instance(
            unit_name,
            Some(device_cfg),
            Some(dev),
            (!invocation_id.is_empty()).then(|| invocation_id.to_string()),
            None,
        );
        Ok(())
    }

    async fn stop(&self, _unit_name: &str) -> Result<()> {
        // Devices cannot be stopped; they disappear when the hardware does.
        anyhow::bail!(sysa::l10n::t_(
            "Operation refused: device units are managed by hardware state"
        ))
    }

    async fn restart(&self, unit_name: &str, config: &[u8], invocation_id: &str) -> Result<()> {
        self.start(unit_name, config, invocation_id).await
    }

    async fn reload(&self, unit_name: &str, config: &[u8]) -> Result<()> {
        if !unit_name.ends_with(".device") {
            anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_("'{unit_name}' is not a .device unit"),
                &[("unit_name", &unit_name.to_string())]
            ));
        }
        let cfg = decode_unit_config(config)?;
        let device_cfg = cfg.device.clone().ok_or_else(|| {
            anyhow!(sysa::l10n::fmt(
                sysa::l10n::t_("No DeviceConfig for '{unit_name}'"),
                &[("unit_name", &unit_name.to_string())]
            ))
        })?;

        let dev = self.resolve_unit(unit_name, &device_cfg);
        self.commit_instance(
            unit_name,
            Some(device_cfg),
            dev,
            None,
            None,
        );
        Ok(())
    }

    async fn sync_state(&self) -> Vec<UnitStatus> {
        let reg = self.shared.registry.read();
        reg.iter().map(|(name, inst)| status_of(name, inst)).collect()
    }
}