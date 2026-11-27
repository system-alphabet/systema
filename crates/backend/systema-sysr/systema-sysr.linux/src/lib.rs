//! System R — the Linux resource-control worker.
//!
//! Implements resource-control management against the unified cgroup
//! hierarchy mounted at `/sys/fs/cgroup`. On non-Linux platforms the cgroup
//! backend is compiled out and [`linux_controller`] returns a
//! [`NoopController`], so the workspace still builds everywhere.

use std::sync::Arc;

use systema_sysr_common::{NoopController, ResourceController};

#[cfg(any(target_os = "linux", target_os = "android"))]
mod cgroup_v2;
pub mod ipc;
pub mod register;
pub mod worker;

pub use worker::{new_registry, ManagedUnit, ResourceRegistry, ResourceWorker, DEFAULT_SLICE};

/// Build the resource controller for this platform.
///
/// On Linux, returns a cgroup v2 controller when the unified hierarchy is
/// mounted and usable, otherwise a no-op fallback.  On every other platform
/// the no-op fallback is always returned.  Shared across connection attempts
/// and worker instances.
pub fn linux_controller() -> Arc<dyn ResourceController> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        match cgroup_v2::CgroupV2Controller::detect() {
            Some(c) => Arc::new(c),
            None => Arc::new(NoopController),
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        Arc::new(NoopController)
    }
}
