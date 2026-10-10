use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use sysa::proto::MountConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountState {
    Dead,
    #[allow(dead_code)]
    Mounting,
    Mounted,
    Unmounting,
    Failed,
}

pub struct MountInstance {
    pub state: MountState,
    pub mount_point: String,
    /// Device or remote source backing the mount (from the mount table).
    pub what: String,
    /// Filesystem type of the mount (from the mount table).
    pub fstype: String,
    /// Comma-separated options of the mount (from the mount table).
    pub options: String,
    /// True when this instance was discovered from the system mount table
    /// rather than created by a start job from SysA.
    pub from_mountinfo: bool,
    pub main_pid: Option<u32>,
}

impl MountInstance {
    /// `unit_name` is accepted for symmetry with the Linux worker, but the
    /// registry maps by name, so the instance itself does not store it.
    pub fn new(_unit_name: String, mount_point: String, what: String) -> Self {
        MountInstance {
            state: MountState::Dead,
            mount_point,
            what,
            fstype: String::new(),
            options: String::new(),
            from_mountinfo: false,
            main_pid: None,
        }
    }
}

pub type MountRegistry = Arc<Mutex<HashMap<String, MountInstance>>>;

pub fn new_registry() -> MountRegistry {
    Arc::new(Mutex::new(HashMap::new()))
}

// ---------------------------------------------------------------------------
// Automount state
// ---------------------------------------------------------------------------
//
// No Unix platform offers a self-contained kernel autofs mechanism like
// Linux, so `.automount` units are implemented with eager-mount semantics:
// start mounts the companion filesystem immediately and stop unmounts it.
// `TimeoutIdleSec` is intentionally ignored.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomountState {
    Dead,
    /// Not used by the eager-mount implementation; kept so the substate
    /// vocabulary matches the Linux worker.
    #[allow(dead_code)]
    Waiting,
    Running,
}

pub struct AutomountInstance {
    pub state: AutomountState,
    /// Preloaded config of the companion `.mount` unit, provided by SysA.
    pub mount_config: Option<MountConfig>,
    /// Failure message of the last start attempt (cleared on success).
    pub last_error: Option<String>,
}

pub type AutomountRegistry = Arc<Mutex<HashMap<String, AutomountInstance>>>;

pub fn new_automount_registry() -> AutomountRegistry {
    Arc::new(Mutex::new(HashMap::new()))
}
