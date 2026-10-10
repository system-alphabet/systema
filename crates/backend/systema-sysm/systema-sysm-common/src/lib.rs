//! Platform-independent logic shared by the System M variants
//! (`systema-sysm.linux` and `systema-sysm.unix`).
//!
//! The mount table itself is discovered differently on each platform
//! (`/proc/self/mountinfo` on Linux, `getmntinfo(3)` / `/etc/mnttab` /
//! `mount -p` on other Unixes), but everything built on top of it — unit
//! name escaping, registry reconciliation, dynamic `UnitIR` construction
//! and the commit flow to System A — is identical and lives here so the
//! two workers cannot drift apart.

pub mod mount_table;
