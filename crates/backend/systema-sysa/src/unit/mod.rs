//! Unit module — re-exports from the finder crates.
//!
//! The systemd-specific types and parser now live in
//! `crates/backend/systema-sysf/systema-sysf-systemd`. This module re-exports them so
//! existing System A code continues to work unchanged. The loader submodule
//! remains here because it couples to `AllocatorState`.

pub use systema_sysf_systemd::parser;
pub use systema_sysf_systemd::types;
pub mod enable;
pub mod loader;
