//! Common library shared by System A and all System Workers.
//!
//! The IPC kernel lives in the separate `sysa-ipc` crate and is
//! re-exported here unchanged, so tree-wide `sysa::ipc`/`sysa::proto`/
//! `sysa::worker_ipc` paths keep working: every binary and worker keeps
//! depending on this crate alone.
//!
//! Owned here (binary infrastructure and server-side policy):
//! - Shared logging bootstrap for every binary
//! - The notify broadcast channel (human-visible boot progress)
//! - Event bus topics (server-side dispatch of unit state)
//! - Mount-point jurisdiction policy and unit name helpers
//! - `--version` / `--full-version` output

pub use sysa_ipc::{
    controller, finder, ipc, l10n, paths, proto, signals, staging_admin, unitstate_admin,
    worker_ipc,
};

pub mod event_bus;
pub mod logging;
pub mod mounts;
pub mod notify;
pub mod unit_name;
pub mod version;
