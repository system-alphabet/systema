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
//! - The bake-in path default registry (once in libsysa so nothing
//!   lower in the dependency stack needs to carry it)
//! - The gettext/gettext wiring and the English msgid helpers (`l10n`)
//! - Localized rendering of the IPC kernel's error codes (`errors`)

pub use sysa_ipc::{
    controller, error_code, finder, ipc, ir, proto, signals, staging_admin, unitstate_admin,
    worker_ipc,
};

pub mod errors;
pub mod event_bus;
pub mod firstboot;
pub mod l10n;
pub mod logging;
pub mod mounts;
pub mod notify;
pub mod paths;
pub mod unit_name;
pub mod version;
