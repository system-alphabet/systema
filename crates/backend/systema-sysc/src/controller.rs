use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use sysa::controller::{decode_unit_config, UnitController, UnitStatus};
use tracing::info;

use crate::engine::{publish_timer, send_fired, status_of, EngineShared};
use crate::schedule::{compute_next_elapse, default_target_unit, epoch_now, missed_calendar_elapse};
use crate::state::{TimerInstance, TimerState};

/// Timer controller: owns every `.timer` unit registered with System C.
pub struct TimerController {
    shared: Arc<EngineShared>,
}

impl TimerController {
    pub fn new(shared: Arc<EngineShared>) -> Self {
        TimerController { shared }
    }
}

#[async_trait]
impl UnitController for TimerController {
    async fn status(&self, unit_name: &str) -> Result<UnitStatus> {
        let reg = self.shared.registry.read();
        let inst = reg.get(unit_name).ok_or_else(|| {
            anyhow!(sysa::l10n::fmt(
                sysa::l10n::t_("Unknown timer unit '{unit_name}'"),
                &[("unit_name", &unit_name.to_string())]
            ))
        })?;
        Ok(status_of(unit_name, inst))
    }

    async fn start(&self, unit_name: &str, config: &[u8], invocation_id: &str) -> Result<()> {
        if !unit_name.ends_with(".timer") {
            anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_("'{unit_name}' is not a .timer unit"),
                &[("unit_name", &unit_name.to_string())]
            ));
        }
        let cfg = decode_unit_config(config)?;
        let timer_cfg = cfg.timer.clone().ok_or_else(|| {
            anyhow!(sysa::l10n::fmt(
                sysa::l10n::t_("No TimerConfig for '{unit_name}'"),
                &[("unit_name", &unit_name.to_string())]
            ))
        })?;

        let target_unit = if timer_cfg.unit.is_empty() {
            default_target_unit(unit_name)
        } else {
            timer_cfg.unit.clone()
        };
        let now = epoch_now();

        let has_trigger = !timer_cfg.on_calendar.is_empty()
            || timer_cfg.on_active_sec.is_some()
            || timer_cfg.on_boot_sec.is_some()
            || timer_cfg.on_startup_sec.is_some()
            || timer_cfg.on_unit_active_sec.is_some()
            || timer_cfg.on_unit_inactive_sec.is_some();

        // Persistent=yes: catch up on a calendar elapse missed while the
        // timer was inactive by triggering immediately.
        let missed = missed_calendar_elapse(&timer_cfg, self.shared.started_epoch);

        let mut inst = TimerInstance {
            state: if has_trigger {
                TimerState::Waiting
            } else {
                TimerState::Failed
            },
            config: timer_cfg.clone(),
            target_unit,
            next_elapse: None,
            last_elapse: missed,
            activated_epoch: now,
            n_fired: if missed.is_some() { 1 } else { 0 },
            n_missed: if missed.is_some() { 1 } else { 0 },
            last_error: if has_trigger {
                None
            } else {
                Some(
                    sysa::l10n::t_(
                        "Timer unit has no OnCalendar=/On*Sec= trigger — nothing to schedule",
                    )
                    .to_string(),
                )
            },
            invocation_id: if invocation_id.is_empty() {
                None
            } else {
                Some(invocation_id.to_string())
            },
        };

        if has_trigger {
            inst.next_elapse = compute_next_elapse(
                &inst.config,
                inst.last_elapse,
                epoch_now(),
                self.shared.boot_epoch,
                self.shared.started_epoch,
                inst.activated_epoch,
            )
            .map(|(e, _)| e);
        }

        {
            let mut reg = self.shared.registry.write();
            reg.insert(unit_name.to_string(), inst);
        }

        publish_timer(&self.shared, unit_name);

        if let Some(missed) = missed {
            info!(
                "Persistent timer '{unit_name}' catching up on missed elapse at epoch {missed}"
            );
            let target = self
                .shared
                .registry
                .read()
                .get(unit_name)
                .map(|i| i.target_unit.clone())
                .unwrap_or_default();
            send_fired(&self.shared, unit_name, &target, missed);
        }
        Ok(())
    }

    async fn stop(&self, unit_name: &str) -> Result<()> {
        {
            let mut reg = self.shared.registry.write();
            let inst = reg.get_mut(unit_name).ok_or_else(|| {
                anyhow!(sysa::l10n::fmt(
                    sysa::l10n::t_("Unknown timer unit '{unit_name}'"),
                    &[("unit_name", &unit_name.to_string())]
                ))
            })?;
            inst.state = TimerState::Dead;
            inst.next_elapse = None;
        }
        publish_timer(&self.shared, unit_name);
        Ok(())
    }

    async fn restart(&self, unit_name: &str, config: &[u8], invocation_id: &str) -> Result<()> {
        self.stop(unit_name).await?;
        self.start(unit_name, config, invocation_id).await
    }

    async fn reload(&self, unit_name: &str, config: &[u8]) -> Result<()> {
        let cfg = decode_unit_config(config)?;
        let timer_cfg = cfg.timer.clone().ok_or_else(|| {
            anyhow!(sysa::l10n::fmt(
                sysa::l10n::t_("No TimerConfig for '{unit_name}'"),
                &[("unit_name", &unit_name.to_string())]
            ))
        })?;
        {
            let mut reg = self.shared.registry.write();
            let inst = reg.get_mut(unit_name).ok_or_else(|| {
                anyhow!(sysa::l10n::fmt(
                    sysa::l10n::t_("Unknown timer unit '{unit_name}'"),
                    &[("unit_name", &unit_name.to_string())]
                ))
            })?;
            inst.config = timer_cfg.clone();
            if !timer_cfg.unit.is_empty() {
                inst.target_unit = timer_cfg.unit.clone();
            }
            if inst.state == TimerState::Waiting {
                let now = epoch_now();
                inst.next_elapse = compute_next_elapse(
                    &inst.config,
                    inst.last_elapse,
                    now,
                    self.shared.boot_epoch,
                    self.shared.started_epoch,
                    inst.activated_epoch,
                )
                .map(|(e, _)| e);
                if inst.next_elapse.is_none() {
                    inst.state = TimerState::Elapsed;
                }
            }
        }
        publish_timer(&self.shared, unit_name);
        Ok(())
    }

    async fn sync_state(&self) -> Vec<UnitStatus> {
        let reg = self.shared.registry.read();
        reg.iter().map(|(name, inst)| status_of(name, inst)).collect()
    }
}