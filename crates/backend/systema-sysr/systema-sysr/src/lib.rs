//! System R — the resource-control worker.
//!
//! Library facade of `systema-sysr`.  The worker implements
//! [`sysa::controller::UnitController`] and is wired into the shared
//! [`sysa::worker_ipc::WorkerIpc`] loop; the daemon binary lives in
//! `main.rs`.

pub mod ipc;
pub mod register;
pub mod worker;

pub use worker::{ManagedUnit, ResourceRegistry, ResourceWorker, new_registry, DEFAULT_SLICE};
