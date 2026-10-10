use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use sysa::proto::MountConfig;
use tokio::task::JoinHandle;

// ---------------------------------------------------------------------------
// Mount state machine
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountState {
    Dead,
    Mounted,
    Unmounting,
}

pub struct MountInstance {
    pub unit_name: String,
    pub state: MountState,
    pub mount_point: String,
    pub what: String,
    pub fstype: String,
    pub options: String,
    pub from_mountinfo: bool,
    pub from_fragment: bool,
    pub n_retry_umount: u32,
}

impl MountInstance {
    pub fn new(unit_name: String, mount_point: String, what: String) -> Self {
        MountInstance {
            unit_name,
            state: MountState::Dead,
            mount_point,
            what,
            fstype: String::new(),
            options: String::new(),
            from_mountinfo: false,
            from_fragment: false,
            n_retry_umount: 0,
        }
    }
}

pub type MountRegistry = Arc<Mutex<HashMap<String, MountInstance>>>;

pub fn new_mount_registry() -> MountRegistry {
    Arc::new(Mutex::new(HashMap::new()))
}

// ---------------------------------------------------------------------------
// Automount state machine
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomountState {
    Dead,
    Waiting,
    Running,
}

pub struct AutomountInstance {
    pub unit_name: String,
    pub state: AutomountState,
    pub where_: String,
    pub extra_options: String,
    pub timeout_idle_usec: u64,
    pub directory_mode: String,
    pub pipe_fd: Option<i32>,
    pub dev_id: u64,
    pub ioctl_fd: Option<i32>,
    /// Preloaded config of the companion `.mount` unit, used to satisfy
    /// kernel mount requests without a round-trip to SysA.
    pub mount_config: Option<MountConfig>,
    /// Failure message of the last trigger attempt (cleared on success).
    pub last_error: Option<String>,
    /// Handle of the idle-expire timer task (aborted on teardown).
    pub expire_handle: Option<JoinHandle<()>>,
}

impl AutomountInstance {
    pub fn new(unit_name: String, where_: String) -> Self {
        AutomountInstance {
            unit_name,
            state: AutomountState::Dead,
            where_,
            extra_options: String::new(),
            timeout_idle_usec: 0,
            directory_mode: String::from("0755"),
            pipe_fd: None,
            dev_id: 0,
            ioctl_fd: None,
            mount_config: None,
            last_error: None,
            expire_handle: None,
        }
    }
}

pub type AutomountRegistry = Arc<Mutex<HashMap<String, AutomountInstance>>>;

pub fn new_automount_registry() -> AutomountRegistry {
    Arc::new(Mutex::new(HashMap::new()))
}
