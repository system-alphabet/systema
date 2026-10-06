//! API filesystem mount setup for SysAInit.
//!
//! The kernel and the initramfs mount `/proc`, `/sys` and `/dev`, but
//! nothing mounts the cgroup v2 (unified) hierarchy, `/dev/shm` or
//! `/dev/pts`: systemd mounts them itself as PID 1 in `mount_setup()`
//! (`src/shared/mount-setup.c`, `mount_table`), so SysAInit does the same.
//!
//! The mounts are attempted only when SysAInit has the privileges to mount
//! (effective root: real root, or root inside a container / user
//! namespace).  In a rootless environment the mounts are skipped and
//! resource control degrades to the no-op controller, as if cgroup2 had
//! never been mounted.
//!
//! `/dev/shm` matters beyond POSIX shm: Wayland compositors using
//! wlroots (Hyprland, sway, ...) create their shared-memory buffers via
//! `shm_open(3)`, and without a tmpfs at `/dev/shm` they abort on
//! startup — a login session would die immediately after the greeter.

use std::ffi::CString;
use std::fs;

use anyhow::{anyhow, Context};
use tracing::{debug, error, info};

// The nix mount API differs between Linux (mount/umount2/MsFlags) and the
// BSDs (FreeBSD nmount/Nmount + unmount/MntFlags).  Platform-specific
// primitives live in `imp` so the rest of this module stays portable.
#[cfg(any(target_os = "linux", target_os = "android"))]
mod imp {
    use nix::errno::Errno;
    use nix::mount::{mount, umount2, MntFlags, MsFlags};

    /// Flags type accepted by the platform mount primitive.
    pub type Flags = MsFlags;

    /// Mount flags for the cgroup v2 hierarchy.
    pub fn cgroup_flags() -> Flags {
        MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC | MsFlags::MS_NODEV
    }
    /// Mount flags for the /dev/shm tmpfs.
    pub fn dev_shm_flags() -> Flags {
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_STRICTATIME
    }
    /// Mount flags for the /dev/pts devpts.
    pub fn dev_pts_flags() -> Flags {
        MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC
    }

    /// Mount `fstype` at `path`.  Returns the `Errno` of a failed mount.
    pub fn do_mount(
        fstype: &str,
        path: &str,
        options: &str,
        flags: Flags,
    ) -> Result<(), Errno> {
        mount(Some(fstype), path, Some(fstype), flags, Some(options))
    }

    /// Best-effort unmount of `path`.
    pub fn do_unmount(path: &str) -> Result<(), Errno> {
        umount2(path, MntFlags::UMOUNT_NOFOLLOW)
    }

    /// True when `e` is `EBUSY` (target already occupied by another fs).
    pub fn is_busy(e: &Errno) -> bool {
        *e == Errno::EBUSY
    }
}

#[cfg(target_os = "freebsd")]
mod imp {
    use nix::errno::Errno;
    use nix::mount::{unmount, MntFlags, Nmount};

    /// Flags type accepted by the platform mount primitive.
    pub type Flags = MntFlags;

    /// Mount flags for the cgroup v2 hierarchy.
    pub fn cgroup_flags() -> Flags {
        MntFlags::MNT_NOSUID | MntFlags::MNT_NOEXEC
    }
    /// Mount flags for the /dev/shm tmpfs.
    pub fn dev_shm_flags() -> Flags {
        MntFlags::MNT_NOSUID | MntFlags::MNT_NOEXEC
    }
    /// Mount flags for the /dev/pts devpts.
    pub fn dev_pts_flags() -> Flags {
        MntFlags::MNT_NOSUID | MntFlags::MNT_NOEXEC
    }

    /// Mount `fstype` at `path`.  Returns the `Errno` of a failed mount.
    ///
    /// FreeBSD's `nmount(2)` takes `name=value` pairs; the Linux-style
    /// `options` string is best-effort (it may be empty), the essential
    /// `fstype`/`fspath` pair is always supplied.
    pub fn do_mount(
        fstype: &str,
        path: &str,
        options: &str,
        _flags: Flags,
    ) -> Result<(), Errno> {
        let mut nm = Nmount::new();
        nm.str_opt_owned("fstype", fstype)
            .str_opt_owned("fspath", path);
        if !options.is_empty() {
            nm.str_opt_owned("options", options);
        }
        nm.nmount(MntFlags::empty()).map_err(|e| e.error())
    }

    /// Best-effort unmount of `path`.
    pub fn do_unmount(path: &str) -> Result<(), Errno> {
        unmount(path, MntFlags::MNT_FORCE)
    }

    /// True when `e` is `EBUSY` (target already occupied by another fs).
    pub fn is_busy(e: &Errno) -> bool {
        *e == Errno::EBUSY
    }
}

// Mount targets are derived from `sysa::mounts` rather than spelled out here,
// so the root paths that gate discovery and the paths SysAInit actually
// mounts cannot drift apart.  Only the mount *parameters* — flags, options and
// their systemd `mount_table` provenance — are this module's business.

/// Mount options for cgroup2, kept in sync with systemd's mount-table
/// entry (`nsdelegate,memory_recursiveprot`).
const CGROUP_OPTIONS: &str = "nsdelegate,memory_recursiveprot";

/// POSIX shared memory tmpfs, kept in sync with systemd's mount-table
/// entry (`mode=01777`, MS_NOSUID|MS_NODEV|MS_STRICTATIME).
const DEV_SHM_OPTIONS: &str = "mode=01777";

/// /dev/pts devpts, kept in sync with systemd's mount-table entry
/// (`mode=0620,gid=5`, MS_NOSUID|MS_NOEXEC).
const DEV_PTS_OPTIONS: &str = "mode=0620,gid=5";

/// Whether SysAInit may perform mounts: the effective user must be root.
///
/// Real root and root-inside-a-userns/container both yield euid 0 and
/// both may mount cgroup2 in their own (cgroup) namespace; a rootless
/// environment runs with a non-root euid and must skip the mount.
fn has_mount_privileges() -> bool {
    // SAFETY: geteuid(2) is always successful and side-effect free.
    (unsafe { nix::libc::geteuid() }) == 0
}

/// Check whether `path` is currently a mount point.
///
/// Parses `/proc/self/mountinfo` instead of statfs, mirroring systemd's
/// `path_is_mount_point_full()`: bind mounts and stacked mounts are
/// handled correctly, and no filesystem interaction is required.
fn is_mount_point(path: &str) -> bool {
    let Ok(mountinfo) = fs::read_to_string("/proc/self/mountinfo") else {
        debug!("Cannot read /proc/self/mountinfo; assuming {path} is not a mount point");
        return false;
    };
    mountinfo.lines().any(|line| line.split(' ').nth(4) == Some(path))
}

/// Mount a filesystem at `path`, mirroring systemd's `mount_table`
/// handling: skip when already mounted or without privileges, create the
/// mount point, mount, then undo when the result is not writable
/// (systemd's MNT_CHECK_WRITABLE).
fn mount_table_entry(
    fstype: &str,
    path: &str,
    options: &str,
    flags: imp::Flags,
) -> anyhow::Result<()> {
    if !has_mount_privileges() {
        debug!("Running without mount privileges (rootless); not mounting {fstype} at {path}");
        return Ok(());
    }

    if is_mount_point(path) {
        info!("{fstype} already mounted at {path}; not mounting again");
        return Ok(());
    }

    fs::create_dir_all(path).with_context(|| {
        sysa::l10n::fmt(
            sysa::l10n::t_("Cannot create mount point {path}"),
            &[("path", &path.to_string())],
        )
    })?;

    imp::do_mount(fstype, path, options, flags).map_err(|e| {
        if imp::is_busy(&e) {
            anyhow!(sysa::l10n::fmt(
                sysa::l10n::t_("{path} is already occupied by another filesystem"),
                &[("path", &path.to_string())]
            ))
        } else {
            anyhow!(sysa::l10n::fmt(
                sysa::l10n::t_("Cannot mount {fstype} at {path}: {e}"),
                &[
                    ("fstype", &fstype.to_string()),
                    ("path", &path.to_string()),
                    ("e", &e.to_string())
                ]
            ))
        }
    })?;

    // systemd's MNT_CHECK_WRITABLE: undo the mount when the filesystem
    // is not actually writable.
    // SAFETY: access(2) only touches errno and returns -1 on failure.
    let c_path = CString::new(path).expect("mount point path must not contain interior NUL");
    if unsafe { nix::libc::access(c_path.as_ptr(), nix::libc::W_OK) } != 0 {
        let err = std::io::Error::last_os_error();
        let _ = imp::do_unmount(path);
        let _ = fs::remove_dir(path);
        return Err(anyhow!(sysa::l10n::fmt(
            sysa::l10n::t_("{fstype} mount at {path} is not writable, undoing: {err}"),
            &[
                ("fstype", &fstype.to_string()),
                ("path", &path.to_string()),
                ("err", &err.to_string())
            ]
        )));
    }

    info!("Mounted {fstype} at {path} ({options})");
    Ok(())
}

/// Mount the cgroup v2 hierarchy at `/sys/fs/cgroup`, mirroring systemd.
///
/// Returns `Ok(())` when cgroup2 is already mounted, was mounted now, or
/// when SysAInit lacks the privileges to mount (rootless environment).
/// Returns `Err` when mounting should have been possible but failed.
pub fn mount_cgroup2() -> anyhow::Result<()> {
    let path = sysa::mounts::CGROUP.path();

    if !has_mount_privileges() {
        debug!("Running without mount privileges (rootless); not mounting cgroup2");
        return Ok(());
    }

    if is_mount_point(&path) {
        info!("cgroup2 already mounted at {path}; not mounting again");
        return Ok(());
    }

    fs::create_dir_all(&path).with_context(|| {
        sysa::l10n::fmt(
            sysa::l10n::t_("Cannot create cgroup mount point {path}"),
            &[("path", &path.to_string())],
        )
    })?;

    imp::do_mount("cgroup2", &path, CGROUP_OPTIONS, imp::cgroup_flags()).map_err(|e| {
        if imp::is_busy(&e) {
            anyhow!(sysa::l10n::fmt(sysa::l10n::t_("{path} is already occupied by another filesystem (cgroup v1?); hybrid cgroup hierarchy is not supported"), &[("path", &path.to_string())]))
        } else {
            anyhow!(sysa::l10n::fmt(sysa::l10n::t_("Cannot mount cgroup2 at {path}: {e}"), &[("path", &path.to_string()), ("e", &e.to_string())]))
        }
    })?;

    // systemd's MNT_CHECK_WRITABLE: undo the mount when the filesystem
    // is not actually writable.
    // SAFETY: access(2) only touches errno and returns -1 on failure.
    let c_cgroup = CString::new(path.as_str()).expect("cgroup path must not contain interior NUL");
    if unsafe { nix::libc::access(c_cgroup.as_ptr(), nix::libc::W_OK) } != 0 {
        let err = std::io::Error::last_os_error();
        let _ = imp::do_unmount(&path);
        let _ = fs::remove_dir(&path);
        return Err(anyhow!(sysa::l10n::fmt(
            sysa::l10n::t_("cgroup2 mount at {path} is not writable, undoing: {err}"),
            &[("path", &path.to_string()), ("err", &err.to_string())]
        )));
    }

    info!("Mounted cgroup2 at {path} ({CGROUP_OPTIONS})");
    Ok(())
}

/// Mount a tmpfs at `/dev/shm` for POSIX shared memory (systemd's
/// mount-table entry: tmpfs, `mode=01777`,
/// `MS_NOSUID|MS_NODEV|MS_STRICTATIME`).  wlroots-based compositors abort
/// without it.  Never fatal: like systemd, the failure is logged by the
/// caller and the boot continues.
pub fn mount_dev_shm() -> anyhow::Result<()> {
    let path = sysa::mounts::DEV_SHM.path();
    mount_table_entry("tmpfs", &path, DEV_SHM_OPTIONS, imp::dev_shm_flags())
}

/// Mount devpts at `/dev/pts` (systemd's mount-table entry: devpts,
/// `mode=0620,gid=5`, `MS_NOSUID|MS_NOEXEC`).  Inside containers
/// (nspawn) the kernel's default `/dev/pts` lacks `newinstance`, so the
/// pseudo-terminal devpts must be remounted; harmless when already
/// mounted by devtmpfs.  Never fatal.
pub fn mount_dev_pts() -> anyhow::Result<()> {
    let path = sysa::mounts::DEV_PTS.path();
    mount_table_entry("devpts", &path, DEV_PTS_OPTIONS, imp::dev_pts_flags())
}

/// Unmount the runstatedir if it is a mount point.
///
/// Best-effort: logs the outcome but never returns an error (the caller
/// continues regardless).  Only attempted when SysAInit has mount
/// privileges (i.e. is root).
pub fn umount_runstatedir(path: &str) {
    if !has_mount_privileges() {
        debug!("Running without mount privileges; not unmounting {path}");
        return;
    }
    if !is_mount_point(path) {
        debug!("{path} is not a mount point; nothing to unmount");
        return;
    }
    match imp::do_unmount(path) {
        Ok(()) => {
            info!("Unmounted {path}");
        }
        Err(e) => {
            error!("Failed to unmount {path}: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_is_a_mount_point() {
        assert!(is_mount_point("/"));
    }

    #[test]
    fn bogus_path_is_not_a_mount_point() {
        assert!(!is_mount_point("/definitely/not/a/real/mountpoint"));
    }

    #[test]
    fn non_privileged_mounts_are_noops() {
        if unsafe { nix::libc::geteuid() } == 0 {
            // Real root may actually mount; only assert the rootless path.
            return;
        }
        assert!(mount_dev_shm().is_ok());
        assert!(mount_dev_pts().is_ok());
    }
}