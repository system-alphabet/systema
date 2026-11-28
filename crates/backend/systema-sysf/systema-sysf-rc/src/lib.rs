//! SysV init script discovery and parsing for System F.
//!
//! The `systema-sysf.rc` finder executable: discovers and parses SysV init
//! (`rc`) scripts — including their LSB headers and `rcN.d` runlevel links —
//! and converts them into the unified [`UnitIR`](systema_sysf::ir::UnitIR)
//! representation.  SysV runlevels become the equivalent systemd targets
//! (`0` → `poweroff.target`, `2 3 4` → `multi-user.target`, …), the way
//! systemd-sysv-generator exposes them.

pub mod facilities;
pub mod finder;
pub mod loader;
pub mod parser;
pub mod types;
