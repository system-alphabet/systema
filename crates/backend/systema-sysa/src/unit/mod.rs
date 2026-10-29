//! Unit definitions and transformations consumed by System A.
//!
//! System A shares the Finder's unit types, but unit-file parsing remains
//! exclusively in System F. The loader submodule injects default dependencies
//! into definitions already held in memory.

pub use systema_sysf_systemd::types;
pub mod enable;
pub mod loader;
pub mod template;
