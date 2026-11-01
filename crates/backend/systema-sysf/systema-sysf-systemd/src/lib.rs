//! Systemd unit discovery and parsing for System F.
//!
//! The `systema-sysf.systemd` finder executable: discovers and parses systemd
//! unit files and converts them into the unified
//! [`UnitIR`](systema_sysf::ir::UnitIR) representation.  Exposed as a library
//! as well so the System Allocator can share the unit-file types it receives
//! as IR; parsing itself stays here — System A never reads unit files.

pub mod finder;
pub mod loader;
pub mod parser;
pub mod types;
