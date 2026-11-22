//! System Wrapper — the bridge plane of System A.
//!
//! System A owns the allocator and exposes a one-to-many control-port bus on
//! `control.socket`.  System Wrapper bridge flavors (`systema-sysw.systemd`)
//! consume that bus: they mirror unit state off snapshots + events and publish
//! it on their native bus (e.g. D-Bus) on behalf of System A.
//!
//! The control-port client and its event model live in `libsystema-shared`
//! (module `manager`); this crate re-exports them so flavor crates have one
//! bridge-facing namespace.

pub use sysa::manager::{calls, ControlClient, ControlEvent, ControlEventKind, ControlEventStream};
