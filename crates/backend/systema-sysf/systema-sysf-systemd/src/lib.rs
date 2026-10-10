//! Systemd unit discovery and parsing for System F.
//!
//! The `systema-sysf.systemd` finder executable: parses systemd unit files
//! and converts them into the unified [`UnitIR`](systema_sysf::ir::UnitIR)
//! representation.  Exposed as a library as well so the System Allocator
//! can reuse the parser and unit-file types directly.

pub mod finder;
pub mod loader;
pub mod parser;
pub mod types;
