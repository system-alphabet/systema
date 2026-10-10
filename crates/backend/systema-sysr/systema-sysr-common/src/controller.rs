//! The [`ResourceController`] trait, its error type and the no-op fallback.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;

use crate::config::ResourceConfig;

/// One process found inside a unit's cgroup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CgroupProcess {
    /// cgroup subpath relative to the unit's own cgroup (`""` = the unit's
    /// own cgroup, e.g. `system.slice/sshd.service` for a process in a
    /// descendant cgroup of the root slice).
    pub subpath: String,
    pub pid: u32,
    /// Process comm (from `/proc/<pid>/comm`); may be empty.
    pub name: String,
}

/// A snapshot of a unit's cgroup runtime metrics.
///
/// Values are keyed by the systemd property name they serve (e.g.
/// `"MemoryCurrent"`, `"CPUUsageNSec"`).  The metrics are opaque to every
/// layer above the controller: only the backend that reads cgroupfs knows
/// what the keys mean.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CgroupMetrics {
    /// systemd-style cgroup path, e.g. `/system.slice/app.service`.
    pub control_group: String,
    /// Inode of the unit's cgroup directory (0 when unknown).
    pub control_group_id: u64,
    /// Runtime metrics keyed by systemd property name.
    pub metrics: HashMap<String, u64>,
    /// Processes in the unit's cgroup subtree, each tagged with its subpath.
    pub processes: Vec<CgroupProcess>,
}

/// Errors from a [`ResourceController`] operation.
#[derive(Debug)]
pub enum ResourceError {
    /// Resource control is not available on this platform / configuration.
    Unavailable(String),
    /// A cgroupfs read or write failed.
    Io { path: String, source: std::io::Error },
    /// The cgroup still holds processes or child cgroups.
    NotEmpty(String),
    /// The value could not be applied to `path`.
    Invalid { path: String, message: String },
}

impl fmt::Display for ResourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResourceError::Unavailable(m) => write!(
                f,
                "{}",
                sysa::l10n::fmt(
                    sysa::l10n::t_("resource control unavailable: {m}"),
                    &[("m", &m.to_string())]
                )
            ),
            ResourceError::Io { path, source } => {
                write!(
                    f,
                    "{}",
                    sysa::l10n::fmt(
                        sysa::l10n::t_("cgroup operation on {path} failed: {source}"),
                        &[("path", &path.to_string()), ("source", &source.to_string())]
                    )
                )
            }
            ResourceError::NotEmpty(path) => write!(
                f,
                "{}",
                sysa::l10n::fmt(
                    sysa::l10n::t_("cgroup {path} is not empty"),
                    &[("path", &path.to_string())]
                )
            ),
            ResourceError::Invalid { path, message } => {
                write!(
                    f,
                    "{}",
                    sysa::l10n::fmt(
                        sysa::l10n::t_("cannot apply resource-control value to {path}: {message}"),
                        &[
                            ("path", &path.to_string()),
                            ("message", &message.to_string())
                        ]
                    )
                )
            }
        }
    }
}

impl std::error::Error for ResourceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ResourceError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// The cgroup hierarchy as it was before System R touched it.
///
/// Everything is recorded relative to the hierarchy root, so the value is
/// independent of where the backend has the root mounted.  [`dirs`](Self::dirs)
/// lists the cgroup directories that already existed (the root itself is
/// never listed, because it must never be removed); [`subtree_control`](Self::subtree_control)
/// maps each of those directories — the root under the empty key `""` — to the
/// verbatim contents of its `cgroup.subtree_control`.
///
/// The snapshot is what lets [`ResourceController::restore`] tell "created by
/// System R" apart from "was already there" without hard-coding any directory
/// name: the two things [`ResourceController::ensure`] does to an existing
/// tree are create directories and add controllers to `subtree_control`, and
/// the baseline records the prior value of both.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CgroupBaseline {
    /// cgroup directories that existed at snapshot time, relative to the root.
    pub dirs: BTreeSet<String>,
    /// Relative directory path (root = `""`) → its `cgroup.subtree_control`.
    pub subtree_control: BTreeMap<String, String>,
}

/// Backend for enforcing resource-control configuration on a cgroup
/// hierarchy.
///
/// Implementations are expected to be cheap, synchronous and idempotent so
/// they can be called from a worker's IPC handler without blocking for long.
pub trait ResourceController: Send + Sync {
    /// Whether resource control is usable on this system.  When `false`,
    /// callers should degrade gracefully (units still start unconstrained).
    fn available(&self) -> bool;

    /// Idempotently create the cgroup directory for `path` (including every
    /// ancestor), enable the controllers needed by `cfg` along the way, and
    /// apply the limits in `cfg` to the leaf.
    fn ensure(&self, path: &str, cfg: &ResourceConfig) -> Result<(), ResourceError>;

    /// Move process `pid` into the cgroup at `path`.
    ///
    /// `path` is expected to have been prepared with [`Self::ensure`].
    fn attach(&self, path: &str, pid: u32) -> Result<(), ResourceError>;

    /// List the PIDs directly present in `path` (not its child cgroups).
    fn processes(&self, path: &str) -> Vec<u32>;

    /// True when `path` has no child cgroup directories.
    fn has_child_cgroups(&self, path: &str) -> bool;

    /// Remove the empty cgroup at `path` (errors if it still holds
    /// processes or child cgroups).
    fn remove(&self, path: &str) -> Result<(), ResourceError>;

    /// Read a snapshot of the cgroup's runtime metrics at `path`.
    ///
    /// Best-effort: unreadable files are skipped and an empty snapshot is
    /// returned when the cgroup does not exist.  The systemd-style
    /// `control_group` path and the cgroup inode are filled in by the
    /// backend (which knows the mount point).
    fn metrics(&self, path: &str) -> CgroupMetrics;

    /// Record the hierarchy as it is now, before any [`Self::ensure`] runs.
    ///
    /// `None` means there is no cgroup filesystem to look at, in which case
    /// there is nothing for [`Self::restore`] to undo either.
    fn snapshot(&self) -> Option<CgroupBaseline>;

    /// Put the hierarchy back to `baseline`: remove every directory created
    /// since the snapshot, move the processes still sitting in them back to
    /// the root, and re-disable the controllers that were added to the
    /// directories that stay.
    ///
    /// Best-effort: a step that fails is logged and skipped, so a cgroup that
    /// will not go away can never keep the worker from exiting.
    fn restore(&self, baseline: &CgroupBaseline) -> Result<(), ResourceError>;
}

/// A controller that reports `available() == false` and is a no-op
/// everywhere else.  Used on non-Linux platforms and whenever the cgroup
/// v2 filesystem is not mounted.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopController;

impl ResourceController for NoopController {
    fn available(&self) -> bool {
        false
    }

    fn ensure(&self, _path: &str, _cfg: &ResourceConfig) -> Result<(), ResourceError> {
        Ok(())
    }

    fn attach(&self, _path: &str, _pid: u32) -> Result<(), ResourceError> {
        Ok(())
    }

    fn processes(&self, _path: &str) -> Vec<u32> {
        Vec::new()
    }

    fn has_child_cgroups(&self, _path: &str) -> bool {
        false
    }

    fn remove(&self, _path: &str) -> Result<(), ResourceError> {
        Ok(())
    }

    fn metrics(&self, _path: &str) -> CgroupMetrics {
        CgroupMetrics::default()
    }

    fn snapshot(&self) -> Option<CgroupBaseline> {
        None
    }

    fn restore(&self, _baseline: &CgroupBaseline) -> Result<(), ResourceError> {
        Ok(())
    }
}
