//! `AmbientCapabilities=`, `CapabilityBoundingSet=` and
//! `NoNewPrivileges=` for the service pre-exec hook.
//!
//! The unit file is parsed in System F and only the directive names/values
//! reach the worker, so this module owns the conversion from capability
//! names to kernel bits.  Parsing happens in the parent (where allocating and
//! logging is safe); the pre-exec hook only performs raw syscalls.
//!
//! Order mirrors systemd's `exec-invoke.c`: drop the bounding set while still
//! privileged, ask the kernel to keep capabilities across `setuid()` when an
//! ambient set is requested, switch credentials, then raise the ambient set
//! and finally apply `NoNewPrivileges=`.  On non-Linux/Android targets every
//! operation is a no-op so the worker still builds and runs.

use tracing::warn;

use crate::config::ServiceConfig;

#[cfg(any(target_os = "linux", target_os = "android"))]
const PR_CAPBSET_DROP: libc::c_int = 24;
#[cfg(any(target_os = "linux", target_os = "android"))]
const PR_SET_KEEPCAPS: libc::c_int = 8;
#[cfg(any(target_os = "linux", target_os = "android"))]
const PR_SET_NO_NEW_PRIVS: libc::c_int = 38;
#[cfg(any(target_os = "linux", target_os = "android"))]
const PR_CAP_AMBIENT: libc::c_int = 47;
#[cfg(any(target_os = "linux", target_os = "android"))]
const PR_CAP_AMBIENT_RAISE: libc::c_ulong = 2;

/// Capability names in kernel numbering (`uapi/linux/capability.h` 0..=40).
const CAP_NAMES: [&str; 41] = [
    "CHOWN",
    "DAC_OVERRIDE",
    "DAC_READ_SEARCH",
    "FOWNER",
    "FSETID",
    "KILL",
    "SETGID",
    "SETUID",
    "SETPCAP",
    "LINUX_IMMUTABLE",
    "NET_BIND_SERVICE",
    "NET_BROADCAST",
    "NET_ADMIN",
    "NET_RAW",
    "IPC_LOCK",
    "IPC_OWNER",
    "SYS_MODULE",
    "SYS_RAWIO",
    "SYS_CHROOT",
    "SYS_PTRACE",
    "SYS_PACCT",
    "SYS_ADMIN",
    "SYS_BOOT",
    "SYS_NICE",
    "SYS_RESOURCE",
    "SYS_TIME",
    "SYS_TTY_CONFIG",
    "MKNOD",
    "LEASE",
    "AUDIT_WRITE",
    "AUDIT_CONTROL",
    "SETFCAP",
    "MAC_OVERRIDE",
    "MAC_ADMIN",
    "SYSLOG",
    "WAKE_ALARM",
    "BLOCK_SUSPEND",
    "AUDIT_READ",
    "PERFMON",
    "BPF",
    "CHECKPOINT_RESTORE",
];

/// The capability directives of one service, resolved to kernel bit masks.
#[derive(Debug, Clone, Default)]
pub(crate) struct CapabilityPlan {
    ambient: u64,
    /// `None` = leave the inherited bounding set alone; `Some(mask)` = drop
    /// every known capability not in the mask.
    bounding: Option<u64>,
    no_new_privileges: bool,
    last_cap: u32,
}

impl CapabilityPlan {
    pub(crate) fn from_service(svc: &ServiceConfig, unit_name: &str) -> Self {
        let mut plan = CapabilityPlan {
            last_cap: cap_last_cap(),
            ..CapabilityPlan::default()
        };

        if let Some(bounds) = &svc.capability_bounding_set {
            plan.bounding = Some(parse_capability_set(
                bounds,
                plan.last_cap,
                unit_name,
                "CapabilityBoundingSet",
            ));
        }

        plan.ambient = parse_capability_set(
            &svc.ambient_capabilities,
            plan.last_cap,
            unit_name,
            "AmbientCapabilities",
        );

        // Ambient capabilities are implicitly added to the bounding set
        // (systemd.exec(5)): an explicit CapabilityBoundingSet= must not
        // erase a capability the unit also asks to keep ambient.
        if let Some(bounds) = plan.bounding.as_mut() {
            *bounds |= plan.ambient;
        }

        plan.no_new_privileges = svc.no_new_privileges;
        plan
    }

    /// A `!`/`!!` prefix on the individual `Exec*=` command forces
    /// `NoNewPrivileges` even when the section does not set it.
    pub(crate) fn with_no_new_privileges(mut self, force: bool) -> Self {
        if force {
            self.no_new_privileges = true;
        }
        self
    }

    /// Whether `PR_SET_KEEPCAPS` must be set before the credential switch.
    pub(crate) fn needs_keep_caps(&self, credentials_will_be_switched: bool) -> bool {
        credentials_will_be_switched && self.ambient != 0
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub(crate) fn apply_bounding_set(&self) -> std::io::Result<()> {
        let Some(bounds) = self.bounding else {
            return Ok(());
        };
        // Leave capabilities above the table untouched: a kernel newer than
        // this table must not lose capabilities merely because we do not know
        // their names yet.
        for cap in 0..=self.last_cap.min(40) {
            if bounds & (1u64 << cap) != 0 {
                continue;
            }
            let rc = unsafe {
                libc::prctl(
                    PR_CAPBSET_DROP,
                    cap as libc::c_ulong,
                    0 as libc::c_ulong,
                    0 as libc::c_ulong,
                    0 as libc::c_ulong,
                )
            };
            if rc < 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    pub(crate) fn apply_bounding_set(&self) -> std::io::Result<()> {
        let _ = (self.ambient, self.bounding, self.last_cap);
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub(crate) fn set_keep_caps(&self) -> std::io::Result<()> {
        let rc = unsafe {
            libc::prctl(
                PR_SET_KEEPCAPS,
                1 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
            )
        };
        if rc < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    pub(crate) fn set_keep_caps(&self) -> std::io::Result<()> {
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub(crate) fn apply_ambient_set(&self) -> std::io::Result<()> {
        if self.ambient == 0 {
            return Ok(());
        }

        #[repr(C)]
        struct CapHeader {
            version: u32,
            pid: i32,
        }
        #[repr(C)]
        #[derive(Clone, Copy, Default)]
        struct CapData {
            effective: u32,
            permitted: u32,
            inheritable: u32,
        }
        const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;

        unsafe extern "C" {
            fn capget(hdrp: *mut CapHeader, dataptr: *mut CapData) -> libc::c_int;
            fn capset(hdrp: *mut CapHeader, dataptr: *const CapData) -> libc::c_int;
        }

        let mut header = CapHeader {
            version: LINUX_CAPABILITY_VERSION_3,
            pid: 0,
        };
        let mut data = [CapData::default(); 2];
        if unsafe { capget(&mut header, data.as_mut_ptr()) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        for (index, slot) in data.iter_mut().enumerate() {
            let shift = index as u32 * 32;
            let bits = ((self.ambient >> shift) & 0xffff_ffff) as u32;
            slot.effective |= bits;
            slot.permitted |= bits;
            slot.inheritable |= bits;
        }
        if unsafe { capset(&mut header, data.as_ptr()) } < 0 {
            return Err(std::io::Error::last_os_error());
        }

        for cap in 0..=40 {
            if self.ambient & (1u64 << cap) == 0 {
                continue;
            }
            let rc = unsafe {
                libc::prctl(
                    PR_CAP_AMBIENT,
                    PR_CAP_AMBIENT_RAISE,
                    cap as libc::c_ulong,
                    0 as libc::c_ulong,
                    0 as libc::c_ulong,
                )
            };
            if rc < 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    pub(crate) fn apply_ambient_set(&self) -> std::io::Result<()> {
        let _ = (self.ambient, self.bounding, self.last_cap);
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub(crate) fn apply_no_new_privileges(&self) -> std::io::Result<()> {
        if !self.no_new_privileges {
            return Ok(());
        }
        let rc = unsafe {
            libc::prctl(
                PR_SET_NO_NEW_PRIVS,
                1 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
            )
        };
        if rc < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    pub(crate) fn apply_no_new_privileges(&self) -> std::io::Result<()> {
        let _ = self.no_new_privileges;
        Ok(())
    }
}

fn parse_capability(raw: &str) -> Option<u32> {
    let name = raw.trim().to_ascii_uppercase();
    let name = name.strip_prefix("CAP_").unwrap_or(&name);
    CAP_NAMES
        .iter()
        .position(|candidate| *candidate == name)
        .map(|index| index as u32)
}

fn parse_capability_set(values: &[String], last_cap: u32, unit_name: &str, directive: &str) -> u64 {
    let inverted = values
        .first()
        .is_some_and(|value| value == "~" || value.starts_with('~'));
    let mut mask = if inverted {
        known_capability_mask(last_cap)
    } else {
        0
    };

    for raw in values {
        let raw = raw.strip_prefix('~').unwrap_or(raw);
        if raw.is_empty() {
            continue;
        }
        match parse_capability(raw) {
            Some(cap) if inverted => mask &= !(1u64 << cap),
            Some(cap) => mask |= 1u64 << cap,
            None => warn!(
                "{}: unknown capability '{}' in {}=; ignoring it",
                unit_name, raw, directive
            ),
        }
    }
    mask
}

fn known_capability_mask(last_cap: u32) -> u64 {
    let last = last_cap.min(CAP_NAMES.len().saturating_sub(1) as u32);
    if last >= 63 {
        u64::MAX
    } else {
        (1u64 << (last + 1)) - 1
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn cap_last_cap() -> u32 {
    std::fs::read_to_string("/proc/sys/kernel/cap_last_cap")
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(40)
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn cap_last_cap() -> u32 {
    40
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service(ambient: &[&str], bounds: Option<&[&str]>, nnp: bool) -> ServiceConfig {
        ServiceConfig {
            ambient_capabilities: ambient.iter().map(|value| value.to_string()).collect(),
            capability_bounding_set: bounds
                .map(|values| values.iter().map(|value| value.to_string()).collect()),
            no_new_privileges: nnp,
            ..Default::default()
        }
    }

    #[test]
    fn names_are_case_insensitive_and_accept_the_optional_prefix() {
        assert_eq!(parse_capability("CAP_NET_BIND_SERVICE"), Some(10));
        assert_eq!(parse_capability("cap_net_raw"), Some(13));
        assert_eq!(parse_capability("NET_ADMIN"), Some(12));
        assert_eq!(parse_capability("CAP_NOT_REAL"), None);
    }

    #[test]
    fn ambient_is_added_to_an_explicit_bounding_set() {
        let plan = CapabilityPlan::from_service(
            &service(&["CAP_NET_BIND_SERVICE"], Some(&["CAP_NET_RAW"]), false),
            "resolved.service",
        );
        assert_eq!(plan.ambient, 1u64 << 10);
        assert_eq!(plan.bounding, Some((1u64 << 13) | (1u64 << 10)));
    }

    #[test]
    fn absent_bounding_set_stays_absent() {
        let plan = CapabilityPlan::from_service(&service(&[], None, true), "test.service");
        assert_eq!(plan.bounding, None);
        assert!(plan.no_new_privileges);
    }

    #[test]
    fn explicit_empty_bounding_set_is_some_zero() {
        let plan = CapabilityPlan::from_service(&service(&[], Some(&[]), false), "test.service");
        assert_eq!(plan.bounding, Some(0));
    }

    #[test]
    fn inverted_bounding_set_keeps_all_but_listed_capabilities() {
        let plan = CapabilityPlan::from_service(
            &service(&[], Some(&["~CAP_SYS_TIME", "CAP_WAKE_ALARM"]), false),
            "udevd.service",
        );
        let bounding = plan.bounding.unwrap();
        assert_eq!(bounding & (1u64 << 25), 0);
        assert_eq!(bounding & (1u64 << 35), 0);
        assert_ne!(bounding & (1u64 << 0), 0);
        assert_ne!(bounding & (1u64 << 12), 0);
    }

    #[test]
    fn normalized_inverted_ambient_set_is_supported() {
        let plan = CapabilityPlan::from_service(
            &service(&["~", "CAP_SYS_TIME"], None, false),
            "test.service",
        );
        assert_eq!(plan.ambient & (1u64 << 25), 0);
        assert_ne!(plan.ambient & (1u64 << 10), 0);
    }

    #[test]
    fn keep_caps_is_only_needed_with_ambient_and_a_credential_switch() {
        let with_ambient =
            CapabilityPlan::from_service(&service(&["CAP_NET_RAW"], None, false), "test.service");
        assert!(with_ambient.needs_keep_caps(true));
        assert!(!with_ambient.needs_keep_caps(false));
        assert!(!CapabilityPlan::default().needs_keep_caps(true));
    }
}
