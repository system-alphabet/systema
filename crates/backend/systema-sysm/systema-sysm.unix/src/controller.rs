use crate::mount::{do_mount, do_remount, do_umount};
use crate::state::{
    AutomountInstance, AutomountRegistry, AutomountState, MountRegistry, MountState,
};
use anyhow::Result;
use std::collections::HashMap;
use sysa::controller::{decode_unit_config, UnitController, UnitStatus};
use sysa::worker_ipc::EventPublisher;
use tracing::info;

#[derive(Clone)]
pub struct MountController {
    registry: MountRegistry,
    automount_registry: AutomountRegistry,
    event_pub: EventPublisher,
}

impl MountController {
    pub fn new(
        registry: MountRegistry,
        automount_registry: AutomountRegistry,
        event_pub: EventPublisher,
    ) -> Self {
        MountController {
            registry,
            automount_registry,
            event_pub,
        }
    }

    fn status_of(&self, unit_name: &str) -> UnitStatus {
        if let Some(inst) = self.registry.lock().get(unit_name) {
            return build_mount_status(unit_name, &inst.state);
        }
        if let Some(inst) = self.automount_registry.lock().get(unit_name) {
            return build_automount_status(unit_name, &inst.state, inst.last_error.as_deref());
        }
        // Unknown unit: synthesize status from unit name as fallback.
        UnitStatus {
            unit_name: unit_name.to_string(),
            active_state: "inactive".to_string(),
            sub_state: "dead".to_string(),
            main_pid: 0,
            invocation_id: String::new(),
            extensions: HashMap::new(),
        }
    }

    fn publish_state(&self, unit_name: &str) {
        let status = self.status_of(unit_name);
        self.event_pub
            .publish_unit_state_update(vec![status], false);
    }

    fn publish_automount_state(&self, unit_name: &str) {
        self.publish_state(unit_name);
    }
}

/// Derive the companion `.mount` unit name from an `.automount` unit name
/// (`foo.automount` → `foo.mount`).
fn companion_mount_unit(unit_name: &str) -> String {
    match unit_name.strip_suffix(".automount") {
        Some(base) => format!("{base}.mount"),
        None => format!("{unit_name}.mount"),
    }
}

/// Determine the unit type by checking the registries, falling back to the
/// unit name suffix.
fn resolve_unit_type(
    mount_registry: &MountRegistry,
    automount_registry: &AutomountRegistry,
    unit_name: &str,
) -> &'static str {
    if mount_registry.lock().contains_key(unit_name) {
        "mount"
    } else if automount_registry.lock().contains_key(unit_name) || unit_name.ends_with(".automount")
    {
        "automount"
    } else {
        "mount"
    }
}

fn build_mount_status(unit_name: &str, state: &MountState) -> UnitStatus {
    let (active_state, sub_state) = match state {
        MountState::Dead => ("inactive", "dead"),
        MountState::Mounting => ("activating", "mounting"),
        MountState::Mounted => ("active", "mounted"),
        MountState::Unmounting => ("deactivating", "unmounting"),
        MountState::Failed => ("failed", "failed"),
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

fn build_automount_status(
    unit_name: &str,
    state: &AutomountState,
    last_error: Option<&str>,
) -> UnitStatus {
    let (active_state, sub_state) = match state {
        AutomountState::Dead => ("inactive", "dead"),
        AutomountState::Waiting => ("active", "waiting"),
        AutomountState::Running => ("active", "running"),
    };
    let mut extensions = HashMap::new();
    if let Some(err) = last_error {
        extensions.insert("last_error".to_string(), err.to_string());
    }
    UnitStatus {
        unit_name: unit_name.to_string(),
        active_state: active_state.to_string(),
        sub_state: sub_state.to_string(),
        main_pid: 0,
        invocation_id: String::new(),
        extensions,
    }
}

#[async_trait::async_trait]
impl UnitController for MountController {
    async fn status(&self, unit_name: &str) -> Result<UnitStatus> {
        Ok(self.status_of(unit_name))
    }

    async fn sync_state(&self) -> Vec<UnitStatus> {
        // Best-effort: bring the registry in line with the real mount table
        // before reporting (filesystems already mounted at boot, external
        // mounts/unmounts missed between ticks).  The background monitor
        // commits the corresponding UnitIRs to System A.
        crate::mounttable::refresh_registry(&self.registry);
        let mut units = Vec::new();
        {
            let names: Vec<String> = {
                let guard = self.registry.lock();
                guard.keys().cloned().collect()
            };
            units.extend(names.iter().map(|n| self.status_of(n)));
        }
        {
            let names: Vec<String> = {
                let guard = self.automount_registry.lock();
                guard.keys().cloned().collect()
            };
            units.extend(names.iter().map(|n| self.status_of(n)));
        }
        units
    }

    async fn start(&self, unit_name: &str, config: &[u8], _invocation_id: &str) -> Result<()> {
        let cfg = decode_unit_config(config)?;
        match resolve_unit_type(&self.registry, &self.automount_registry, unit_name) {
            "mount" => {
                let mount_cfg = cfg.mount.as_ref().ok_or_else(|| {
                    anyhow::anyhow!(sysa::l10n::fmt(
                        sysa::l10n::t_("No MountConfig for {unit_name}"),
                        &[("unit_name", &unit_name.to_string())]
                    ))
                })?;
                do_mount(self.registry.clone(), unit_name, mount_cfg).await?;
                self.publish_state(unit_name);
            }
            "automount" => {
                let auto_cfg = cfg.automount.as_ref().ok_or_else(|| {
                    anyhow::anyhow!(sysa::l10n::fmt(
                        sysa::l10n::t_("No AutomountConfig for {unit_name}"),
                        &[("unit_name", &unit_name.to_string())]
                    ))
                })?;
                let mount_cfg = cfg.mount.as_ref().ok_or_else(|| {
                    anyhow::anyhow!(sysa::l10n::fmt(
                        sysa::l10n::t_("No companion MountConfig for {unit_name}"),
                        &[("unit_name", &unit_name.to_string())]
                    ))
                })?;
                let companion = companion_mount_unit(unit_name);
                // Eager-mount semantics (no autofs on Unix): mount the
                // companion device immediately.  TimeoutIdleSec is
                // intentionally ignored.
                info!(
                    "Automount {} at {} (eager mount, TimeoutIdleSec ignored)",
                    unit_name, auto_cfg.r#where
                );
                {
                    let mut reg = self.automount_registry.lock();
                    reg.insert(
                        unit_name.to_string(),
                        AutomountInstance {
                            state: AutomountState::Running,
                            mount_config: Some(mount_cfg.clone()),
                            last_error: None,
                        },
                    );
                }
                if let Err(e) = do_mount(self.registry.clone(), &companion, mount_cfg).await {
                    let mut reg = self.automount_registry.lock();
                    if let Some(inst) = reg.get_mut(unit_name) {
                        inst.state = AutomountState::Dead;
                        inst.last_error = Some(e.to_string());
                    }
                    drop(reg);
                    self.publish_automount_state(unit_name);
                    return Err(e);
                }
                self.publish_state(&companion);
                self.publish_automount_state(unit_name);
            }
            _ => anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_("Unknown unit type for {unit_name}"),
                &[("unit_name", &unit_name.to_string())]
            )),
        }
        Ok(())
    }

    async fn stop(&self, unit_name: &str) -> Result<()> {
        match resolve_unit_type(&self.registry, &self.automount_registry, unit_name) {
            "mount" => {
                do_umount(self.registry.clone(), unit_name, None).await?;
                self.publish_state(unit_name);
            }
            "automount" => {
                // If the companion real fs is mounted, unmount it first
                // (fails if busy, leaving the automount Running), then mark
                // Dead.
                let (partner_mounted, mount_cfg) = {
                    let reg = self.automount_registry.lock();
                    let cfg = reg.get(unit_name).and_then(|inst| inst.mount_config.clone());
                    let mounted = {
                        let mreg = self.registry.lock();
                        let partner = companion_mount_unit(unit_name);
                        mreg.get(&partner)
                            .map(|inst| inst.state != MountState::Dead)
                            .unwrap_or(false)
                    };
                    (mounted, cfg)
                };
                if partner_mounted {
                    let partner = companion_mount_unit(unit_name);
                    do_umount(self.registry.clone(), &partner, mount_cfg.as_ref()).await?;
                    self.publish_state(&partner);
                }
                {
                    let mut reg = self.automount_registry.lock();
                    if let Some(inst) = reg.get_mut(unit_name) {
                        inst.state = AutomountState::Dead;
                        inst.last_error = None;
                    }
                }
                self.publish_automount_state(unit_name);
            }
            _ => anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_("Unknown unit type for {unit_name}"),
                &[("unit_name", &unit_name.to_string())]
            )),
        }
        Ok(())
    }

    async fn restart(&self, unit_name: &str, config: &[u8], invocation_id: &str) -> Result<()> {
        self.stop(unit_name).await?;
        self.start(unit_name, config, invocation_id).await?;
        Ok(())
    }

    async fn reload(&self, unit_name: &str, config: &[u8]) -> Result<()> {
        let cfg = decode_unit_config(config)?;
        let mount_cfg = cfg.mount.as_ref().ok_or_else(|| {
            anyhow::anyhow!(sysa::l10n::fmt(
                sysa::l10n::t_("No MountConfig for {unit_name}"),
                &[("unit_name", &unit_name.to_string())]
            ))
        })?;
        do_remount(self.registry.clone(), unit_name, mount_cfg).await?;
        self.publish_state(unit_name);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use parking_lot::Mutex;

    use crate::state::{AutomountInstance, AutomountState, MountInstance};

    use super::*;

    #[test]
    fn companion_unit_name_derivation() {
        assert_eq!(companion_mount_unit("foo.automount"), "foo.mount");
        assert_eq!(companion_mount_unit("foo.mount"), "foo.mount.mount");
        assert_eq!(companion_mount_unit("root.automount"), "root.mount");
    }

    #[test]
    fn unit_type_resolution() {
        let mreg: MountRegistry = Arc::new(Mutex::new(HashMap::new()));
        let areg: AutomountRegistry = Arc::new(Mutex::new(HashMap::new()));

        assert_eq!(resolve_unit_type(&mreg, &areg, "x.mount"), "mount");
        assert_eq!(resolve_unit_type(&mreg, &areg, "x.automount"), "automount");

        mreg.lock().insert(
            "y.mount".to_string(),
            MountInstance::new("y.mount".to_string(), "/y".to_string(), "dev".to_string()),
        );
        assert_eq!(resolve_unit_type(&mreg, &areg, "y.mount"), "mount");

        areg.lock().insert(
            "z.automount".to_string(),
            AutomountInstance {
                state: AutomountState::Dead,
                mount_config: None,
                last_error: None,
            },
        );
        assert_eq!(resolve_unit_type(&mreg, &areg, "z.automount"), "automount");
        // Registry entries take precedence over the suffix.
        areg.lock().insert(
            "w.mount".to_string(),
            AutomountInstance {
                state: AutomountState::Dead,
                mount_config: None,
                last_error: None,
            },
        );
        assert_eq!(resolve_unit_type(&mreg, &areg, "w.mount"), "automount");
    }
}
