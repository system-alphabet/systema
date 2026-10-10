//! System Wrapper — the bridge plane of System A.
//!
//! System A owns the allocator and exposes a one-to-many control-port bus on
//! `control.socket`.  System Wrapper bridge flavors (`systema-sysw.systemd`)
//! consume that bus: they mirror unit state off snapshots + events and publish
//! it on their native bus (e.g. D-Bus) on behalf of System A.
//!
//! This crate is the flavor-independent foundation: the control-port client,
//! event classification, and (later) the shared mirror.

pub mod client;
pub mod event;

pub use client::{ControlClient, ControlEventStream};
pub use event::{ControlEvent, ControlEventKind};