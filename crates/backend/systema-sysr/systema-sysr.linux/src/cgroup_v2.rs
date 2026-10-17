//! cgroup v2 implementation of [`ResourceController`].

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

use systema_sysr_common::{
    CGROUP_ROOT, CgroupBaseline, CgroupMetrics, CgroupProcess, DEFAULT_TASKS_MAX,
    ResourceConfig, ResourceController, ResourceError, split_device_directive,
};
use tracing::{debug, warn};

/// Controllers System R enables in every ancestor cgroup, in the order they
/// are enabled.  `cpuset` must be enabled before `cpu`/`memory` on kernels
/// that compile it in, and `io` depends on the I/O scheduling layer.
const CONTROLLERS: &[&str] = &["cpuset", "cpu", "memory", "pids", "io"];

/// cgroup v2 backend backed by the unified hierarchy at `/sys/fs/cgroup`.
pub struct CgroupV2Controller {
    root: PathBuf,
}

impl CgroupV2Controller {
    pub fn new() -> Self {
        CgroupV2Controller {
            root: PathBuf::from(CGROUP_ROOT),
        }
    }

    /// Detect a usable unified cgroup hierarchy.
    ///
    /// A cgroup v2 mount is identified by the `cgroup.controllers` marker
    /// file at the mount point.  When it is missing (cgroup v1, or no
    /// cgroup filesystem at all), resource control is unavailable and
    /// callers fall back to the no-op controller.
    pub fn detect() -> Option<Self> {
        let marker = Path::new(CGROUP_ROOT).join("cgroup.controllers");
        if !marker.exists() {
            return None;
        }
        Some(Self::new())
    }

    /// Relative path of `path` under the cgroup root, or `None` when `path`
    /// is outside the hierarchy.
    fn rel(&self, path: &str) -> Option<PathBuf> {
        Path::new(path).strip_prefix(&self.root).ok().map(|p| p.to_path_buf())
    }

    /// Absolute filesystem path for a relative cgroup path (`""` = root).
    fn full(&self, rel: &Path) -> PathBuf {
        if rel.as_os_str().is_empty() {
            self.root.clone()
        } else {
            self.root.join(rel)
        }
    }

    /// Enable every requested controller in `dir`'s `cgroup.subtree_control`.
    ///
    /// This is best-effort: a controller that cannot be enabled (not
    /// compiled in, already bound elsewhere) is logged and skipped.  A cgroup
    /// can only create *children* that use a controller when the controller
    /// is enabled in the parent's subtree_control, so this is called on every
    /// ancestor before the child directories are created.
    fn enable_controllers(&self, dir: &Path) {
        self.enable_controllers_list(dir, &CONTROLLERS);
    }

    /// Enable each of `names` in `dir`'s `cgroup.subtree_control`.
    ///
    /// Same best-effort policy as [`Self::enable_controllers`]: a controller
    /// that cannot be enabled (not compiled in, already bound elsewhere) is
    /// logged and skipped.
    fn enable_controllers_list(&self, dir: &Path, names: &[&str]) {
        let current = match read_file(&dir.join("cgroup.subtree_control")) {
            Ok(s) => s,
            Err(e) => {
                warn!(
                    "Cannot read cgroup.subtree_control on {}: {}",
                    dir.display(),
                    e
                );
                return;
            }
        };
        let enabled: HashSet<&str> = current
            .split_whitespace()
            .filter_map(|t| t.strip_prefix('+'))
            .collect();

        let to_enable: Vec<String> = names
            .iter()
            .filter(|c| !enabled.contains(**c))
            .map(|c| format!("+{c}"))
            .collect();
        if to_enable.is_empty() {
            return;
        }

        // Write the whole batch first; if that fails (one controller is
        // unavailable), fall back to enabling controllers one at a time so a
        // single bad controller does not block the rest.
        if write_file(&dir.join("cgroup.subtree_control"), &to_enable.join(" ")).is_ok() {
            debug!("Enabled controllers {:?} on {}", to_enable, dir.display());
            return;
        }
        for entry in &to_enable {
            match write_file(&dir.join("cgroup.subtree_control"), entry) {
                Ok(()) => debug!("Enabled controller {entry} on {}", dir.display()),
                Err(e) => warn!(
                    "Cannot enable controller {entry} on {}: {}",
                    dir.display(),
                    e
                ),
            }
        }
    }

    /// Apply `cfg` to the leaf cgroup `dir`.
    ///
    /// Limit writes are best-effort: a limit that cannot be applied (e.g.
    /// its controller is unavailable) is logged and skipped, so resource
    /// control degrades gracefully without failing the unit start.
    fn apply_limits(&self, dir: &Path, cfg: &ResourceConfig) {
        if cfg.is_empty() {
            return;
        }

        if let Some(v) = cfg.cpu_max() {
            self.write_limit(dir, "cpu.max", &v);
        }
        if let Some(w) = cfg.cpu_weight_v2() {
            self.write_limit(dir, "cpu.weight", &w.to_string());
        }
        if let Some(b) = cfg.memory_min_bytes() {
            self.write_limit(dir, "memory.min", &b.to_string());
        }
        if let Some(b) = cfg.memory_low_bytes() {
            self.write_limit(dir, "memory.low", &b.to_string());
        }
        if let Some(b) = cfg.memory_high_bytes() {
            self.write_limit(dir, "memory.high", &b.to_string());
        }
        if let Some(b) = cfg.memory_max_bytes() {
            self.write_limit(dir, "memory.max", &b.to_string());
        }
        if let Some(b) = cfg.memory_swap_max_bytes() {
            self.write_limit(dir, "memory.swap.max", &b.to_string());
        }
        if let Some(w) = cfg.io_weight_v2() {
            self.write_limit(dir, "io.weight", &w.to_string());
        }
        if let Some(p) = cfg.pids_max() {
            self.write_limit(dir, "pids.max", &p);
        }

        let cpus = pick(&cfg.allowed_cpus, &cfg.cpu_set_cpus);
        if !cpus.is_empty() {
            self.write_limit(dir, "cpuset.cpus", cpus);
        }
        let mems = pick(&cfg.allowed_memory_nodes, &cfg.cpu_set_memory_nodes);
        if !mems.is_empty() {
            self.write_limit(dir, "cpuset.mems", mems);
        }

        self.apply_io_device_limits(dir, cfg);
    }

    /// Apply per-device I/O limits (`io.weight` and `io.max`).
    ///
    /// Devices may be given as `/dev` paths or `MAJ:MIN` ids; paths are
    /// resolved with `stat(2)` so symlinks under `/dev/disk/by-*` work.
    fn apply_io_device_limits(&self, dir: &Path, cfg: &ResourceConfig) {
        for line in &cfg.io_device_weight {
            let Some((device, value)) = split_device_directive(line) else {
                continue;
            };
            let Some(id) = resolve_device(device) else {
                warn!("Cannot resolve device '{device}' for io.weight on {}", dir.display());
                continue;
            };
            self.write_limit(dir, "io.weight", &format!("{id} {value}"));
        }

        // Group read/write bandwidth limits per device so each `io.max`
        // write carries both directions.
        let mut max: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for line in &cfg.io_read_bandwidth_max {
            if let Some((device, value)) = split_device_directive(line) {
                if let Some(id) = resolve_device(device) {
                    max.entry(id).or_default().push(format!("rbps={value}"));
                } else {
                    warn!("Cannot resolve device '{device}' for io.max on {}", dir.display());
                }
            }
        }
        for line in &cfg.io_write_bandwidth_max {
            if let Some((device, value)) = split_device_directive(line) {
                if let Some(id) = resolve_device(device) {
                    max.entry(id).or_default().push(format!("wbps={value}"));
                } else {
                    warn!("Cannot resolve device '{device}' for io.max on {}", dir.display());
                }
            }
        }
        for (id, fields) in max {
            self.write_limit(dir, "io.max", &format!("{id} {}", fields.join(" ")));
        }
    }

    fn write_limit(&self, dir: &Path, file: &str, value: &str) {
        let path = dir.join(file);
        if let Err(e) = write_file(&path, value) {
            warn!(
                "Cannot apply {file}={value} on {}: {}",
                dir.display(),
                e
            );
        }
    }
}

/// Return the first non-empty of `a`, `b`.
fn pick<'a>(a: &'a str, b: &'a str) -> &'a str {
    if !a.is_empty() {
        a
    } else if !b.is_empty() {
        b
    } else {
        ""
    }
}

/// Resolve a per-device directive's device to a cgroup v2 `MAJ:MIN` id.
///
/// `"8:0"` is passed through unchanged; any other value is treated as a
/// filesystem path (e.g. `/dev/sda` or a `/dev/disk/by-id/...` symlink) and
/// resolved through `stat(2)`.  Returns `None` when the device is neither a
/// numeric id nor a resolvable block/char device node.
fn resolve_device(device: &str) -> Option<String> {
    if let Some((maj, min)) = device.split_once(':') {
        let digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
        if digits(maj) && digits(min) {
            return Some(device.to_string());
        }
    }
    let meta = fs::metadata(device).ok()?;
    let ft = meta.file_type();
    if !ft.is_block_device() && !ft.is_char_device() {
        return None;
    }
    let dev = meta.rdev();
    let major = (dev >> 8) & 0xfff;
    let minor = (dev & 0xff) | ((dev >> 12) & 0xfff00);
    Some(format!("{major}:{minor}"))
}

/// Read a single numeric value from a cgroup file (`"512\n"`, `"max\n"`).
/// `"max"` and unreadable/absent files yield `None`.
fn read_number(path: &Path) -> Option<u64> {
    let s = read_file(path).ok()?;
    let v = s.trim();
    if v.is_empty() || v == "max" {
        return None;
    }
    v.parse().ok()
}

/// Read a `"key value"` pair from a cgroup file (e.g. `cpu.stat`).
fn read_key_value(path: &Path, key: &str) -> Option<u64> {
    let s = read_file(path).ok()?;
    s.lines().find_map(|l| {
        let mut it = l.split_whitespace();
        if it.next()? == key {
            it.next().and_then(|v| v.parse().ok())
        } else {
            None
        }
    })
}

/// Aggregate the per-device `io.stat` lines into the systemd I/O properties.
fn collect_io_stat(dir: &Path, metrics: &mut HashMap<String, u64>) {
    let Ok(s) = read_file(&dir.join("io.stat")) else {
        return;
    };
    let mut read_bytes = 0u64;
    let mut read_ops = 0u64;
    let mut write_bytes = 0u64;
    let mut write_ops = 0u64;
    for line in s.lines() {
        for field in line.split_whitespace().skip(1) {
            let Some((k, v)) = field.split_once('=') else {
                continue;
            };
            let Ok(v) = v.parse::<u64>() else {
                continue;
            };
            match k {
                "rbytes" => read_bytes = read_bytes.saturating_add(v),
                "rios" => read_ops = read_ops.saturating_add(v),
                "wbytes" => write_bytes = write_bytes.saturating_add(v),
                "wios" => write_ops = write_ops.saturating_add(v),
                _ => {}
            }
        }
    }
    metrics.insert("IOReadBytes".to_string(), read_bytes);
    metrics.insert("IOReadOperations".to_string(), read_ops);
    metrics.insert("IOWriteBytes".to_string(), write_bytes);
    metrics.insert("IOWriteOperations".to_string(), write_ops);
}

/// Read the PIDs in `dir`'s `cgroup.procs` and resolve their comm, walking
/// into every descendant cgroup so `systemctl status` can render the whole
/// subtree.  Each process carries its subpath relative to `dir` (`""` for
/// processes directly in `dir`, e.g. `system.slice/sshd.service`).
fn read_processes(dir: &Path) -> Vec<CgroupProcess> {
    let mut processes = Vec::new();
    collect_processes(dir, "", &mut processes);
    processes
}

/// Recursive half of [`read_processes`]: read `dir`'s own `cgroup.procs`,
/// then descend into every child cgroup directory.
fn collect_processes(dir: &Path, subpath: &str, out: &mut Vec<CgroupProcess>) {
    if let Ok(s) = read_file(&dir.join("cgroup.procs")) {
        for line in s.lines() {
            let Ok(pid) = line.trim().parse::<u32>() else {
                continue;
            };
            let name = fs::read_to_string(format!("/proc/{pid}/comm"))
                .ok()
                .map(|c| c.trim().to_string())
                .unwrap_or_default();
            out.push(CgroupProcess {
                subpath: subpath.to_string(),
                pid,
                name,
            });
        }
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        let child_subpath = if subpath.is_empty() {
            name.clone()
        } else {
            format!("{subpath}/{name}")
        };
        collect_processes(&path, &child_subpath, out);
    }
}

fn read_file(path: &Path) -> std::io::Result<String> {
    fs::read_to_string(path)
}

fn write_file(path: &Path, value: &str) -> std::io::Result<()> {
    fs::write(path, value)
}

/// Resolve verbatim `User=`/`Group=` values into a `(uid, gid)` pair.
///
/// Mirrors System S's credential resolution so both sides agree on the
/// owner: names resolve through nss, a numeric value falls back to the
/// uid/gid directly (`User=1000` works without an /etc/passwd name match,
/// like systemd's `get_user_creds()`), a user without an explicit group
/// takes the user's primary group, and an empty pair resolves to the
/// cgroup's current (root) owner.  A name that matches nothing is an error
/// — failing the start job visibly beats spawning a unit into a cgroup
/// whose owner could not be established.
fn resolve_owner(user: &str, group: &str) -> Result<(u32, u32), String> {
    use nix::unistd::{Gid, Group, Uid, User};

    let mut uid: u32 = 0;
    let mut gid: u32 = 0;

    if !user.is_empty() {
        let found = User::from_name(user).map_err(|e| {
            sysa::l10n::fmt(
                sysa::l10n::t_("lookup of user '{user}' failed: {e}"),
                &[("user", &user.to_string()), ("e", &e.to_string())],
            )
        })?;
        let usr = found
            .or_else(|| {
                user.parse::<u32>()
                    .ok()
                    .and_then(|n| User::from_uid(Uid::from_raw(n)).ok().flatten())
            })
            .ok_or_else(|| {
                sysa::l10n::fmt(
                    sysa::l10n::t_("user '{user}' not found"),
                    &[("user", &user.to_string())],
                )
            })?;
        uid = usr.uid.as_raw();
        gid = usr.gid.as_raw();
    }

    if !group.is_empty() {
        let found = Group::from_name(group).map_err(|e| {
            sysa::l10n::fmt(
                sysa::l10n::t_("lookup of group '{group}' failed: {e}"),
                &[("group", &group.to_string()), ("e", &e.to_string())],
            )
        })?;
        let grp = found
            .or_else(|| {
                group
                    .parse::<u32>()
                    .ok()
                    .and_then(|n| Group::from_gid(Gid::from_raw(n)).ok().flatten())
            })
            .ok_or_else(|| {
                sysa::l10n::fmt(
                    sysa::l10n::t_("group '{group}' not found"),
                    &[("group", &group.to_string())],
                )
            })?;
        gid = grp.gid.as_raw();
    }

    Ok((uid, gid))
}

/// Chown `path` to `uid:gid` through raw `chown(2)` (std has no portable
/// owner setter; the path comes from the cgroup hierarchy and never holds
/// interior NULs).
fn chown_one(path: &Path, uid: u32, gid: u32) -> Result<(), ResourceError> {
    let cpath = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).map_err(|_| {
        ResourceError::Io {
            path: path.display().to_string(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in cgroup path"),
        }
    })?;
    if unsafe { libc::chown(cpath.as_ptr(), uid, gid) } != 0 {
        return Err(ResourceError::Io {
            path: path.display().to_string(),
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(())
}

/// Recursively chown the cgroup directory `path` to `uid:gid`.
///
/// Everything inside must be chowned too: the `cgroup.procs`,
/// `cgroup.subtree_control` & co control files are created root-owned by
/// the kernel, and chowning only the directory would leave them
/// inaccessible to the delegate.
fn chown_recursive(path: &Path, uid: u32, gid: u32) -> Result<(), ResourceError> {
    chown_one(path, uid, gid)?;
    let entries = fs::read_dir(path).map_err(|source| ResourceError::Io {
        path: path.display().to_string(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| ResourceError::Io {
            path: path.display().to_string(),
            source,
        })?;
        let child = entry.path();
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            chown_recursive(&child, uid, gid)?;
        } else {
            chown_one(&child, uid, gid)?;
        }
    }
    Ok(())
}

/// Mark `dir` as a delegated cgroup with both delegate xattr spellings
/// (`user.delegate`, `trusted.delegate`) — the markers systemd and the
/// OCI tooling look for.  Best-effort: a kernel without cgroup xattr
/// support only loses the marker, never the ownership above.
fn set_delegate_xattrs(dir: &Path) {
    let Ok(cpath) = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()) else {
        return;
    };
    for attr in [c"user.delegate", c"trusted.delegate"] {
        let rc = unsafe {
            libc::setxattr(
                cpath.as_ptr(),
                attr.as_ptr(),
                b"1".as_ptr().cast(),
                b"1".len(),
                0,
            )
        };
        if rc != 0 {
            warn!(
                "Cannot set {} on {}: {}",
                attr.to_string_lossy(),
                dir.display(),
                std::io::Error::last_os_error()
            );
        }
    }
}

/// Every cgroup directory at and below `root`, as paths relative to it, with
/// the root itself as `""`.  Parents come before their children.
fn walk_cgroups(root: &Path) -> Vec<String> {
    fn rec(dir: &Path, rel: &str, out: &mut Vec<String>) {
        out.push(rel.to_string());
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
                continue;
            };
            let child = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            rec(&path, &child, out);
        }
    }

    let mut out = Vec::new();
    rec(root, "", &mut out);
    out
}

/// The PIDs listed in `dir`'s `cgroup.procs`.
fn read_pids(dir: &Path) -> Vec<u32> {
    read_file(&dir.join("cgroup.procs"))
        .map(|s| s.lines().filter_map(|l| l.trim().parse().ok()).collect())
        .unwrap_or_default()
}

/// The `-controller` entries that turn `dir`'s `cgroup.subtree_control` back
/// into `keep` — i.e. every controller enabled there that `keep` did not
/// record.  `None` when the directory (or its control file) is gone, so
/// there is nothing left to undo.
fn extra_controllers(dir: &Path, keep: &str) -> Option<Vec<String>> {
    let current = read_file(&dir.join("cgroup.subtree_control")).ok()?;
    let keep: HashSet<&str> = keep
        .split_whitespace()
        .filter_map(|t| t.strip_prefix('+'))
        .collect();
    Some(
        current
            .split_whitespace()
            .filter_map(|t| t.strip_prefix('+'))
            .filter(|c| !keep.contains(c))
            .map(|c| format!("-{c}"))
            .collect(),
    )
}

impl Default for CgroupV2Controller {
    fn default() -> Self {
        Self::new()
    }
}

impl ResourceController for CgroupV2Controller {
    fn available(&self) -> bool {
        true
    }

    fn ensure(&self, path: &str, cfg: &ResourceConfig) -> Result<(), ResourceError> {
        let rel = self.rel(path).ok_or_else(|| ResourceError::Invalid {
            path: path.to_string(),
            message: sysa::l10n::t_("path is outside the cgroup v2 hierarchy").to_string(),
        })?;

        let mut cur = self.root.clone();
        for comp in rel.components() {
            // Enable controllers on the current directory so the next level
            // down may use them.
            self.enable_controllers(&cur);
            let next = cur.join(comp);
            if !next.exists() {
                fs::create_dir(&next).map_err(|source| ResourceError::Io {
                    path: next.display().to_string(),
                    source,
                })?;
            }
            cur = next;
        }
        if rel.as_os_str().is_empty() {
            // Target is the root cgroup itself (the `-.slice`): enable
            // controllers so descendants can use them.
            self.enable_controllers(&cur);
        }

        self.apply_limits(&cur, cfg);
        Ok(())
    }

    fn attach(&self, path: &str, pid: u32) -> Result<(), ResourceError> {
        let rel = self.rel(path).ok_or_else(|| ResourceError::Invalid {
            path: path.to_string(),
            message: sysa::l10n::t_("path is outside the cgroup v2 hierarchy").to_string(),
        })?;
        if rel.as_os_str().is_empty() {
            // The root cgroup already contains every process; nothing to do.
            return Ok(());
        }
        let full = self.full(&rel);
        write_file(&full.join("cgroup.procs"), &pid.to_string()).map_err(|source| {
            ResourceError::Io {
                path: full.join("cgroup.procs").display().to_string(),
                source,
            }
        })?;
        Ok(())
    }

    fn delegate(
        &self,
        path: &str,
        owner_user: &str,
        owner_group: &str,
        cfg: &ResourceConfig,
    ) -> Result<(), ResourceError> {
        let rel = self.rel(path).ok_or_else(|| ResourceError::Invalid {
            path: path.to_string(),
            message: sysa::l10n::t_("path is outside the cgroup v2 hierarchy").to_string(),
        })?;
        if rel.as_os_str().is_empty() {
            // The hierarchy root must never change hands.
            return Err(ResourceError::Invalid {
                path: path.to_string(),
                message: sysa::l10n::t_("cannot delegate the cgroup root").to_string(),
            });
        }
        let full = self.full(&rel);

        // 1. Resolve the owner.  The User=/Group= strings arrive verbatim
        //    from System A; nss runs here, at the point of use.
        let (uid, gid) = resolve_owner(owner_user, owner_group).map_err(|message| {
            ResourceError::Invalid {
                path: path.to_string(),
                message,
            }
        })?;

        // 2. Chown the whole subtree, including the cgroup.* control files
        //    that already exist: chowning the directory alone would leave
        //    them root-owned and the delegate could not read them.
        chown_recursive(&full, uid, gid)?;

        // 3. DelegateSubgroup=: a single child directory created and owned
        //    like the delegated root (user@.service: init.scope).  With a
        //    subgroup the main process runs in the child, so the unit root
        //    only distributes resources.
        let subgroup = cfg.delegate_subgroup.trim();
        let mut subgroup_created = false;
        if !subgroup.is_empty() {
            if subgroup == "." || subgroup == ".." || subgroup.contains('/') {
                return Err(ResourceError::Invalid {
                    path: path.to_string(),
                    message: sysa::l10n::fmt(
                        sysa::l10n::t_(
                            "DelegateSubgroup '{subgroup}' must be a single directory name"
                        ),
                        &[("subgroup", &subgroup.to_string())],
                    ),
                });
            }
            let child = full.join(subgroup);
            if !child.exists() {
                fs::create_dir(&child).map_err(|source| ResourceError::Io {
                    path: child.display().to_string(),
                    source,
                })?;
            }
            chown_recursive(&child, uid, gid)?;
            subgroup_created = true;
        }

        // 4. Enable the delegated controllers in the unit root's own
        //    subtree_control so the delegate can create child cgroups that
        //    use them — but only when a subgroup took the main process.
        //    The kernel's no-internal-process rule rejects enabling domain
        //    controllers on a cgroup that still hosts processes (EBUSY);
        //    without a subgroup the service starts in the unit root, and
        //    the delegate enables controllers itself once it moves its own
        //    processes into children.
        if subgroup_created {
            if cfg.delegate_controllers.is_empty() {
                self.enable_controllers(&full);
            } else {
                let names: Vec<&str> = cfg
                    .delegate_controllers
                    .iter()
                    .map(|s| s.as_str())
                    .collect();
                self.enable_controllers_list(&full, &names);
            }
        }

        // 5. Mark the directory as delegated (best-effort: the xattr is a
        //    marker for tooling; the ownership above is what enforces
        //    delegation).
        set_delegate_xattrs(&full);

        debug!("Delegated {path} to uid {uid} gid {gid}");
        Ok(())
    }

    fn processes(&self, path: &str) -> Vec<u32> {
        let Some(rel) = self.rel(path) else {
            return Vec::new();
        };
        let full = self.full(&rel);
        match read_file(&full.join("cgroup.procs")) {
            Ok(s) => s
                .lines()
                .filter_map(|l| l.trim().parse().ok())
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    fn has_child_cgroups(&self, path: &str) -> bool {
        let Some(rel) = self.rel(path) else {
            return false;
        };
        let full = self.full(&rel);
        let entries = match fs::read_dir(&full) {
            Ok(e) => e,
            Err(_) => return false,
        };
        entries
            .filter_map(|e| e.ok())
            .any(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
    }

    fn remove(&self, path: &str) -> Result<(), ResourceError> {
        let rel = self.rel(path).ok_or_else(|| ResourceError::Invalid {
            path: path.to_string(),
            message: sysa::l10n::t_("path is outside the cgroup v2 hierarchy").to_string(),
        })?;
        if rel.as_os_str().is_empty() {
            // Never remove the root cgroup.
            return Ok(());
        }
        if !self.processes(path).is_empty() || self.has_child_cgroups(path) {
            return Err(ResourceError::NotEmpty(path.to_string()));
        }
        let full = self.full(&rel);
        fs::remove_dir(&full).map_err(|source| ResourceError::Io {
            path: full.display().to_string(),
            source,
        })?;
        Ok(())
    }

    fn metrics(&self, path: &str) -> CgroupMetrics {
        let Some(rel) = self.rel(path) else {
            return CgroupMetrics::default();
        };
        let full = self.full(&rel);
        if !full.is_dir() {
            return CgroupMetrics::default();
        }

        // Processes from the whole subtree (direct + every descendant),
        // each tagged with its subpath relative to this cgroup.
        let processes = read_processes(&full);

        let mut metrics = HashMap::new();
        if let Some(v) = read_number(&full.join("memory.current")) {
            metrics.insert("MemoryCurrent".to_string(), v);
        }
        if let Some(v) = read_number(&full.join("memory.peak")) {
            metrics.insert("MemoryPeak".to_string(), v);
        }
        if let Some(v) = read_number(&full.join("memory.swap.current")) {
            metrics.insert("MemorySwapCurrent".to_string(), v);
        }
        if let Some(usage_usec) = read_key_value(&full.join("cpu.stat"), "usage_usec") {
            metrics.insert("CPUUsageNSec".to_string(), usage_usec * 1000);
        }
        if let Some(v) = read_number(&full.join("pids.current")) {
            metrics.insert("TasksCurrent".to_string(), v);
        }
        if let Some(v) = read_key_value(&full.join("memory.events"), "oom_kill") {
            metrics.insert("OOMKills".to_string(), v);
        }
        collect_io_stat(&full, &mut metrics);

        if let Some(limit) = self.effective_limit(&full, "pids.max") {
            metrics.insert("EffectiveTasksMax".to_string(), limit);
        } else {
            metrics.insert(
                "EffectiveTasksMax".to_string(),
                u64::from(DEFAULT_TASKS_MAX),
            );
        }
        if let Some(limit) = self.effective_limit(&full, "memory.max") {
            metrics.insert("EffectiveMemoryMax".to_string(), limit);
        }

        let control_group = {
            let rel_str = rel.to_string_lossy();
            if rel_str.is_empty() {
                "/".to_string()
            } else {
                format!("/{rel_str}")
            }
        };
        let control_group_id = fs::metadata(&full).map(|m| m.ino()).unwrap_or(0);

        CgroupMetrics {
            control_group,
            control_group_id,
            metrics,
            processes,
        }
    }

    fn snapshot(&self) -> Option<CgroupBaseline> {
        if !self.root.join("cgroup.controllers").exists() {
            return None;
        }
        let mut baseline = CgroupBaseline::default();
        for rel in walk_cgroups(&self.root) {
            let dir = self.full(Path::new(&rel));
            if let Ok(s) = read_file(&dir.join("cgroup.subtree_control")) {
                baseline.subtree_control.insert(rel.clone(), s);
            }
            if !rel.is_empty() {
                baseline.dirs.insert(rel);
            }
        }
        debug!(
            "Snapshot of the cgroup hierarchy: {} director(y/ies) present",
            baseline.dirs.len()
        );
        Some(baseline)
    }

    fn restore(&self, baseline: &CgroupBaseline) -> Result<(), ResourceError> {
        // No cgroup v2 hierarchy (never mounted here, or gone again): we
        // cannot have created anything in it.
        if !self.root.join("cgroup.controllers").exists() {
            debug!("No cgroup hierarchy to restore");
            return Ok(());
        }

        // Directories that were not there when the baseline was taken are
        // ours — no directory name is ever matched.  Deepest first, so a
        // directory never outlives its children.
        let mut created: Vec<String> = walk_cgroups(&self.root)
            .into_iter()
            .filter(|rel| !rel.is_empty() && !baseline.dirs.contains(rel))
            .collect();
        created.sort_by(|a, b| {
            b.matches('/')
                .count()
                .cmp(&a.matches('/').count())
                .then_with(|| b.cmp(a))
        });

        // Logged individually so every problem shows up in the log, but only
        // the first one is reported: a restore never blocks the exit.
        let mut outcome = Ok(());

        // Processes go back to the root cgroup first — that is where the
        // baseline found them, and it is also what empties each directory
        // for the removal below.
        for rel in &created {
            let dir = self.full(Path::new(rel));
            for pid in read_pids(&dir) {
                let root_procs = self.root.join("cgroup.procs");
                if let Err(e) = write_file(&root_procs, &pid.to_string()) {
                    warn!("Cannot move pid {pid} back to the root cgroup: {e}");
                    if outcome.is_ok() {
                        outcome = Err(ResourceError::Io {
                            path: root_procs.display().to_string(),
                            source: e,
                        });
                    }
                }
            }
        }

        for rel in &created {
            let dir = self.full(Path::new(rel));
            match fs::remove_dir(&dir) {
                Ok(()) => debug!("Removed cgroup {rel}"),
                // Already gone: nothing to undo.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    warn!("Cannot remove cgroup {rel}: {e}");
                    if outcome.is_ok() {
                        outcome = Err(ResourceError::Io {
                            path: dir.display().to_string(),
                            source: e,
                        });
                    }
                }
            }
        }

        // Controllers enabled on a directory that stays have to go again.
        // `ensure` only ever adds controllers, so disabling the surplus
        // restores exactly the recorded value.
        for (rel, keep) in &baseline.subtree_control {
            let dir = self.full(Path::new(rel));
            let Some(extra) = extra_controllers(&dir, keep) else {
                continue;
            };
            if extra.is_empty() {
                continue;
            }
            let file = dir.join("cgroup.subtree_control");
            if let Err(e) = write_file(&file, &extra.join(" ")) {
                warn!(
                    "Cannot restore cgroup.subtree_control on {}: {e}",
                    dir.display()
                );
                if outcome.is_ok() {
                    outcome = Err(ResourceError::Invalid {
                        path: file.display().to_string(),
                        message: e.to_string(),
                    });
                }
            }
        }

        match &outcome {
            Ok(()) => debug!("Cgroup hierarchy restored to its baseline"),
            Err(e) => warn!("Cgroup hierarchy restored imperfectly: {e}"),
        }
        outcome
    }
}

impl CgroupV2Controller {
    /// The first finite limit for `file` walking from the unit's cgroup
    /// `dir` up to the hierarchy root.  A value of `"max"` (unlimited) at a
    /// level means "inherit", so the walk continues upward; when no level
    /// sets a finite limit, `None` is returned.
    fn effective_limit(&self, dir: &Path, file: &str) -> Option<u64> {
        let mut cur = dir.to_path_buf();
        loop {
            if let Some(v) = read_number(&cur.join(file)) {
                return Some(v);
            }
            if cur == self.root {
                return None;
            }
            cur = cur.parent()?.to_path_buf();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use systema_sysr_common::{NoopController, ResourceController};

    #[test]
    fn path_rel_math() {
        let c = CgroupV2Controller::new();
        assert!(c.rel("/sys/fs/cgroup/system.slice").is_some());
        assert_eq!(
            c.rel("/sys/fs/cgroup/system.slice/foo.slice").unwrap(),
            PathBuf::from("system.slice/foo.slice")
        );
        assert!(c.rel("/tmp/nope").is_none());
        assert!(c.rel("/sys/fs/cgroup").unwrap().as_os_str().is_empty());
    }

    #[test]
    fn root_never_removed() {
        let c = CgroupV2Controller::new();
        // Relative to a synthetic root, "/sys/fs/cgroup" is the root and
        // must be a no-op for remove().
        assert!(c.remove("/sys/fs/cgroup").is_ok());
    }

    #[test]
    fn detect_is_false_without_marker() {
        // The real detection path is filesystem dependent; this exercises the
        // new() constructor only.
        let _ = CgroupV2Controller::new();
    }

    #[test]
    fn read_processes_walks_subtree() {
        let root = std::env::temp_dir().join(format!("sysr-procs-test-{}", std::process::id()));
        std::fs::create_dir_all(root.join("sub.service/deep.scope")).unwrap();
        let me = std::process::id();
        std::fs::write(root.join("cgroup.procs"), format!("{me}\n")).unwrap();
        std::fs::write(root.join("sub.service/cgroup.procs"), format!("{me}\n")).unwrap();
        std::fs::write(root.join("sub.service/deep.scope/cgroup.procs"), format!("{me}\n"))
            .unwrap();
        // A pseudo-file sibling must not be mistaken for a child cgroup.
        std::fs::write(root.join("memory.current"), "1\n").unwrap();

        let procs = read_processes(&root);
        let mut subs: Vec<&str> = procs.iter().map(|p| p.subpath.as_str()).collect();
        subs.sort();
        assert_eq!(subs, vec!["", "sub.service", "sub.service/deep.scope"]);
        assert!(procs.iter().all(|p| p.pid == me));

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn metrics_parse_number_and_key_value() {
        let dir = std::env::temp_dir().join(format!("sysr-metrics-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("memory.current"), "1048576\n").unwrap();
        std::fs::write(dir.join("pids.max"), "max\n").unwrap();
        std::fs::write(
            dir.join("cpu.stat"),
            "usage_usec 12345\nuser_usec 100\nsystem_usec 2345\n",
        )
        .unwrap();

        assert_eq!(read_number(&dir.join("memory.current")), Some(1_048_576));
        assert_eq!(read_number(&dir.join("pids.max")), None);
        assert_eq!(read_key_value(&dir.join("cpu.stat"), "usage_usec"), Some(12345));
        assert_eq!(read_key_value(&dir.join("cpu.stat"), "user_usec"), Some(100));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn metrics_aggregate_io_stat() {
        let dir = std::env::temp_dir().join(format!("sysr-io-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("io.stat"),
            "8:0 rbytes=1000 wbytes=500 rios=10 wios=5\n8:1 rbytes=2000 wbytes=500 rios=4 wios=3\n",
        )
        .unwrap();

        let mut m = HashMap::new();
        collect_io_stat(&dir, &mut m);
        assert_eq!(m.get("IOReadBytes"), Some(&3000));
        assert_eq!(m.get("IOReadOperations"), Some(&14));
        assert_eq!(m.get("IOWriteBytes"), Some(&1000));
        assert_eq!(m.get("IOWriteOperations"), Some(&8));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A throw-away stand-in for a freshly mounted cgroup2: the marker file
    /// that makes `snapshot()` recognise it, and an empty root
    /// `cgroup.subtree_control` for it to record.
    fn fake_hierarchy(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("sysr-restore-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("cgroup.controllers"),
            "cpuset cpu memory pids io\n",
        )
        .unwrap();
        std::fs::write(root.join("cgroup.subtree_control"), "").unwrap();
        root
    }

    /// Absolute path of `rel` under `root`, the form `ensure()` takes.
    fn abs(root: &Path, rel: &str) -> String {
        root.join(rel).to_string_lossy().into_owned()
    }

    /// Drop a throw-away hierarchy again.
    fn discard(root: &Path) {
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn snapshot_records_the_hierarchy_it_found() {
        let root = fake_hierarchy("snapshot");
        std::fs::create_dir_all(root.join("pre.slice")).unwrap();
        std::fs::write(root.join("cgroup.subtree_control"), "+memory\n").unwrap();
        std::fs::write(root.join("pre.slice/cgroup.subtree_control"), "+cpu\n").unwrap();

        let c = CgroupV2Controller { root: root.clone() };
        let baseline = c.snapshot().unwrap();

        let dirs: Vec<&str> = baseline.dirs.iter().map(String::as_str).collect();
        assert_eq!(dirs, vec!["pre.slice"]);
        assert_eq!(
            baseline.subtree_control.get("").map(String::as_str),
            Some("+memory\n")
        );
        assert_eq!(
            baseline
                .subtree_control
                .get("pre.slice")
                .map(String::as_str),
            Some("+cpu\n")
        );
        discard(&root);
    }

    #[test]
    fn restore_removes_everything_created_since_the_snapshot() {
        let root = fake_hierarchy("remove");
        let c = CgroupV2Controller { root: root.clone() };
        let baseline = c.snapshot().unwrap();

        c.ensure(
            &abs(&root, "work.slice/plain.service"),
            &ResourceConfig::default(),
        )
        .unwrap();
        assert!(root.join("work.slice/plain.service").is_dir());

        c.restore(&baseline).unwrap();

        // Deepest first, so the parent never outlives its child.
        assert!(!root.join("work.slice").exists());
        assert_eq!(walk_cgroups(&root), vec![String::new()]);
        discard(&root);
    }

    #[test]
    fn restore_puts_processes_back_in_the_root() {
        let root = fake_hierarchy("procs");
        let c = CgroupV2Controller { root: root.clone() };
        let baseline = c.snapshot().unwrap();

        c.ensure(&abs(&root, "plain.service"), &ResourceConfig::default())
            .unwrap();
        let pid = std::process::id();
        std::fs::write(root.join("plain.service/cgroup.procs"), format!("{pid}\n")).unwrap();

        let outcome = c.restore(&baseline);

        // Back where the baseline found them: the root cgroup.
        assert_eq!(read_pids(&root), vec![pid]);
        // Here `cgroup.procs` is a plain file where a real cgroup2 has a
        // pseudo-file that `rmdir` ignores, so the directory is left behind
        // — which is exactly the imperfect-restore path `restore` reports.
        assert!(root.join("plain.service").is_dir());
        assert!(outcome.is_err());
        discard(&root);
    }

    #[test]
    fn restore_leaves_the_recorded_hierarchy_alone() {
        let root = fake_hierarchy("keep");
        std::fs::create_dir_all(root.join("pre.slice")).unwrap();
        std::fs::write(root.join("pre.slice/cgroup.subtree_control"), "+cpu\n").unwrap();

        let c = CgroupV2Controller { root: root.clone() };
        let baseline = c.snapshot().unwrap();

        c.ensure(&abs(&root, "new.service"), &ResourceConfig::default())
            .unwrap();
        c.restore(&baseline).unwrap();

        assert!(root.join("pre.slice").is_dir());
        assert_eq!(
            std::fs::read_to_string(root.join("pre.slice/cgroup.subtree_control")).unwrap(),
            "+cpu\n"
        );
        assert!(!root.join("new.service").exists());
        discard(&root);
    }

    #[test]
    fn extra_controllers_lists_only_the_surplus() {
        let root = fake_hierarchy("extra");
        std::fs::write(root.join("cgroup.subtree_control"), "+cpu +memory\n").unwrap();

        assert_eq!(
            extra_controllers(&root, "+cpu\n"),
            Some(vec!["-memory".to_string()])
        );
        assert_eq!(extra_controllers(&root, "+cpu +memory\n"), Some(Vec::new()));
        assert_eq!(extra_controllers(&root.join("gone"), "+cpu\n"), None);
        discard(&root);
    }

    #[test]
    fn restore_disables_the_controllers_it_enabled() {
        let root = fake_hierarchy("subtree");
        std::fs::write(root.join("cgroup.subtree_control"), "+cpu\n").unwrap();
        let c = CgroupV2Controller { root: root.clone() };
        let baseline = c.snapshot().unwrap();

        // What the backend leaves behind: the recorded value plus whatever
        // it enabled on the way in.
        std::fs::write(root.join("cgroup.subtree_control"), "+cpu +memory +pids\n").unwrap();

        c.restore(&baseline).unwrap();

        // The write carries only the surplus.  On a real cgroup2 that leaves
        // the root at `+cpu`, its recorded value; this fake file simply ends
        // up holding what was written.
        assert_eq!(
            std::fs::read_to_string(root.join("cgroup.subtree_control")).unwrap(),
            "-memory -pids"
        );
        discard(&root);
    }

    #[test]
    fn restoring_twice_changes_nothing_the_second_time() {
        let root = fake_hierarchy("idempotent");
        let c = CgroupV2Controller { root: root.clone() };
        let baseline = c.snapshot().unwrap();

        c.ensure(&abs(&root, "a/b.service"), &ResourceConfig::default())
            .unwrap();
        c.restore(&baseline).unwrap();
        let after_first = walk_cgroups(&root);

        c.restore(&baseline).unwrap();
        assert_eq!(walk_cgroups(&root), after_first);
        assert_eq!(after_first, vec![String::new()]);
        discard(&root);
    }

    #[test]
    fn snapshot_without_a_cgroup_filesystem_is_none() {
        let root = std::env::temp_dir().join(format!("sysr-nomarker-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let c = CgroupV2Controller { root: root.clone() };
        assert!(c.snapshot().is_none());
        assert!(c.restore(&CgroupBaseline::default()).is_ok());
        discard(&root);
    }

    #[test]
    fn noop_controller_has_nothing_to_restore() {
        assert!(NoopController.snapshot().is_none());
        assert!(NoopController.restore(&CgroupBaseline::default()).is_ok());
    }

    #[test]
    fn resolve_owner_defaults_names_and_numeric() {
        // Neither User= nor Group=: the cgroup keeps its root owner.
        assert_eq!(resolve_owner("", "").unwrap(), (0, 0));
        // Names resolve through nss; a user without Group= takes its
        // primary group.
        assert_eq!(resolve_owner("root", "").unwrap(), (0, 0));
        assert_eq!(resolve_owner("", "root").unwrap(), (0, 0));
        // Numeric values resolve like systemd's get_user_creds fallback.
        assert_eq!(resolve_owner("0", "").unwrap(), (0, 0));
        // Nothing matches: an error, so the start job fails visibly instead
        // of spawning the unit into a cgroup with an unresolvable owner.
        assert!(resolve_owner("no-such-user-systema-test", "").is_err());
        assert!(resolve_owner("", "no-such-group-systema-test").is_err());
    }
}
