//! Background discovery engine.
//!
//! A single loop, woken by netlink uevents (Linux) or a 5s poll timer,
//! reconciles the device registry with the real hardware:
//! 1. re-enumerate `/dev` (+ sysfs metadata when available),
//! 2. upsert real-device units and re-evaluate match rules,
//! 3. inject newly-materialised real devices into System A via the finder
//!    staging area (UID-bound, timeless — safe to re-run),
//! 4. publish `unit.state_update` for every unit whose state changed.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use sysa::controller::UnitStatus;
use sysa::finder::UnitFinder;
use sysa::worker_ipc::EventPublisher;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{error, info, warn};
use sysf::ir::{UnitIR, UnitType};

use crate::discovery::{enumerate_devices, DeviceMeta};
use crate::matchrule::{device_matches, has_rules};
use crate::naming::unit_name_for_node;
use crate::state::{DeviceInstance, DeviceState, Registry};

/// Poll interval when no kernel uevents are available.
pub const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// State shared between the engine task, the IPC controller and the
/// uevent watcher thread.
pub struct EngineShared {
    pub registry: Registry,
    /// Latest `EventPublisher` from the current IPC connection (None while
    /// disconnected; publishes are then skipped).
    pub event_pub: Mutex<Option<EventPublisher>>,
    /// Device units already committed into System A.
    pub injected: Mutex<HashSet<String>>,
    /// Delivered state keys per unit ("active/waiting" etc.), used to only
    /// publish on change.
    pub published: Mutex<HashMap<String, String>>,
    /// Send a unit to wake the engine for an out-of-band refresh.
    pub refresh_tx: UnboundedSender<()>,
}

impl EngineShared {
    pub fn new() -> (Arc<Self>, tokio::sync::mpsc::UnboundedReceiver<()>) {
        let (refresh_tx, refresh_rx) = tokio::sync::mpsc::unbounded_channel();
        let shared = Arc::new(EngineShared {
            registry: crate::state::new_registry(),
            event_pub: Mutex::new(None),
            injected: Mutex::new(HashSet::new()),
            published: Mutex::new(HashMap::new()),
            refresh_tx,
        });
        (shared, refresh_rx)
    }
}

/// Spawn the discovery engine loop on the current runtime.
pub fn spawn_engine(shared: Arc<EngineShared>, mut refresh_rx: tokio::sync::mpsc::UnboundedReceiver<()>) {
    tokio::spawn(async move {
        // The uevent watcher is an optimisation; if it fails to start the
        // poll timer below remains the safety net.
        let _ = crate::netlink::spawn_uevent_watcher(shared.refresh_tx.clone());

        loop {
            tokio::select! {
                _ = refresh_rx.recv() => {}
                _ = tokio::time::sleep(POLL_INTERVAL) => {}
            }
            // Coalesce any burst of signals that arrived together.
            while refresh_rx.try_recv().is_ok() {}

            if let Err(e) = do_refresh(&shared).await {
                warn!("Device refresh failed: {e}");
            }
        }
    });
}

/// Scan + reconcile + inject + publish.  Idempotent and cheap to call.
async fn do_refresh(shared: &Arc<EngineShared>) -> anyhow::Result<()> {
    let devices = enumerate_devices();

    let new_to_inject = reconcile(shared, &devices);
    inject_units(shared, &new_to_inject).await;
    publish_diffs(shared);
    Ok(())
}

/// Recompute the registry against the current device set.  Returns the names
/// of real device units that appeared and were not yet injected into System A.
fn reconcile(shared: &Arc<EngineShared>, devices: &[DeviceMeta]) -> Vec<String> {
    let mut reg = shared.registry.write();
    let mut new_to_inject = Vec::new();
    let mut present: HashSet<String> = HashSet::with_capacity(devices.len());

    // Pass 1: materialise every real device by its canonical unit name.
    for dev in devices {
        let uname = unit_name_for_node(&dev.node);
        present.insert(uname.clone());
        match reg.entry(uname.clone()) {
            std::collections::hash_map::Entry::Occupied(mut e) => {
                let inst = e.get_mut();
                inst.dev = Some(dev.clone());
                if !inst.state.is_active() {
                    inst.state = DeviceState::Active;
                    inst.last_error = None;
                }
            }
            std::collections::hash_map::Entry::Vacant(v) => {
                info!("Device unit '{uname}' appeared (/dev/{})", dev.node);
                v.insert(DeviceInstance {
                    state: DeviceState::Active,
                    config: None,
                    dev: Some(dev.clone()),
                    invocation_id: None,
                    last_error: None,
                });
                new_to_inject.push(uname);
            }
        }
    }

    // Pass 2: rule-based units (config from start/reload).
    let names: Vec<String> = reg.keys().cloned().collect();
    for name in names {
        let inst = match reg.get_mut(&name) {
            Some(i) => i,
            None => continue,
        };
        if inst.state == DeviceState::Dead {
            continue;
        }
        match &inst.config {
            Some(cfg) if has_rules(cfg) => {
                let matched =
                    devices.iter().find(|d| device_matches(cfg, d)).cloned();
                match matched {
                    Some(dev) => {
                        if !inst.state.is_active() {
                            info!("Device unit '{name}' matched /dev/{}", dev.node);
                            inst.state = DeviceState::Active;
                            inst.last_error = None;
                        }
                        inst.dev = Some(dev);
                    }
                    None => {
                        if inst.state.is_active() {
                            info!("Device unit '{name}' no longer matches any device");
                            inst.state = DeviceState::Inactive;
                        }
                        inst.dev = None;
                    }
                }
            }
            // No match rules: the unit's identity is its name (handled in
            // pass 1). Mark it inactive only if that node is not in the
            // current device set.
            _ => {
                if !present.contains(&name) && inst.state.is_active() {
                    inst.state = DeviceState::Inactive;
                }
            }
        }
    }
    drop(reg); // no borrow held below
    new_to_inject
}

/// Inject the given unit identities into System A via the finder staging
/// area.  Staging is keyed by the caller's UID, so this process simply
/// registers, commits, and both connections resolve to the same staging.
async fn inject_units(shared: &Arc<EngineShared>, names: &[String]) {
    if names.is_empty() {
        return;
    }
    let mut units: HashMap<String, UnitIR> = HashMap::with_capacity(names.len());
    {
        let reg = shared.registry.read();
        for name in names {
            let Some(inst) = reg.get(name) else {
                continue;
            };
            let description = inst
                .dev
                .as_ref()
                .map(|d| {
                    format!(
                        "{} ({}:{})",
                        d.dev_file, d.major, d.minor
                    )
                })
                .unwrap_or_else(|| format!("Device {}", name.trim_end_matches(".device")));
            units.insert(
                name.clone(),
                UnitIR {
                    id: name.clone(),
                    unit_type: Some(UnitType::Device),
                    description: Some(description),
                    source_format: None,
                    source_path: None,
                    slice: None,
                    dependencies: None,
                    service: None,
                    mount: None,
                    automount: None,
                    timer: None,
                    socket: None,
                    conditions: None,
                    asserts: None,
                    wanted_by: None,
                    required_by: None,
                    aliases: Vec::new(),
                    resource_control: None,
                },
            );
        }
    }
    if units.is_empty() {
        return;
    }

    let json = match serde_json::to_vec(&units) {
        Ok(j) => j,
        Err(e) => {
            error!("Cannot serialise device units for injection: {e}");
            return;
        }
    };

    let client = UnitFinder::new();
    match client.register_units("systema-sysd/discovery", json).await {
        Ok(reg_ack) if reg_ack.success => {
            match client.commit_units("systema-sysd/discovery").await {
                Ok(cm_ack) if cm_ack.success => {
                    info!(
                        "Committed {} newly discovered device units ({})",
                        cm_ack.unit_count,
                        names.join(", ")
                    );
                    let mut injected = shared.injected.lock();
                    injected.extend(names.iter().cloned());
                }
                Ok(cm_ack) => error!("Finder commit refused: {}", cm_ack.message),
                Err(e) => error!("Finder commit failed: {e}"),
            }
        }
        Ok(reg_ack) => error!("Finder register refused: {}", reg_ack.message),
        Err(e) => warn!("Cannot inject device units (System A unavailable?): {e}"),
    }
}

/// Push `unit.state_update` for every unit whose state changed.
pub fn publish_diffs(shared: &Arc<EngineShared>) {
    let to_publish: Vec<UnitStatus> = {
        let reg = shared.registry.read();
        let mut published = shared.published.lock();
        let mut out = Vec::new();
        for (name, inst) in reg.iter() {
            let key = state_key(inst);
            if published.get(name).map(|k| k == &key).unwrap_or(false) {
                continue;
            }
            published.insert(name.clone(), key);
            out.push(status_of(name, inst));
        }
        out
    };
    if to_publish.is_empty() {
        return;
    }
    let guard = shared.event_pub.lock();
    if let Some(pub_) = guard.as_ref() {
        pub_.publish_unit_state_update(to_publish, false);
    }
}

/// Publish the current state of a single named unit (used by the controller
/// after explicit activations).
pub fn publish_unit(shared: &Arc<EngineShared>, unit_name: &str) {
    let status = {
        let reg = shared.registry.read();
        let Some(inst) = reg.get(unit_name) else {
            return;
        };
        let key = state_key(inst);
        let mut published = shared.published.lock();
        published.insert(unit_name.to_string(), key);
        status_of(unit_name, inst)
    };
    let guard = shared.event_pub.lock();
    if let Some(pub_) = guard.as_ref() {
        pub_.publish_unit_state_update(vec![status], false);
    }
}

fn state_key(inst: &DeviceInstance) -> String {
    format!("{}/{}", inst.state.active_state(), inst.state.as_str())
}

/// Build the standardised `UnitStatus` for a device instance.
pub fn status_of(unit_name: &str, inst: &DeviceInstance) -> UnitStatus {
    let mut extensions = HashMap::new();
    if let Some(dev) = &inst.dev {
        extensions.insert("dev_node".to_string(), dev.dev_file.clone());
        extensions.insert("major".to_string(), dev.major.to_string());
        extensions.insert("minor".to_string(), dev.minor.to_string());
        if let Some(sp) = &dev.sysfs_path {
            extensions.insert("sysfs_path".to_string(), sp.clone());
        }
        if let Some(sub) = &dev.subsystem {
            extensions.insert("subsystem".to_string(), sub.clone());
        }
    }
    if let Some(err) = &inst.last_error {
        extensions.insert("last_error".to_string(), err.clone());
    }
    UnitStatus {
        unit_name: unit_name.to_string(),
        active_state: inst.state.active_state().to_string(),
        sub_state: inst.state.as_str().to_string(),
        main_pid: 0,
        invocation_id: inst.invocation_id.clone().unwrap_or_default(),
        extensions,
    }
}