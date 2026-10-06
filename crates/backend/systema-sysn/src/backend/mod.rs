//! Filesystem watch backends.
//!
//! The engine treats backend events purely as wake-up signals: it re-evaluates
//! conditions from the live filesystem (stat/scan) rather than trusting event
//! payloads.  This keeps the three backends (inotify, kqueue, polling) simple
//! and behaviour uniform across platforms.

use async_trait::async_trait;

use crate::spec::PathSpec;

/// Neutral filesystem change bits, translated from backend-native flags
/// (IN_*, NOTE_*).  Multiple bits may be set for one wake-up.
#[derive(Debug, Clone, Copy, Default)]
pub struct FsEvent {
    pub created: bool,
    pub deleted: bool,
    pub renamed: bool,
    pub attrib: bool,
    /// Content was written and the writing fd closed.
    pub closed_write: bool,
    /// Content was written (may also be true for directory entry changes on
    /// backends that cannot distinguish them).
    pub written: bool,
}

impl FsEvent {
    /// Events that satisfy a `PathChanged=` spec (systemd v255 semantics):
    /// creation, deletion, rename, or a close-after-write.  Bare in-place
    /// writes and attribute-only changes (chmod) do *not* satisfy it.
    pub fn satisfies_changed(&self) -> bool {
        self.created || self.deleted || self.renamed || self.closed_write
    }

    /// Events that satisfy a `PathModified=` spec (systemd v255 semantics):
    /// any content change, including simple writes.  Attribute-only changes
    /// do *not* satisfy it.
    pub fn satisfies_modified(&self) -> bool {
        self.created || self.deleted || self.renamed || self.closed_write || self.written
    }
}

/// A change observed for one path unit.
#[derive(Debug)]
pub struct PathChange {
    pub unit: String,
    pub event: FsEvent,
}

#[async_trait]
pub trait PathBackend: Send + Sync {
    /// (Re)place the watches for `unit` with the given specs.  Idempotent:
    /// arming an already-armed unit replaces its watches.
    fn arm(&self, unit: &str, specs: &[PathSpec]) -> anyhow::Result<()>;

    /// Remove all watches for `unit`.
    fn disarm(&self, unit: &str);

    /// All currently armed units (used to re-scan everything after an
    /// event queue overflow).
    fn armed_units(&self) -> Vec<String>;

    /// Block until at least one change is available, then return the
    /// affected units.  May return an empty list on spurious wake-ups.
    async fn changes(&self) -> Vec<PathChange>;
}

/// Create the platform-appropriate backend for this host.
pub fn default_backend() -> anyhow::Result<Box<dyn PathBackend>> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        Ok(Box::new(crate::backend::linux::InotifyBackend::new()?))
    }
    #[cfg(any(
        target_os = "freebsd",
        target_os = "macos",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    {
        Ok(Box::new(crate::backend::kqueue::KqueueBackend::new()?))
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "macos",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    )))]
    {
        Ok(Box::new(crate::backend::polling::PollingBackend::new()))
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
pub mod linux;
#[cfg(any(
    target_os = "freebsd",
    target_os = "macos",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
pub mod kqueue;
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd",
    target_os = "macos",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
)))]
pub mod polling;

/// Watched path targets for one spec.
struct WatchTargets {
    /// Parent directory of the path (always watched).
    parent: String,
    /// The path itself, when it exists (catches in-place writes on files and
    /// entry changes on directories).
    itself: Option<String>,
}

impl WatchTargets {
    /// Compute the set of paths to watch for one spec.
    fn for_spec(spec: &PathSpec) -> WatchTargets {
        let path = spec.path.clone();
        let parent = match path.rfind('/') {
            Some(0) => "/".to_string(),
            Some(idx) => path[..idx].to_string(),
            None => ".".to_string(),
        };
        let itself = match spec.kind {
            // Exists/ExistsGlob watch the parent so creation/deletion fires;
            // no need to watch the (non-existent) path itself.
            crate::spec::PathSpecKind::Exists | crate::spec::PathSpecKind::ExistsGlob => None,
            _ => {
                if std::path::Path::new(&path).exists() {
                    Some(path)
                } else {
                    None
                }
            }
        };
        WatchTargets { parent, itself }
    }

    /// All paths to watch for a spec, deduplicated by the caller.
    fn into_paths(self) -> Vec<String> {
        let mut out = vec![self.parent];
        if let Some(itself) = self.itself {
            out.push(itself);
        }
        out
    }
}
