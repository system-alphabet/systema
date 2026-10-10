//! Device discovery.
//!
//! Two-tier model that degrades gracefully:
//! - **Sysfs backend** (Linux): the candidate set is `/dev` populated with
//!   real device nodes; each node is resolved back into sysfs via the
//!   canonical `/sys/dev/block|<maj>:<min>|` / `/sys/dev/char/...` links,
//!   and its `uevent` attributes become the property bag for `Property=`
//!   matching.
//! - **Devscan backend** (any Unix, or sysfs-less environments): falls back
//!   to the same `/dev` node scan with no sysfs metadata.
//!
//! The `/dev` scan is the anchor (density is bounded by the number of device
//! nodes); sysfs only augments identities.  This keeps FreeBSD/macOS and
//! chroot/containers working with identical code paths.

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use tracing::{debug, warn};

/// Where device nodes live.
pub const DEV_RUN: &str = "/dev";
/// Mount point of a Linux-compatible sysfs.
pub const SYSFS_ROOT: &str = "/sys";
/// Node prefixes that are virtual/noise and never become units.
const NOISE_DENYLIST: &[&str] = &["fc", "pty", "pts", "tty", "vcs", "vcsa", "ptmx"];

/// Which backend level is usable right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// A Linux-compatible sysfs is mounted and readable, so properties and
    /// `SysfsPath=` matching are available.
    pub sysfs: bool,
}

/// A single discovered device node.
#[derive(Debug, Clone, Default)]
pub struct DeviceMeta {
    /// Basename under `/dev`, e.g. "sda".
    pub node: String,
    /// Canonical `/dev` path, e.g. "/dev/sda".
    pub dev_file: String,
    /// Major/minor pair of the node.
    pub major: u32,
    pub minor: u32,
    /// Canonical sysfs dir of the device, when resolvable (None on the
    /// devscan backend).
    pub sysfs_path: Option<String>,
    /// Subsystem name, e.g. "block" / "tty".
    pub subsystem: Option<String>,
    /// Sysfs properties: the full `uevent` payload plus `SUBSYSTEM`,
    /// `DEVPATH` and `DEVTYPE`.
    pub properties: HashMap<String, String>,
}

/// Runtime probe of sysfs availability. Called once at startup and again
/// whenever discovery quietly fails, implementing the L2/L3 degradation.
pub fn probe_capabilities() -> Capabilities {
    Capabilities {
        sysfs: sysfs_readable(),
    }
}

fn sysfs_readable() -> bool {
    let dev = Path::new(SYSFS_ROOT).join("dev");
    for sub in ["block", "char"] {
        if !dev.join(sub).is_dir() {
            return false;
        }
    }
    true
}

/// Enumerate every `/dev` device node that should become a `.device` unit.
pub fn enumerate_devices() -> Vec<DeviceMeta> {
    let caps = probe_capabilities();
    scan_devices(caps)
}

fn scan_devices(caps: Capabilities) -> Vec<DeviceMeta> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(DEV_RUN) {
        Ok(e) => e,
        Err(e) => {
            warn!("Cannot read {DEV_RUN}: {e}");
            return out;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let name = entry.file_name().into_string().unwrap_or_default();
        if name.is_empty() || is_noise(&name) {
            continue;
        }
        let p = entry.path();
        // Follow symlinks (e.g. a by-id name or /dev/cdrom) but drop
        // dangling links.
        let md = match fs::metadata(&p) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let mode = md.mode();
        // mode() is u32 but the libc S_IF* constants are u16 on some BSDs,
        // so cast both sides to u32 for a portable comparison.
        let ifmt = libc::S_IFMT as u32;
        let is_dev_node = (mode & ifmt) == libc::S_IFBLK as u32
            || (mode & ifmt) == libc::S_IFCHR as u32;
        if !is_dev_node {
            continue;
        }
        let mut meta = DeviceMeta {
            node: name.clone(),
            dev_file: p.to_string_lossy().to_string(),
            // rdev() is u64 while libc::dev_t is platform-specific
            // (u64 on Linux, i32 on OpenBSD, ...): narrow it the way the
            // platform's own `major()`/`minor()` expect.
            major: libc::major(md.rdev() as libc::dev_t) as u32,
            minor: libc::minor(md.rdev() as libc::dev_t) as u32,
            ..Default::default()
        };
        if caps.sysfs {
            load_sysfs(&mut meta);
        }
        out.push(meta);
    }
    debug!("Device scan found {} nodes", out.len());
    out
}

/// Resolve a node's major/minor to its sysfs directory and load attributes.
fn load_sysfs(meta: &mut DeviceMeta) {
    let anchor = Path::new(SYSFS_ROOT).join("dev");
    for sub in ["block", "char"] {
        let ln = anchor.join(sub).join(format!("{}:{}", meta.major, meta.minor));
        if let Ok(target) = fs::canonicalize(&ln) {
            let sp = target.to_string_lossy().to_string();
            meta.sysfs_path = Some(sp.clone());
            meta.subsystem = Some(sub.to_string());
            load_sysfs_properties(meta, &sp);
            return;
        }
    }
    debug!(
        "No sysfs match for /dev/{} ({}:{}), proceeding without properties",
        meta.node, meta.major, meta.minor
    );
}

/// Fill `properties` from the device's `uevent` file plus a few standard
/// pointers (SUBSYSTEM, DEVPATH, DEVTYPE, DRIVER).
fn load_sysfs_properties(meta: &mut DeviceMeta, sysfs_path: &str) {
    let base = Path::new(sysfs_path);

    if let Ok(uevent) = fs::read_to_string(base.join("uevent")) {
        for (k, v) in parse_uevent(&uevent) {
            meta.properties.entry(k).or_insert(v);
        }
    }
    meta.properties
        .entry("SUBSYSTEM".to_string())
        .or_insert_with(|| meta.subsystem.clone().unwrap_or_default());

    let devpath = sysfs_path.trim_start_matches(SYSFS_ROOT);
    meta.properties
        .entry("DEVPATH".to_string())
        .or_insert_with(|| devpath.to_string());

    if let Ok(sl) = fs::read_link(base.join("subsystem")) {
        if let Some(name) = sl.file_name() {
            meta.properties
                .entry("SUBSYSTEM".to_string())
                .or_insert_with(|| name.to_string_lossy().to_string());
        }
    }
    if let Ok(dl) = fs::read_link(base.join("driver")) {
        if let Some(name) = dl.file_name() {
            meta.properties
                .entry("DRIVER".to_string())
                .or_insert_with(|| name.to_string_lossy().to_string());
        }
    }
}

/// Parse a sysfs `uevent` file ("KEY=VALUE" lines) into a property map.
pub fn parse_uevent(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.is_empty() || l.starts_with('#') {
                return None;
            }
            let (k, v) = l.split_once('=').unwrap_or((l, ""));
            Some((k.to_string(), v.to_string()))
        })
        .collect()
}

/// Should this `/dev` basename be skipped?
pub fn is_noise(name: &str) -> bool {
    NOISE_DENYLIST.iter().any(|p| name.starts_with(p))
}

#[cfg(test)]
mod tests {
    #[test]
    fn uevent_parsing() {
        let text = "MAJOR=8\nMINOR=0\nDEVNAME=sda\n\nDEVTYPE=disk\n";
        let props = super::parse_uevent(text);
        assert_eq!(props.get("MAJOR").map(|s| s.as_str()), Some("8"));
        assert_eq!(props.get("DEVNAME").map(|s| s.as_str()), Some("sda"));
        assert_eq!(props.get("DEVTYPE").map(|s| s.as_str()), Some("disk"));
    }

    #[test]
    fn noise_filtering() {
        assert!(super::is_noise("tty1"));
        assert!(super::is_noise("vcs1"));
        assert!(!super::is_noise("sda"));
        assert!(!super::is_noise("nvme0n1"));
    }
}