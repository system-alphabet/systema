//! Mount-point jurisdiction: which mount points the unit system does not own.
//!
//! Workers must not know about each other, so mount-point policy cannot live
//! as a private table inside any one of them.  This module is pure predicates
//! over path strings — no I/O, no registry, no worker types — and holds two
//! orthogonal tables answering two different questions, mirroring the two
//! different questions systemd asks:
//!
//! * [`is_api_mount`] — a *platform fact*.  `/proc`, `/sys`, `/dev`, `/run`
//!   and the subpaths underneath them belong to the kernel, the initramfs or
//!   SysAInit, which mount them before System M starts (`sysi::mount_setup`).
//!   Mirrors systemd's `mount_table[]` and `mount_point_ignore()`
//!   (`src/shared/mount-setup.c`).
//! * [`is_extrinsic_mount`] — *unit semantics*.  The OS base filesystems take
//!   no `local-fs.target` default dependencies in systemd.  Mirrors
//!   `fstab_is_extrinsic()` (`src/shared/fstab-util.c`).
//!
//! The two tables are not collapsible: `/` is extrinsic but not API, `/run`
//! is API but not extrinsic, and `/proc/sys/fs/binfmt_misc` is neither
//! (extrinsic covers it, API does not).
//!
//! Because System A carries no mount-point knowledge at all, System M folds
//! both tables into [`is_exempt_from_units`] while it reconciles the mount
//! table.  The discovery gate is the single place that knowledge lives.
//!
//! Paths match component-wise, equivalent to systemd's `path_equal()` /
//! `path_startswith()`: `/sys/fs/cgroup/system.slice` lies under
//! `/sys/fs/cgroup`, but `/sys/fs/cgroupx` does not, and `/run/hostage` is not
//! under `/run/host`.

use std::sync::OnceLock;

/// Environment variable extending the API table additively: a `:`-separated
/// list of absolute mount points.  A trailing `/` exempts the whole subtree.
/// Entries only ever widen the table — the built-in defaults are fixed.
const API_MOUNTS_ENV: &str = "SYSTEMA_API_MOUNTS";

// ---------------------------------------------------------------------------
// Roots
//
// The only mount-point path literals this module ever writes.  Every derived
// mount point is spelled as (root, relative components), never as a full
// string, so a root can be retargeted without the derived paths drifting.
// ---------------------------------------------------------------------------

/// Root of the kernel interface filesystems.
pub const PROC_PATH: &str = "/proc";
/// Root of the kernel's sysfs.
pub const SYS_PATH: &str = "/sys";
/// Root of devtmpfs.
pub const DEV_PATH: &str = "/dev";
/// Root of the volatile runtime filesystem.
pub const RUN_PATH: &str = "/run";
/// Filesystem root.
pub const ROOT_PATH: &str = "/";
/// OS base hierarchy (systemd's `Usr=`).
pub const USR_PATH: &str = "/usr";
/// Static configuration hierarchy (systemd's `SYSCONF_DIR`).
pub const ETC_PATH: &str = "/etc";

/// A mount point expressed as a root plus relative components.
///
/// Deriving instead of repeating a full path keeps `/sys/fs/cgroup` written
/// once as components and once in [`Self::path`], so the matching table and
/// the string handed to `mount(2)` cannot drift apart.
#[derive(Clone, Copy, Debug)]
pub struct MountSpec {
    root: &'static str,
    sub: &'static str,
}

impl MountSpec {
    const fn new(root: &'static str, sub: &'static str) -> Self {
        Self { root, sub }
    }

    /// Materialise this mount point as a path string.
    ///
    /// Only callers that need real bytes (SysAInit's `mount(2)` targets) do
    /// this.  Matching never materialises anything.
    pub fn path(&self) -> String {
        if self.sub.is_empty() {
            return self.root.to_string();
        }
        let sep = if self.root.ends_with('/') { "" } else { "/" };
        format!("{}{}{}", self.root, sep, self.sub)
    }
}

/// The cgroup v2 unified hierarchy, derived from [`SYS_PATH`].
pub const CGROUP: MountSpec = MountSpec::new(SYS_PATH, "fs/cgroup");
/// POSIX shared memory, derived from [`DEV_PATH`].
pub const DEV_SHM: MountSpec = MountSpec::new(DEV_PATH, "shm");
/// Pseudo-terminal devpts, derived from [`DEV_PATH`].
pub const DEV_PTS: MountSpec = MountSpec::new(DEV_PATH, "pts");

// ---------------------------------------------------------------------------
// Tables
// ---------------------------------------------------------------------------

/// How far a table entry reaches.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// Only the mount point itself.
    Exact,
    /// The mount point and everything beneath it.
    Subtree,
}

/// API mount points: never become `.mount` units.
///
/// systemd's `mount_table[]` tells PID 1 to mount these itself, and
/// `mount_point_ignore()` keeps them out of the unit system.  systema keeps
/// the same set: SysAInit already mounts the three entries below that need
/// mounting, and the rest come from the kernel or the initramfs.
///
/// `/` is deliberately absent — it is extrinsic, not API (see [`EXTRINSIC`]).
const API: &[(MountSpec, Scope)] = &[
    // Roots, themselves only.
    (MountSpec::new(PROC_PATH, ""), Scope::Exact),
    (MountSpec::new(SYS_PATH, ""), Scope::Exact),
    (MountSpec::new(DEV_PATH, ""), Scope::Exact),
    (MountSpec::new(RUN_PATH, ""), Scope::Exact),
    // Kernel-owned paths that never appear as mount points but may be
    // bind-mounted in from elsewhere.
    (MountSpec::new(PROC_PATH, "sys"), Scope::Exact),
    (MountSpec::new(PROC_PATH, "kmsg"), Scope::Exact),
    (
        MountSpec::new(PROC_PATH, "sys/kernel/random/boot_id"),
        Scope::Exact,
    ),
    // Named hierarchies.  Subtree because containers and cgroup v1 hybrids
    // bind-mount individual children underneath them.
    (CGROUP, Scope::Subtree),
    (MountSpec::new(SYS_PATH, "kernel/security"), Scope::Exact),
    (MountSpec::new(SYS_PATH, "fs/pstore"), Scope::Exact),
    (
        MountSpec::new(SYS_PATH, "firmware/efi/efivars"),
        Scope::Exact,
    ),
    (MountSpec::new(SYS_PATH, "fs/bpf"), Scope::Exact),
    (MountSpec::new(SYS_PATH, "fs/selinux"), Scope::Exact),
    (DEV_SHM, Scope::Exact),
    (DEV_PTS, Scope::Exact),
    (MountSpec::new(DEV_PATH, "console"), Scope::Exact),
    // Stage-1 handoff area.
    (MountSpec::new(RUN_PATH, "host"), Scope::Subtree),
];

/// Extrinsic mount points: the OS base, given no `local-fs.target` default
/// dependencies by systemd.
///
/// In systema the meaning narrows: System A no longer holds any mount-point
/// knowledge, so an extrinsic mount point is simply one that must not enter
/// the unit system.  Excluding it here is what keeps `-.mount` from growing
/// `Conflicts=umount.target` — otherwise shutdown would ask System M to
/// unmount `/`.
const EXTRINSIC: &[(MountSpec, Scope)] = &[
    // systemd's `PATH_IN_SET("/", "/usr", SYSCONF_DIR)`.
    (MountSpec::new(ROOT_PATH, ""), Scope::Exact),
    (MountSpec::new(USR_PATH, ""), Scope::Exact),
    (MountSpec::new(ETC_PATH, ""), Scope::Exact),
    // Stage-1 and alternate-root handoff areas.
    (MountSpec::new(RUN_PATH, "initramfs"), Scope::Subtree),
    (MountSpec::new(RUN_PATH, "nextroot"), Scope::Subtree),
    // systemd's `PATH_STARTSWITH_SET(mount, "/proc", "/sys", "/dev")`.
    // Component-aware, so `/development` is not extrinsic.
    (MountSpec::new(PROC_PATH, ""), Scope::Subtree),
    (MountSpec::new(SYS_PATH, ""), Scope::Subtree),
    (MountSpec::new(DEV_PATH, ""), Scope::Subtree),
];

// ---------------------------------------------------------------------------
// Matching
// ---------------------------------------------------------------------------

/// Whether `path` lies at (`Exact`) or under (`Subtree`) `root`/`sub`.
///
/// Compares path components one at a time, so neither a prefix of a component
/// nor a missing boundary can match.  Allocation-free: `path` is re-split per
/// table entry rather than collected once.
fn matches(path: &str, root: &str, sub: &str, scope: Scope) -> bool {
    let mut got = path.split('/').filter(|c| !c.is_empty());
    for want in root
        .split('/')
        .chain(sub.split('/'))
        .filter(|c| !c.is_empty())
    {
        match got.next() {
            Some(got) if got == want => {}
            _ => return false,
        }
    }
    scope == Scope::Subtree || got.next().is_none()
}

/// Whether `path` is an API mount point: owned by the kernel, the initramfs
/// or SysAInit, never a `.mount` unit.
///
/// Widened additively by the `SYSTEMA_API_MOUNTS` environment variable; the
/// built-in table can only ever be extended, never narrowed.
pub fn is_api_mount(path: &str) -> bool {
    is_api_mount_in(path, api_extras())
}

/// [`is_api_mount`] against an explicit set of environment-supplied extras,
/// so the parser and the extension rule can be tested without touching the
/// process environment.
fn is_api_mount_in(path: &str, extras: &[String]) -> bool {
    if !path.starts_with('/') {
        return false;
    }
    API.iter()
        .any(|(spec, scope)| matches(path, spec.root, spec.sub, *scope))
        || extras.iter().any(|extra| matches_extra(path, extra))
}

/// Whether an environment-supplied entry exempts `path`.  A trailing `/`
/// widens it to a subtree; otherwise it names the mount point alone.
fn matches_extra(path: &str, extra: &str) -> bool {
    match extra.strip_suffix('/') {
        Some(prefix) => matches(path, prefix, "", Scope::Subtree),
        None => matches(path, extra, "", Scope::Exact),
    }
}

/// Whether `path` is an extrinsic mount point: part of the OS base rather
/// than something a unit is responsible for.
///
/// Purely static — unlike [`is_api_mount`] there is no environment knob,
/// because systemd's extrinsic set is fixed too.
pub fn is_extrinsic_mount(path: &str) -> bool {
    path.starts_with('/')
        && EXTRINSIC
            .iter()
            .any(|(spec, scope)| matches(path, spec.root, spec.sub, *scope))
}

/// The discovery gate: `true` when `path` must stay out of the unit system
/// entirely.
///
/// This is the single question System M asks while reconciling the mount
/// table, and therefore the only place mount-point policy lives.
pub fn is_exempt_from_units(path: &str) -> bool {
    is_api_mount(path) || is_extrinsic_mount(path)
}

// ---------------------------------------------------------------------------
// Environment extension
// ---------------------------------------------------------------------------

/// The environment-supplied API entries, parsed once on first use.
fn api_extras() -> &'static [String] {
    static EXTRAS: OnceLock<Vec<String>> = OnceLock::new();
    EXTRAS.get_or_init(|| {
        std::env::var(API_MOUNTS_ENV)
            .map(|raw| parse_extras(&raw))
            .unwrap_or_default()
    })
}

/// Split a raw [`API_MOUNTS_ENV`] value into mount points.
///
/// Absolute paths only: a relative entry can never name a mount point, and
/// dropping it means a typo cannot silently exempt anything real.
fn parse_extras(raw: &str) -> Vec<String> {
    raw.split(':')
        .map(str::trim)
        .filter(|entry| entry.starts_with('/'))
        .map(String::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_table_matches_systemd() {
        for path in [
            "/proc",
            "/sys",
            "/dev",
            "/run",
            "/proc/sys",
            "/proc/kmsg",
            "/proc/sys/kernel/random/boot_id",
            "/sys/fs/cgroup",
            "/sys/fs/cgroup/system.slice",
            "/sys/kernel/security",
            "/sys/fs/pstore",
            "/sys/firmware/efi/efivars",
            "/sys/fs/bpf",
            "/sys/fs/selinux",
            "/dev/shm",
            "/dev/pts",
            "/dev/console",
            "/run/host",
            "/run/host/units",
        ] {
            assert!(is_api_mount(path), "{path} should be an API mount");
        }

        // Not API, though most are extrinsic (see the gate test).
        for path in ["/", "/usr", "/etc", "/home", "/tmp", "/run/initramfs"] {
            assert!(!is_api_mount(path), "{path} should not be an API mount");
        }
    }

    #[test]
    fn extrinsic_table_matches_fstab_is_extrinsic() {
        for path in [
            "/",
            "/usr",
            "/etc",
            "/proc",
            "/proc/sys",
            "/proc/sys/fs/binfmt_misc",
            "/sys",
            "/sys/kernel/debug",
            "/dev",
            "/dev/mqueue",
            "/run/initramfs",
            "/run/initramfs/boot",
            "/run/nextroot/x",
        ] {
            assert!(is_extrinsic_mount(path), "{path} should be extrinsic");
        }

        for path in [
            "/run",
            "/run/host",
            "/tmp",
            "/home",
            "/development",
            "/etcetera",
            "/usr/local",
            "/usrlocal",
        ] {
            assert!(!is_extrinsic_mount(path), "{path} should not be extrinsic");
        }
    }

    #[test]
    fn prefix_matching_is_component_aware() {
        // Under a Subtree entry.
        assert!(is_api_mount("/sys/fs/cgroup/system.slice"));
        assert!(is_api_mount("/run/host/x"));
        assert!(is_extrinsic_mount("/proc/1/root"));

        // A longer component that merely starts with the entry's name.
        assert!(!is_api_mount("/sys/fs/cgroupx"));
        assert!(!is_api_mount("/run/hostage"));
        assert!(!is_extrinsic_mount("/development"));
        assert!(!is_extrinsic_mount("/etcetera"));

        // A subpath of an Exact entry is not the entry itself.
        assert!(!is_api_mount("/proc/1"));
        assert!(!is_api_mount("/run/initramfs"));
    }

    #[test]
    fn root_and_usr_are_extrinsic_but_not_api() {
        for path in ["/", "/usr"] {
            assert!(is_extrinsic_mount(path), "{path} is extrinsic");
            assert!(!is_api_mount(path), "{path} is not API");
            assert!(is_exempt_from_units(path), "{path} is exempt");
        }
    }

    #[test]
    fn gate_is_api_union_extrinsic() {
        for path in [
            "/proc",
            "/proc/sys/fs/binfmt_misc",
            "/sys",
            "/sys/kernel/debug",
            "/dev",
            "/dev/mqueue",
            "/run",
            "/run/host/x",
            "/run/initramfs",
            "/sys/fs/cgroup/system.slice",
            "/",
            "/usr",
            "/etc",
        ] {
            assert!(is_exempt_from_units(path), "{path} should be exempt");
        }

        // Neither table: these become units.
        for path in [
            "/home",
            "/tmp",
            "/var",
            "/usr/local",
            "/run/foo",
            "/mnt/data",
        ] {
            assert!(!is_exempt_from_units(path), "{path} should not be exempt");
        }
    }

    #[test]
    fn relative_paths_are_never_exempt() {
        assert!(!is_api_mount("proc"));
        assert!(!is_api_mount("sys/fs/cgroup"));
        assert!(!is_exempt_from_units(""));
        assert!(!is_extrinsic_mount("proc"));
    }

    #[test]
    fn specs_materialise_from_their_roots() {
        assert_eq!(CGROUP.path(), "/sys/fs/cgroup");
        assert_eq!(DEV_SHM.path(), "/dev/shm");
        assert_eq!(DEV_PTS.path(), "/dev/pts");
        assert_eq!(MountSpec::new(ROOT_PATH, "").path(), "/");
        assert_eq!(MountSpec::new(RUN_PATH, "host").path(), "/run/host");
        // A root that already ends in `/` must not double it up.
        assert_eq!(MountSpec::new("/run/", "host").path(), "/run/host");
    }

    #[test]
    fn parse_extras_keeps_only_absolute_paths() {
        assert_eq!(parse_extras(""), Vec::<String>::new());
        assert_eq!(parse_extras("   "), Vec::<String>::new());
        assert_eq!(parse_extras("proc"), Vec::<String>::new());
        assert_eq!(
            parse_extras("/srv/data::/opt/media/ : proc"),
            vec!["/srv/data".to_string(), "/opt/media/".to_string()]
        );
    }

    #[test]
    fn extra_mounts_widen_the_api_gate() {
        let extras = parse_extras("/srv/data:/opt/media/");

        // Exact entry: the mount point alone.
        assert!(is_api_mount_in("/srv/data", &extras));
        assert!(!is_api_mount_in("/srv/data/inner", &extras));

        // Trailing `/`: the whole subtree.
        assert!(is_api_mount_in("/opt/media", &extras));
        assert!(is_api_mount_in("/opt/media/one", &extras));
        assert!(!is_api_mount_in("/opt/mediax", &extras));

        // Additive only — built-ins still apply, nothing is ever removed.
        assert!(is_api_mount_in("/proc", &extras));
        assert!(!is_api_mount_in("/home", &extras));
    }
}
