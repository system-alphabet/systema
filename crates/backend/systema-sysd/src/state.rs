//! Runtime registry of `.device` units managed by System D.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use sysa::proto::DeviceConfig;

use crate::discovery::DeviceMeta;

/// Lifecycle state of a device unit, modelled on systemd.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeviceState {
    /// No matching device has been seen since the unit was (de)activated.
    #[default]
    Dead,
    /// A real device satisfying the unit currently exists.
    Active,
    /// Unit is configured but the device is absent right now.
    Inactive,
}

impl DeviceState {
    /// Sub-state used in `UnitStatus.sub_state`.
    pub fn as_str(&self) -> &'static str {
        match self {
            DeviceState::Dead => "dead",
            DeviceState::Active => "active",
            DeviceState::Inactive => "inactive",
        }
    }

    /// Systemd-style active state used in `UnitStatus.active_state`.
    pub fn active_state(&self) -> &'static str {
        match self {
            DeviceState::Dead => "inactive",
            DeviceState::Active => "active",
            DeviceState::Inactive => "inactive",
        }
    }

    /// Convenience: is this the "active" active-state?
    pub fn is_active(&self) -> bool {
        self.active_state() == "active"
    }
}

/// One `.device` unit instance.
#[derive(Debug, Clone, Default)]
pub struct DeviceInstance {
    pub state: DeviceState,
    /// Match configuration supplied by System A via `start`/`reload`.
    /// `None` for implicitly discovered real devices (their identity is
    /// encoded in the unit name itself).
    pub config: Option<DeviceConfig>,
    /// The real device currently backing this unit (None while absent).
    pub dev: Option<DeviceMeta>,
    /// Invocation ID of the current activation.
    pub invocation_id: Option<String>,
    /// Human-readable failure of the last activation/reconciliation.
    pub last_error: Option<String>,
}

pub type Registry = Arc<RwLock<HashMap<String, DeviceInstance>>>;

pub fn new_registry() -> Registry {
    Arc::new(RwLock::new(HashMap::new()))
}