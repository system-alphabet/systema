use crate::linux::automount::{
    autofs_send_fail, autofs_send_ready, automount_enter_dead, automount_enter_waiting,
    companion_mount_unit, AutomountTrigger, TriggerEvent,
};
use crate::linux::mount::{do_mount, do_remount, do_umount};
use crate::linux::mountinfo;
use crate::linux::state::{AutomountRegistry, AutomountState, MountRegistry, MountState};
use anyhow::Result;
use std::collections::HashMap;
use sysa::controller::{decode_unit_config, UnitController, UnitStatus};
use sysa::worker_ipc::EventPublisher;
use tokio::sync::mpsc;
use tracing::{debug, warn};

#[derive(Clone)]
pub struct MountController {
    mount_registry: MountRegistry,
    automount_registry: AutomountRegistry,
    event_pub: EventPublisher,
    trigger_tx: mpsc::UnboundedSender<AutomountTrigger>,
}

impl MountController {
    pub fn new(
        mount_registry: MountRegistry,
        automount_registry: AutomountRegistry,
        event_pub: EventPublisher,
    ) -> Self {
        let (trigger_tx, trigger_rx) = mpsc::unbounded_channel::<AutomountTrigger>();
        let controller = MountController {
            mount_registry,
            automount_registry,
            event_pub,
            trigger_tx,
        };
        controller.spawn_trigger_consumer(trigger_rx);
        controller
    }

    /// Consume kernel autofs triggers and handle them entirely inside this
    /// worker (no round-trip to SysA): mount/umount the companion real
    /// filesystem, reply ACK/FAIL to the kernel, publish unified state.
    fn spawn_trigger_consumer(&self, mut trigger_rx: mpsc::UnboundedReceiver<AutomountTrigger>) {
        let mount_registry = self.mount_registry.clone();
        let automount_registry = self.automount_registry.clone();
        let event_pub = self.event_pub.clone();
        tokio::spawn(async move {
            while let Some(trigger) = trigger_rx.recv().await {
                let unit_name = trigger.unit_name.clone();
                if let Err(e) =
                    handle_trigger(&mount_registry, &automount_registry, &event_pub, trigger).await
                {
                    warn!("Trigger handling failed for {}: {}", unit_name, e);
                }
            }
            warn!("Automount trigger consumer finished (channel closed)");
        });
    }

    /// Publish the current runtime state of a mount unit.
    fn publish_mount_state(&self, unit_name: &str) {
        let status = {
            let guard = self.mount_registry.lock();
            guard
                .get(unit_name)
                .map(|inst| build_mount_status(unit_name, &inst.state))
        };
        if let Some(status) = status {
            self.event_pub
                .publish_unit_state_update(vec![status], false);
        }
    }

    /// Publish the current runtime state of an automount unit.
    fn publish_automount_state(&self, unit_name: &str) {
        let status = {
            let guard = self.automount_registry.lock();
            guard.get(unit_name).map(|inst| {
                build_automount_status(unit_name, &inst.state, inst.last_error.as_deref())
            })
        };
        if let Some(status) = status {
            self.event_pub
                .publish_unit_state_update(vec![status], false);
        }
    }
}

/// Handle a single kernel automount trigger.
async fn handle_trigger(
    mount_registry: &MountRegistry,
    automount_registry: &AutomountRegistry,
    event_pub: &EventPublisher,
    trigger: AutomountTrigger,
) -> Result<()> {
    match trigger.event {
        TriggerEvent::MountRequest { token } => {
            handle_mount_request(
                mount_registry,
                automount_registry,
                event_pub,
                &trigger,
                token,
            )
            .await
        }
        TriggerEvent::ExpireRequest { token } => {
            handle_expire_request(
                mount_registry,
                automount_registry,
                event_pub,
                &trigger,
                token,
            )
            .await
        }
    }
}

/// MountRequest: mount the companion real fs, then ACK and return to Waiting.
/// On failure: FAIL reply, stay in Waiting with `last_error` marked so the
/// next access re-triggers.
async fn handle_mount_request(
    mount_registry: &MountRegistry,
    automount_registry: &AutomountRegistry,
    event_pub: &EventPublisher,
    trigger: &AutomountTrigger,
    token: u32,
) -> Result<()> {
    // Stale trigger from a torn-down instance: drop it (do not mount).
    let active = {
        let reg = automount_registry.lock();
        reg.get(&trigger.unit_name)
            .map(|inst| inst.state == AutomountState::Running)
            .unwrap_or(false)
    };
    if !active {
        warn!(
            "Ignoring stale mount request for {} (automount not running)",
            trigger.unit_name
        );
        return Ok(());
    }

    let result = match &trigger.mount_config {
        Some(cfg) => do_mount(mount_registry.clone(), &trigger.mount_unit, cfg).await,
        None => Err(anyhow::anyhow!(sysa::l10n::fmt(
            sysa::l10n::t_("No preloaded MountConfig for companion {mount_unit}"),
            &[("mount_unit", &(trigger.mount_unit).to_string())]
        ))),
    };

    // Re-check liveness before replying: the automount may have been
    // torn down while the (potentially slow) mount was in flight.
    let still_active = {
        let reg = automount_registry.lock();
        reg.get(&trigger.unit_name)
            .map(|inst| inst.state == AutomountState::Running)
            .unwrap_or(false)
    };
    if !still_active {
        warn!(
            "Automount {} torn down during mount; skipping ACK",
            trigger.unit_name
        );
        return Ok(());
    }

    match result {
        Ok(()) => {
            if let Err(e) = autofs_send_ready(trigger.ioctl_fd, token) {
                warn!("ACK failed for {}: {}", trigger.unit_name, e);
            }
            let mut reg = automount_registry.lock();
            if let Some(inst) = reg.get_mut(&trigger.unit_name) {
                inst.state = AutomountState::Waiting;
                inst.last_error = None;
            }
        }
        Err(e) => {
            warn!(
                "Automount trigger mount failed for {}: {}",
                trigger.unit_name, e
            );
            if let Err(ack_err) = autofs_send_fail(trigger.ioctl_fd, token) {
                warn!("FAIL reply failed for {}: {}", trigger.unit_name, ack_err);
            }
            let mut reg = automount_registry.lock();
            if let Some(inst) = reg.get_mut(&trigger.unit_name) {
                inst.state = AutomountState::Waiting;
                inst.last_error = Some(e.to_string());
            }
        }
    }
    event_pub.publish_unit_state_update(
        vec![build_mount_status_of(mount_registry, &trigger.mount_unit)],
        false,
    );
    {
        let reg = automount_registry.lock();
        if let Some(inst) = reg.get(&trigger.unit_name) {
            event_pub.publish_unit_state_update(
                vec![build_automount_status(
                    &trigger.unit_name,
                    &inst.state,
                    inst.last_error.as_deref(),
                )],
                false,
            );
        }
    }
    Ok(())
}

/// ExpireRequest: the kernel wants to expire the idle mount.  Umount the
/// real fs ourselves (the kernel blocks on our ACK), then ACK and return to
/// Waiting.  If the fs is busy, FAIL and stay Running (kernel retries later).
async fn handle_expire_request(
    mount_registry: &MountRegistry,
    automount_registry: &AutomountRegistry,
    event_pub: &EventPublisher,
    trigger: &AutomountTrigger,
    token: u32,
) -> Result<()> {
    let active = {
        let reg = automount_registry.lock();
        reg.get(&trigger.unit_name)
            .map(|inst| inst.state == AutomountState::Running)
            .unwrap_or(false)
    };
    if !active {
        return Ok(());
    }

    let still_mounted = mountinfo::mount_point_is_mounted(&trigger.where_);
    if still_mounted {
        if let Err(e) = do_umount(
            mount_registry.clone(),
            &trigger.mount_unit,
            trigger.mount_config.as_ref(),
        )
        .await
        {
            warn!(
                "Expire umount failed for {}: {} (staying Running)",
                trigger.unit_name, e
            );
            if let Err(ack_err) = autofs_send_fail(trigger.ioctl_fd, token) {
                warn!("FAIL reply failed for {}: {}", trigger.unit_name, ack_err);
            }
            event_pub.publish_unit_state_update(
                vec![build_mount_status_of(mount_registry, &trigger.mount_unit)],
                false,
            );
            return Ok(());
        }
    }

    if let Err(e) = autofs_send_ready(trigger.ioctl_fd, token) {
        warn!("Expire ACK failed for {}: {}", trigger.unit_name, e);
    }
    {
        let mut reg = automount_registry.lock();
        if let Some(inst) = reg.get_mut(&trigger.unit_name) {
            inst.state = AutomountState::Waiting;
        }
    }

    event_pub.publish_unit_state_update(
        vec![build_mount_status_of(mount_registry, &trigger.mount_unit)],
        false,
    );
    {
        let reg = automount_registry.lock();
        if let Some(inst) = reg.get(&trigger.unit_name) {
            event_pub.publish_unit_state_update(
                vec![build_automount_status(
                    &trigger.unit_name,
                    &inst.state,
                    inst.last_error.as_deref(),
                )],
                false,
            );
        }
    }
    Ok(())
}

fn build_mount_status_of(mount_registry: &MountRegistry, unit_name: &str) -> UnitStatus {
    let guard = mount_registry.lock();
    match guard.get(unit_name) {
        Some(inst) => build_mount_status(unit_name, &inst.state),
        None => UnitStatus {
            unit_name: unit_name.to_string(),
            active_state: "inactive".to_string(),
            sub_state: "dead".to_string(),
            main_pid: 0,
            invocation_id: String::new(),
            extensions: HashMap::new(),
        },
    }
}

// Helper to determine unit type by checking registries
fn resolve_unit_type(
    mount_registry: &MountRegistry,
    automount_registry: &AutomountRegistry,
    unit_name: &str,
) -> &'static str {
    if mount_registry.lock().contains_key(unit_name) {
        "mount"
    } else if automount_registry.lock().contains_key(unit_name) {
        "automount"
    } else {
        if unit_name.ends_with(".automount") {
            "automount"
        } else {
            "mount"
        }
    }
}

fn path_is_mount_point(path: &str) -> bool {
    let path_c = match std::ffi::CString::new(path) {
        Ok(p) => p,
        Err(_) => return false,
    };
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        if libc::stat(path_c.as_ptr(), &mut st) < 0 {
            return false;
        }
        let mut parent_st: libc::stat = std::mem::zeroed();
        let parent = match std::path::Path::new(path).parent() {
            Some(p) => p,
            None => return false,
        };
        let parent_c = match std::ffi::CString::new(parent.to_string_lossy().as_ref()) {
            Ok(p) => p,
            Err(_) => return false,
        };
        if libc::stat(parent_c.as_ptr(), &mut parent_st) < 0 {
            return false;
        }
        st.st_dev != parent_st.st_dev
    }
}

fn build_mount_status(unit_name: &str, state: &MountState) -> UnitStatus {
    let (active_state, sub_state) = match state {
        MountState::Dead => ("inactive", "dead"),
        MountState::Mounted => ("active", "mounted"),
        MountState::Unmounting => ("deactivating", "unmounting"),
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
        // Check mount registry first.
        let mount_point = {
            let guard = self.mount_registry.lock();
            guard
                .get(unit_name)
                .map(|inst| (inst.state, inst.mount_point.clone()))
        };

        if let Some((state, mp)) = mount_point {
            // Cross-verify against kernel: if we think Dead but
            // the mount is actually present, auto-correct.
            if state == MountState::Dead && mountinfo::mount_point_is_mounted(&mp) {
                let mut guard = self.mount_registry.lock();
                if let Some(inst) = guard.get_mut(unit_name) {
                    if inst.state == MountState::Dead {
                        debug!(
                            "status() cross-verify: {} is Dead but mounted in kernel → auto-correcting",
                            unit_name
                        );
                        inst.state = MountState::Mounted;
                        inst.from_mountinfo = true;
                    }
                }
                return Ok(build_mount_status(unit_name, &MountState::Mounted));
            }
            return Ok(build_mount_status(unit_name, &state));
        }

        // Check automount registry second.
        {
            let guard = self.automount_registry.lock();
            if let Some(inst) = guard.get(unit_name) {
                return Ok(build_automount_status(
                    unit_name,
                    &inst.state,
                    inst.last_error.as_deref(),
                ));
            }
        }

        // Unknown unit: synthesize status from unit name as fallback.
        Ok(UnitStatus {
            unit_name: unit_name.to_string(),
            active_state: "inactive".to_string(),
            sub_state: "dead".to_string(),
            main_pid: 0,
            invocation_id: String::new(),
            extensions: HashMap::new(),
        })
    }

    async fn sync_state(&self) -> Vec<UnitStatus> {
        // Discover already-mounted filesystems so the connect-time full
        // snapshot includes them even if the mountinfo monitor has not run
        // its first poll yet (e.g. tmp.mount for the kernel-mounted /tmp).
        mountinfo::refresh_registry(&self.mount_registry);
        let mut units = Vec::new();
        {
            let guard = self.mount_registry.lock();
            for inst in guard.values() {
                units.push(build_mount_status(&inst.unit_name, &inst.state));
            }
        }
        {
            let guard = self.automount_registry.lock();
            for inst in guard.values() {
                units.push(build_automount_status(
                    &inst.unit_name,
                    &inst.state,
                    inst.last_error.as_deref(),
                ));
            }
        }
        units
    }

    async fn start(&self, unit_name: &str, config: &[u8], _invocation_id: &str) -> Result<()> {
        let cfg = decode_unit_config(config)?;
        match resolve_unit_type(&self.mount_registry, &self.automount_registry, unit_name) {
            "mount" => {
                let mount_cfg = cfg.mount.as_ref().ok_or_else(|| {
                    anyhow::anyhow!(sysa::l10n::fmt(
                        sysa::l10n::t_("No MountConfig for {unit_name}"),
                        &[("unit_name", &unit_name.to_string())]
                    ))
                })?;
                do_mount(self.mount_registry.clone(), unit_name, mount_cfg).await?;
                self.publish_mount_state(unit_name);
            }
            "automount" => {
                let auto_cfg = cfg.automount.as_ref().ok_or_else(|| {
                    anyhow::anyhow!(sysa::l10n::fmt(
                        sysa::l10n::t_("No AutomountConfig for {unit_name}"),
                        &[("unit_name", &unit_name.to_string())]
                    ))
                })?;
                if path_is_mount_point(&auto_cfg.r#where) {
                    anyhow::bail!(sysa::l10n::fmt(
                        sysa::l10n::t_("Path {path} is already a mount point"),
                        &[("path", &(auto_cfg.r#where).to_string())]
                    ));
                }
                // The companion mount config is preloaded by SysA (scheme A):
                // the worker mounts the real fs on kernel trigger with no
                // round-trip.  `None` is tolerated but trigger mounts fail.
                automount_enter_waiting(
                    self.automount_registry.clone(),
                    unit_name,
                    auto_cfg,
                    cfg.mount.as_ref(),
                    self.trigger_tx.clone(),
                )
                .await?;
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
        match resolve_unit_type(&self.mount_registry, &self.automount_registry, unit_name) {
            "mount" => {
                do_umount(self.mount_registry.clone(), unit_name, None).await?;
                self.publish_mount_state(unit_name);
            }
            "automount" => {
                // Boundary B: if the companion real fs is mounted (state
                // Running), unmount it first (fails if busy, leaving the
                // automount Running), then tear down the autofs sentinel.
                let (partner_mounted, mount_cfg) = {
                    let reg = self.automount_registry.lock();
                    let cfg = reg
                        .get(unit_name)
                        .and_then(|inst| inst.mount_config.clone());
                    let mounted = {
                        let mreg = self.mount_registry.lock();
                        let partner = companion_mount_unit(unit_name);
                        mreg.get(&partner)
                            .map(|inst| inst.state != MountState::Dead)
                            .unwrap_or(false)
                    };
                    (mounted, cfg)
                };
                if partner_mounted {
                    let partner = companion_mount_unit(unit_name);
                    do_umount(self.mount_registry.clone(), &partner, mount_cfg.as_ref()).await?;
                    self.publish_mount_state(&partner);
                }
                automount_enter_dead(self.automount_registry.clone(), unit_name).await?;
                self.publish_automount_state(unit_name);
            }
            _ => anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_("Unknown unit type for {unit_name}"),
                &[("unit_name", &unit_name.to_string())]
            )),
        }
        Ok(())
    }

    async fn restart(&self, unit_name: &str, config: &[u8], _invocation_id: &str) -> Result<()> {
        let cfg = decode_unit_config(config)?;
        match resolve_unit_type(&self.mount_registry, &self.automount_registry, unit_name) {
            "mount" => {
                let mount_cfg = cfg.mount.as_ref().ok_or_else(|| {
                    anyhow::anyhow!(sysa::l10n::fmt(
                        sysa::l10n::t_("No MountConfig for {unit_name}"),
                        &[("unit_name", &unit_name.to_string())]
                    ))
                })?;
                do_umount(self.mount_registry.clone(), unit_name, Some(mount_cfg)).await?;
                do_mount(self.mount_registry.clone(), unit_name, mount_cfg).await?;
                self.publish_mount_state(unit_name);
            }
            "automount" => {
                automount_enter_dead(self.automount_registry.clone(), unit_name).await?;
                let auto_cfg = cfg.automount.as_ref().ok_or_else(|| {
                    anyhow::anyhow!(sysa::l10n::fmt(
                        sysa::l10n::t_("No AutomountConfig for {unit_name}"),
                        &[("unit_name", &unit_name.to_string())]
                    ))
                })?;
                automount_enter_waiting(
                    self.automount_registry.clone(),
                    unit_name,
                    auto_cfg,
                    cfg.mount.as_ref(),
                    self.trigger_tx.clone(),
                )
                .await?;
                self.publish_automount_state(unit_name);
            }
            _ => anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_("Unknown unit type for {unit_name}"),
                &[("unit_name", &unit_name.to_string())]
            )),
        }
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
        do_remount(self.mount_registry.clone(), unit_name, mount_cfg).await?;
        self.publish_mount_state(unit_name);
        Ok(())
    }
}
