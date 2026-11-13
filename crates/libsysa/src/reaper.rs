//! Child-subreaper / reaper registration.
//!
//! On Linux and Android a process declares itself a child subreaper with
//! `PR_SET_CHILD_SUBREAPER`: orphaned descendants are reparented to it
//! instead of PID 1.  FreeBSD and DragonFly provide the equivalent
//! "reaper" role through `procctl(P_PID, 0, PROC_REAP_ACQUIRE, NULL)`.
//!
//! This module owns only the *registration*.  Collecting the adopted
//! children stays the caller's job: a process that acquires the role must
//! also run a `SIGCHLD`/`waitid` loop, or the orphans it attracts become
//! zombies.
//!
//! Platforms without either facility (OpenBSD, ...) treat the call as a
//! successful no-op, so callers do not need their own `#[cfg]` branches.

use std::io;

/// Register (`enabled = true`) or release (`false`) the child-subreaper /
/// reaper role for the calling process.
///
/// Returns the OS error when the kernel rejects the request.  The error
/// message names the underlying syscall (`PR_SET_CHILD_SUBREAPER` or
/// `PROC_REAP_ACQUIRE`/`PROC_REAP_RELEASE`).  On platforms without the
/// facility this is a no-op and always succeeds.
pub fn set_child_subreaper(enabled: bool) -> io::Result<()> {
    imp::set_child_subreaper(enabled)
}

/// Linux and Android: `prctl(PR_SET_CHILD_SUBREAPER, ...)`.
#[cfg(any(target_os = "linux", target_os = "android"))]
mod imp {
    use std::io;

    pub(super) fn set_child_subreaper(enabled: bool) -> io::Result<()> {
        // `nix` gates its `prctl` wrapper on `target_os = "linux"` alone, so
        // Android (whose bionic exposes the same prctl) calls libc directly.
        // The subreaper command reads only the second argument; the rest are
        // zero, matching nix's own `prctl_set_bool` body.
        let on = enabled as libc::c_ulong;
        // SAFETY: prctl(2) is variadic and side-effect free for this command.
        let rc = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, on, 0, 0, 0) };
        if rc == 0 {
            Ok(())
        } else {
            Err(with_syscall("PR_SET_CHILD_SUBREAPER"))
        }
    }

    fn with_syscall(name: &str) -> io::Error {
        let err = io::Error::last_os_error();
        io::Error::new(err.kind(), format!("{name}: {err}"))
    }
}

/// FreeBSD and DragonFly: `procctl(P_PID, 0, PROC_REAP_*, NULL)`.
#[cfg(any(target_os = "freebsd", target_os = "dragonfly"))]
mod imp {
    use std::io;

    pub(super) fn set_child_subreaper(enabled: bool) -> io::Result<()> {
        let (cmd, name) = if enabled {
            (libc::PROC_REAP_ACQUIRE, "PROC_REAP_ACQUIRE")
        } else {
            (libc::PROC_REAP_RELEASE, "PROC_REAP_RELEASE")
        };
        // SAFETY: procctl(P_PID, 0, cmd, NULL) operates on the calling
        // process; the reaper commands require a NULL data pointer.
        let rc = unsafe { libc::procctl(libc::P_PID, 0, cmd, std::ptr::null_mut()) };
        if rc == 0 {
            Ok(())
        } else {
            let err = io::Error::last_os_error();
            Err(io::Error::new(err.kind(), format!("{name}: {err}")))
        }
    }
}

/// Other platforms: no kernel facility, so the call is a no-op.
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd",
    target_os = "dragonfly"
)))]
mod imp {
    use std::io;

    pub(super) fn set_child_subreaper(_enabled: bool) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Linux `PR_SET_CHILD_SUBREAPER` is idempotent, so toggling it in the
    /// test process is safe and leaves the flag cleared afterwards.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn sets_and_clears_the_child_subreaper_flag() {
        set_child_subreaper(true).expect("enable child subreaper");
        set_child_subreaper(false).expect("disable child subreaper");
    }
}
