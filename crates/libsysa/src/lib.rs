//! Common library shared by System A and all System Workers.
//!
//! The IPC layer is split into two crates: `libsystema-shared-rs` owns the wire
//! contract (protobuf types, framing, IR, error codes), the `UnitStatus`
//! model and the worker-side runtime/clients; `libsystema-sysa` depends on it
//! and adds only the server-side handshake gate.  Both are re-exported here
//! unchanged, so tree-wide `sysa::ipc`/`sysa::proto`/`sysa::worker_ipc` paths
//! keep working: every binary and worker keeps depending on this crate alone.
//!
//! Owned here (binary infrastructure and server-side policy):
//! - Shared logging bootstrap for every binary
//! - The notify broadcast channel (human-visible boot progress)
//! - Event bus topics (server-side dispatch of unit state)
//! - Child-subreaper / reaper registration for workers
//! - Mount-point jurisdiction policy and unit name helpers
//! - `--version` / `--full-version` output
//! - The bake-in path default registry (once in libsysa so nothing
//!   lower in the dependency stack needs to carry it)
//! - The gettext/gettext wiring and the English msgid helpers (`l10n`)
//! - Localized rendering of the IPC kernel's error codes (`errors`)

pub use libsystema_sysa::ipc;
pub use systema_shared::{
    controller, error_code, finder, ir, proto, staging_admin, unitstate_admin, worker_ipc,
};

pub mod errors;
pub mod event_bus;
pub mod firstboot;
pub mod init_config;
pub mod l10n;
pub mod logging;
pub mod mounts;
pub mod notify;
pub mod paths;
pub mod reaper;
pub mod signals;
pub mod unit_name;
pub mod version;
