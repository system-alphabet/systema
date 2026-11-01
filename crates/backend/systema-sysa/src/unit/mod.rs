//! Unit definitions and transformations consumed by System A.
//!
//! System A shares the Finder's unit types, but unit-file discovery and
//! parsing remain exclusively in System F: the loader submodule only injects
//! default dependencies into definitions already held in memory, and
//! enablement (`UnitFile::enabled`) arrives already decided by the Finder's
//! scan of the unit search paths.

pub use systema_sysf_systemd::types;
pub mod loader;
pub mod template;
