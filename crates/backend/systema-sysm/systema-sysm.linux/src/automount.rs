use std::ffi::CString;
use std::os::unix::io::RawFd;
use std::time::Duration;

use anyhow::Result;
use sysa::proto::{AutomountConfig, MountConfig};
use tokio::io::unix::AsyncFd;
use tracing::{debug, info, warn};

use crate::linux::state::{AutomountInstance, AutomountRegistry, AutomountState};

// ---------------------------------------------------------------------------
// Linux ioctl / autofs constants
// ---------------------------------------------------------------------------

// Values from <asm-generic/ioctl.h> and <linux/auto_dev-ioctl.h>
const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;
const IOC_DIR_SHIFT: u32 = 30;
const IOC_TYPE_SHIFT: u32 = 8;
const IOC_NR_SHIFT: u32 = 0;
const IOC_SIZE_SHIFT: u32 = 16;

// Spelled `libc::Ioctl` rather than a concrete width: that alias is
// `c_ulong` on linux-gnu but `c_int` on android (and on musl), and it is
// what `libc::ioctl`'s second parameter is declared with — so the request
// number has to agree with it, not with a fixed 64-bit type.  The `_IOC`
// layout itself is the same on both, so only the spelling changes.
const fn ioc(dir: u32, ty: u8, nr: u8, size: usize) -> libc::Ioctl {
    ((dir << IOC_DIR_SHIFT)
        | ((ty as u32) << IOC_TYPE_SHIFT)
        | ((nr as u32) << IOC_NR_SHIFT)
        | ((size as u32) << IOC_SIZE_SHIFT)) as libc::Ioctl
}

const fn iowr(ty: u8, nr: u8, size: usize) -> libc::Ioctl {
    ioc(IOC_READ | IOC_WRITE, ty, nr, size)
}

/// `struct autofs_dev_ioctl` (`<linux/auto_dev-ioctl.h>`): a 16-byte header
/// followed by an 8-byte command parameter union, 24 bytes in total.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct AutofsDevIoctl {
    ver_major: u32,
    ver_minor: u32,
    /// Total data size: this struct plus an optional trailing path.
    size: u32,
    /// Autofs mount fd the command applies to (`-1` where not applicable).
    ioctlfd: i32,
    /// Parameter union, split into native-endian words.  Every command
    /// except `TIMEOUT` reads a `u32` from the low word; `TIMEOUT` reads
    /// the whole `u64`.
    arg_lo: u32,
    arg_hi: u32,
}

impl AutofsDevIoctl {
    /// Kernel `init_autofs_dev_ioctl()`: the interface version the kernel
    /// validates, the size of the struct itself, and no command fd.
    fn new() -> Self {
        Self {
            ver_major: AUTOFS_DEV_IOCTL_VERSION_MAJOR,
            ver_minor: AUTOFS_DEV_IOCTL_VERSION_MINOR,
            size: AUTOFS_DEV_IOCTL_SIZEOF as u32,
            ioctlfd: -1,
            arg_lo: 0,
            arg_hi: 0,
        }
    }

    fn with_ioctl_fd(ioctl_fd: i32) -> Self {
        Self {
            ioctlfd: ioctl_fd,
            ..Self::new()
        }
    }

    fn arg_u32(&self) -> u32 {
        self.arg_lo
    }

    fn set_arg_u32(&mut self, value: u32) {
        self.arg_lo = value;
        self.arg_hi = 0;
    }

    /// Store a `u64` union member (`args_timeout.timeout`) in native byte
    /// order so the kernel reads it correctly on either endianness.
    fn set_arg_u64(&mut self, value: u64) {
        let bytes = value.to_ne_bytes();
        self.arg_lo = u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        self.arg_hi = u32::from_ne_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    }
}

/// Type byte shared by the autofs misc-device commands (`AUTOFS_IOCTL`).
const AUTOFS_IOCTL: u8 = 0x93;

const AUTOFS_DEV_IOCTL_SIZEOF: usize = std::mem::size_of::<AutofsDevIoctl>();
/// The kernel validates the command's encoded size against its own struct;
/// pin the ABI so a layout drift cannot silently turn every ioctl into
/// `-ENOTTY`/`-EINVAL` again.
const _: () = assert!(AUTOFS_DEV_IOCTL_SIZEOF == 24);

/// `AUTOFS_DEV_IOCTL_VERSION_MAJOR`/`_MINOR`: the kernel rejects any
/// request whose major differs and any newer minor.
const AUTOFS_DEV_IOCTL_VERSION_MAJOR: u32 = 1;
const AUTOFS_DEV_IOCTL_VERSION_MINOR: u32 = 1;

// Command numbers from `<linux/auto_dev-ioctl.h>` (`*_CMD` enum).
const AUTOFS_DEV_IOCTL_VERSION: libc::Ioctl = iowr(AUTOFS_IOCTL, 0x71, AUTOFS_DEV_IOCTL_SIZEOF);
const AUTOFS_DEV_IOCTL_PROTOVER: libc::Ioctl = iowr(AUTOFS_IOCTL, 0x72, AUTOFS_DEV_IOCTL_SIZEOF);
const AUTOFS_DEV_IOCTL_PROTOSUBVER: libc::Ioctl = iowr(AUTOFS_IOCTL, 0x73, AUTOFS_DEV_IOCTL_SIZEOF);
const AUTOFS_DEV_IOCTL_OPENMOUNT: libc::Ioctl = iowr(AUTOFS_IOCTL, 0x74, AUTOFS_DEV_IOCTL_SIZEOF);
const AUTOFS_DEV_IOCTL_READY: libc::Ioctl = iowr(AUTOFS_IOCTL, 0x76, AUTOFS_DEV_IOCTL_SIZEOF);
const AUTOFS_DEV_IOCTL_FAIL: libc::Ioctl = iowr(AUTOFS_IOCTL, 0x77, AUTOFS_DEV_IOCTL_SIZEOF);
const AUTOFS_DEV_IOCTL_TIMEOUT: libc::Ioctl = iowr(AUTOFS_IOCTL, 0x7a, AUTOFS_DEV_IOCTL_SIZEOF);
const AUTOFS_DEV_IOCTL_EXPIRE: libc::Ioctl = iowr(AUTOFS_IOCTL, 0x7c, AUTOFS_DEV_IOCTL_SIZEOF);

// ---------------------------------------------------------------------------
// Autofs v5 packet
// ---------------------------------------------------------------------------

/// `struct autofs_v5_packet` (`<linux/auto_fs.h>`): packet header, request
/// token and the identity of the requesting process.
#[repr(C)]
#[derive(Debug, Clone)]
struct AutofsPacket {
    /// `struct autofs_packet_hdr`.
    proto_version: u32,
    type_: u32,
    wait_queue_token: u32,
    dev: u32,
    ino: u64,
    uid: u32,
    gid: u32,
    pid: u32,
    tgid: u32,
    len: u32,
    /// `NAME_MAX + 1`.
    name: [u8; 256],
}

// Pin the field offsets the kernel writes; the struct's tail padding is
// ABI-dependent (304 bytes on LP64, 300 on ILP32), the offsets are not.
const _: () = assert!(std::mem::offset_of!(AutofsPacket, wait_queue_token) == 8);
const _: () = assert!(std::mem::offset_of!(AutofsPacket, len) == 40);
const _: () = assert!(std::mem::offset_of!(AutofsPacket, name) == 44);

/// Direct-mount request and expire packet types
/// (`autofs_ptype_missing_direct` / `autofs_ptype_expire_direct`).
const AUTOFS_PTYPE_MOUNT_DIRECT: u32 = 5;
const AUTOFS_PTYPE_EXPIRE_DIRECT: u32 = 6;

// ---------------------------------------------------------------------------
// Dev autofs fd (global, shared across all automount instances)
// ---------------------------------------------------------------------------

static mut DEV_AUTOFS_FD: i32 = -1;

/// Open `/dev/autofs` once and cache the fd.
///
/// Must run only after the kernel has an autofs user: a modular autofs
/// registers the misc device when `mount(2)` asks for the filesystem, and
/// `open(2)` never triggers module autoloading on its own.
fn ensure_dev_autofs() -> Result<i32> {
    unsafe {
        if DEV_AUTOFS_FD >= 0 {
            return Ok(DEV_AUTOFS_FD);
        }

        let fd = libc::open(
            CString::new("/dev/autofs").unwrap().as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC,
        );
        if fd < 0 {
            let err = std::io::Error::last_os_error();
            anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_(
                    "Failed to open /dev/autofs (is the autofs kernel module loaded?): {e}"
                ),
                &[("e", &err.to_string())]
            ));
        }

        // Verify version.
        let mut params = AutofsDevIoctl::new();
        let rc = libc::ioctl(
            fd,
            AUTOFS_DEV_IOCTL_VERSION,
            &mut params as *mut _ as *mut libc::c_void,
        );
        if rc < 0 {
            libc::close(fd);
            anyhow::bail!(sysa::l10n::t_("AUTOFS_DEV_IOCTL_VERSION failed"));
        }

        info!(
            "Autofs kernel version {}.{}",
            params.ver_major, params.ver_minor
        );

        DEV_AUTOFS_FD = fd;
        Ok(fd)
    }
}

// ---------------------------------------------------------------------------
// Core automount operations
// ---------------------------------------------------------------------------

fn open_ioctl_fd(dev_autofs_fd: i32, where_: &str, dev_id: u64) -> Result<i32> {
    let path_bytes = where_.as_bytes();
    // `param.size` covers the struct plus the NUL-terminated path; the
    // kernel copies exactly that many bytes from user space.
    let buf_size = AUTOFS_DEV_IOCTL_SIZEOF + path_bytes.len() + 1;
    let mut buf = vec![0u8; buf_size];

    unsafe {
        let params = &mut *(buf.as_mut_ptr() as *mut AutofsDevIoctl);
        *params = AutofsDevIoctl::new();
        params.size = buf_size as u32;
        // `args_openmount.devid`: the encoded device number of the mount.
        params.set_arg_u32(dev_id as u32);

        // Copy path after the struct.
        let path_ptr = buf.as_mut_ptr().add(AUTOFS_DEV_IOCTL_SIZEOF);
        std::ptr::copy_nonoverlapping(path_bytes.as_ptr(), path_ptr, path_bytes.len());
        *path_ptr.add(path_bytes.len()) = 0;

        let rc = libc::ioctl(
            dev_autofs_fd,
            AUTOFS_DEV_IOCTL_OPENMOUNT,
            buf.as_ptr() as *const libc::c_void,
        );
        if rc < 0 {
            anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_("AUTOFS_DEV_IOCTL_OPENMOUNT failed for {path}"),
                &[("path", &where_.to_string())]
            ));
        }

        let ioctl_fd = params.ioctlfd;
        if ioctl_fd < 0 {
            anyhow::bail!(sysa::l10n::t_(
                "AUTOFS_DEV_IOCTL_OPENMOUNT returned invalid fd"
            ));
        }

        Ok(ioctl_fd)
    }
}

fn check_autofs_protocol(dev_autofs_fd: i32, ioctl_fd: i32) -> Result<()> {
    unsafe {
        let mut params = AutofsDevIoctl::with_ioctl_fd(ioctl_fd);

        let rc = libc::ioctl(
            dev_autofs_fd,
            AUTOFS_DEV_IOCTL_PROTOVER,
            &mut params as *mut _ as *mut libc::c_void,
        );
        if rc < 0 {
            anyhow::bail!(sysa::l10n::t_("AUTOFS_DEV_IOCTL_PROTOVER failed"));
        }
        let major = params.arg_u32();

        let mut params2 = AutofsDevIoctl::with_ioctl_fd(ioctl_fd);
        let rc = libc::ioctl(
            dev_autofs_fd,
            AUTOFS_DEV_IOCTL_PROTOSUBVER,
            &mut params2 as *mut _ as *mut libc::c_void,
        );
        if rc < 0 {
            anyhow::bail!(sysa::l10n::t_("AUTOFS_DEV_IOCTL_PROTOSUBVER failed"));
        }
        let minor = params2.arg_u32();

        debug!("Autofs protocol version {}.{}", major, minor);
    }
    Ok(())
}

fn set_autofs_timeout(dev_autofs_fd: i32, ioctl_fd: i32, timeout_sec: u32) -> Result<()> {
    unsafe {
        let mut params = AutofsDevIoctl::with_ioctl_fd(ioctl_fd);
        params.set_arg_u64(timeout_sec as u64);

        let rc = libc::ioctl(
            dev_autofs_fd,
            AUTOFS_DEV_IOCTL_TIMEOUT,
            &mut params as *mut _ as *mut libc::c_void,
        );
        if rc < 0 {
            anyhow::bail!(sysa::l10n::t_("AUTOFS_DEV_IOCTL_TIMEOUT failed"));
        }
    }
    Ok(())
}

fn send_ack_or_fail(ioctl_fd: i32, token: u32, success: bool) -> Result<()> {
    let dev_autofs_fd = ensure_dev_autofs()?;
    unsafe {
        let mut params = AutofsDevIoctl::with_ioctl_fd(ioctl_fd);
        params.set_arg_u32(token);
        let cmd = if success {
            AUTOFS_DEV_IOCTL_READY
        } else {
            AUTOFS_DEV_IOCTL_FAIL
        };
        let rc = libc::ioctl(
            dev_autofs_fd,
            cmd,
            &mut params as *mut _ as *mut libc::c_void,
        );
        if rc < 0 {
            anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_("autofs reply (token={token}) failed: {error}"),
                &[
                    ("token", &token.to_string()),
                    ("error", &(std::io::Error::last_os_error()).to_string())
                ]
            ));
        }
    }
    Ok(())
}

/// Acknowledge a kernel autofs request (mount or expire) as handled.
pub fn autofs_send_ready(ioctl_fd: i32, token: u32) -> Result<()> {
    send_ack_or_fail(ioctl_fd, token, true)
}

/// Tell the kernel the autofs request failed (mount not performed).
/// The blocked accessor gets ENOENT; the trap stays armed for the next access.
pub fn autofs_send_fail(ioctl_fd: i32, token: u32) -> Result<()> {
    send_ack_or_fail(ioctl_fd, token, false)
}

// ---------------------------------------------------------------------------
// Mount(2) helper for autofs
// ---------------------------------------------------------------------------

fn mount_autofs(pipe_write_fd: RawFd, where_: &str, extra_options: &str) -> Result<()> {
    let source = format!("systemd-{}", unsafe { libc::getpid() });
    let fstype = CString::new("autofs").unwrap();
    let target = CString::new(where_).unwrap();
    // NOTE: no explicit pgrp= option.  The kernel records the process group
    // of the mounting process and treats every request from that group as
    // coming from the automount daemon (see Documentation/filesystems/
    // autofs.rst, "detecting the daemon").  Passing an explicit pgrp= would
    // only be correct if this process were a process-group leader.
    let options = if extra_options.is_empty() {
        format!("fd={},minproto=5,maxproto=5,direct", pipe_write_fd)
    } else {
        format!(
            "fd={},minproto=5,maxproto=5,direct,{}",
            pipe_write_fd, extra_options
        )
    };
    let options_c = CString::new(options).unwrap();
    let source_c = CString::new(source).unwrap();

    unsafe {
        let rc = libc::mount(
            source_c.as_ptr(),
            target.as_ptr(),
            fstype.as_ptr(),
            0,
            options_c.as_ptr() as *const libc::c_void,
        );
        if rc < 0 {
            anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_("mount(2) autofs failed for {path}: {error}"),
                &[
                    ("path", &where_.to_string()),
                    ("error", &(std::io::Error::last_os_error()).to_string())
                ]
            ));
        }
    }
    Ok(())
}

fn unmount_autofs(where_: &str) -> Result<()> {
    let target = CString::new(where_).unwrap();
    unsafe {
        let rc = libc::umount2(target.as_ptr(), libc::MNT_DETACH | libc::UMOUNT_NOFOLLOW);
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            warn!("umount2(MNT_DETACH) failed for {}: {}", where_, err);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub async fn automount_enter_waiting(
    registry: AutomountRegistry,
    unit_name: &str,
    config: &AutomountConfig,
    mount_config: Option<&MountConfig>,
    mount_event_tx: tokio::sync::mpsc::UnboundedSender<AutomountTrigger>,
) -> Result<()> {
    let where_ = config.r#where.clone();

    // Create pipe for autofs communication.
    let mut pipe_fds = [-1i32; 2];
    unsafe {
        let rc = libc::pipe2(pipe_fds.as_mut_ptr(), libc::O_CLOEXEC);
        if rc < 0 {
            anyhow::bail!(sysa::l10n::t_("pipe2 for automount failed"));
        }
        // Make read side non-blocking.
        let flags = libc::fcntl(pipe_fds[0], libc::F_GETFL, 0);
        libc::fcntl(pipe_fds[0], libc::F_SETFL, flags | libc::O_NONBLOCK);
    }

    // Create the mount point directory when it does not exist yet.  An
    // existing path keeps its mode: the sentinel may live under `/proc` or
    // `/sys` (e.g. `proc-sys-fs-binfmt_misc.automount`), where `chmod` fails
    // with EPERM.  A failed attempt is not fatal — the autofs mount below
    // reports the real problem.
    if let Err(e) = crate::linux::mount::ensure_mount_point(&where_, &config.directory_mode) {
        debug!(
            "Automount mount point for {} is not usable yet: {e}",
            where_
        );
    }

    // Mount autofs.  Besides creating the sentinel this is what makes the
    // kernel issue `request_module("fs-autofs")`, loading a modular autofs
    // and thereby materialising `/dev/autofs`.
    mount_autofs(pipe_fds[1], &where_, &config.extra_options)?;

    // Close write end in parent.
    unsafe {
        libc::close(pipe_fds[1]);
    }

    // Resolve the autofs device only now: before the mount above, a modular
    // autofs has no device node yet and the open fails with ENOENT.  A
    // failure here leaves the sentinel without a daemon, so tear it down
    // before reporting.
    let dev_autofs_fd = match ensure_dev_autofs() {
        Ok(fd) => fd,
        Err(e) => {
            let _ = unmount_autofs(&where_);
            unsafe {
                libc::close(pipe_fds[0]);
            }
            return Err(e);
        }
    };

    // Stat the mount point to get dev_id.
    let dev_id = {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let path_c = CString::new(where_.as_str()).unwrap();
        unsafe {
            let rc = libc::stat(path_c.as_ptr(), &mut st);
            if rc < 0 {
                unmount_autofs(&where_)?;
                anyhow::bail!(sysa::l10n::fmt(
                    sysa::l10n::t_("stat of automount point {path} failed"),
                    &[("path", &where_.to_string())]
                ));
            }
        }
        st.st_dev
    };

    // Open ioctl fd.
    let ioctl_fd = open_ioctl_fd(dev_autofs_fd, &where_, dev_id)?;

    // Check protocol.
    check_autofs_protocol(dev_autofs_fd, ioctl_fd)?;

    // Set timeout.
    let timeout_idle_sec = config.timeout_idle_sec;
    if timeout_idle_sec > 0 {
        set_autofs_timeout(dev_autofs_fd, ioctl_fd, timeout_idle_sec)?;
    }

    // Spawn expire timer if timeout_idle is set.  The task is started before
    // the instance is registered and its handle is stored atomically with
    // the insert below, so teardown can always find and abort it (no leaked
    // timers across restart/stop).
    let expire_handle = if timeout_idle_sec > 0 {
        let reg_clone = registry.clone();
        let unit_name_clone = unit_name.to_string();
        let expire_interval = std::cmp::max(timeout_idle_sec / 3, 1);
        Some(tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(expire_interval as u64)).await;
                let should_expire = {
                    let reg = reg_clone.lock();
                    match reg.get(&unit_name_clone) {
                        // Instance gone (torn down): stop the timer.
                        None => break,
                        Some(inst) => inst.state == AutomountState::Running,
                    }
                };
                if should_expire {
                    if let Err(e) = do_expire(reg_clone.clone(), &unit_name_clone) {
                        warn!("Automount expire failed for {}: {}", unit_name_clone, e);
                    }
                }
            }
        }))
    } else {
        None
    };

    // Register the instance.
    {
        let mut reg = registry.lock();
        let mut inst = AutomountInstance::new(unit_name.to_string(), where_.clone());
        inst.state = AutomountState::Waiting;
        inst.timeout_idle_usec = (timeout_idle_sec as u64) * 1_000_000;
        inst.directory_mode = config.directory_mode.clone();
        inst.extra_options = config.extra_options.clone();
        inst.pipe_fd = Some(pipe_fds[0]);
        inst.dev_id = dev_id;
        inst.ioctl_fd = Some(ioctl_fd);
        inst.mount_config = mount_config.cloned();
        inst.expire_handle = expire_handle;
        reg.insert(unit_name.to_string(), inst);
    }

    // Spawn pipe reader.
    let read_fd = pipe_fds[0];
    let reg_clone = registry.clone();
    let unit_name_clone = unit_name.to_string();
    let trigger_tx = mount_event_tx.clone();
    tokio::spawn(async move {
        automount_pipe_reader(read_fd, reg_clone, unit_name_clone, trigger_tx).await;
    });

    info!("Automount waiting for {} on {}", unit_name, where_);
    Ok(())
}

pub async fn automount_enter_dead(registry: AutomountRegistry, unit_name: &str) -> Result<()> {
    let (where_, pipe_fd, ioctl_fd, expire_handle) = {
        let mut reg = registry.lock();
        let inst = match reg.get_mut(unit_name) {
            Some(i) => i,
            None => return Ok(()),
        };
        let w = inst.where_.clone();
        let pfd = inst.pipe_fd.take();
        let ifd = inst.ioctl_fd.take();
        let eh = inst.expire_handle.take();
        inst.state = AutomountState::Dead;
        (w, pfd, ifd, eh)
    };

    // Stop the idle-expire timer before tearing down the fds.
    if let Some(handle) = expire_handle {
        handle.abort();
    }

    // Unmount autofs.
    let _ = unmount_autofs(&where_);

    // Close fds.
    if let Some(fd) = pipe_fd {
        unsafe {
            libc::close(fd);
        }
    }
    if let Some(fd) = ioctl_fd {
        unsafe {
            libc::close(fd);
        }
    }

    info!("Automount dead for {}", unit_name);
    Ok(())
}

// ---------------------------------------------------------------------------
// Pipe reader: handles kernel autofs trigger packets
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct AutomountTrigger {
    pub unit_name: String,
    /// Companion `.mount` unit name (the real filesystem to mount/umount).
    pub mount_unit: String,
    /// Mount point path (identical for the automount and its companion).
    pub where_: String,
    /// Preloaded config for the companion mount, snapshotted at packet time.
    pub mount_config: Option<MountConfig>,
    /// Ioctl fd of the autofs instance, snapshotted at packet time.
    pub ioctl_fd: i32,
    pub event: TriggerEvent,
}

#[derive(Debug, Clone)]
pub enum TriggerEvent {
    MountRequest { token: u32 },
    ExpireRequest { token: u32 },
}

/// Derive the companion mount unit name from an automount unit name.
pub fn companion_mount_unit(unit_name: &str) -> String {
    match unit_name.strip_suffix(".automount") {
        Some(base) => format!("{}.mount", base),
        None => format!("{}.mount", unit_name),
    }
}

async fn automount_pipe_reader(
    read_fd: i32,
    registry: AutomountRegistry,
    unit_name: String,
    trigger_tx: tokio::sync::mpsc::UnboundedSender<AutomountTrigger>,
) {
    let async_fd = match AsyncFd::new(read_fd) {
        Ok(fd) => fd,
        Err(e) => {
            warn!("Failed to create AsyncFd for automount pipe: {}", e);
            return;
        }
    };

    'reader: loop {
        let mut guard = match async_fd.readable().await {
            Ok(g) => g,
            Err(_) => break 'reader,
        };

        // Drain everything the kernel queued for this readiness edge, then
        // clear the readiness flag.  Dropping the guard does NOT clear it
        // (tokio's `AsyncFdReadyGuard` has no `Drop` and `retain_ready()` is
        // a no-op), so without `clear_ready()` the loop would spin on
        // `WouldBlock` and starve the single-threaded runtime.
        'packets: loop {
            let mut packet: AutofsPacket = unsafe { std::mem::zeroed() };
            let packet_size = std::mem::size_of::<AutofsPacket>();
            let n = unsafe {
                libc::read(
                    read_fd,
                    &mut packet as *mut _ as *mut libc::c_void,
                    packet_size,
                )
            };

            if n <= 0 {
                if n < 0 {
                    let err = std::io::Error::last_os_error();
                    if err.kind() == std::io::ErrorKind::WouldBlock {
                        guard.clear_ready();
                        break 'packets;
                    }
                    warn!("Error reading automount pipe for {}: {}", unit_name, err);
                }
                // EOF or error — pipe closed
                break 'reader;
            }

            let event = match packet.type_ {
                AUTOFS_PTYPE_MOUNT_DIRECT => {
                    debug!(
                        "Automount mount request for {} (token={})",
                        unit_name, packet.wait_queue_token
                    );
                    {
                        let mut reg = registry.lock();
                        if let Some(inst) = reg.get_mut(&unit_name) {
                            inst.state = AutomountState::Running;
                        }
                    }
                    TriggerEvent::MountRequest {
                        token: packet.wait_queue_token,
                    }
                }
                AUTOFS_PTYPE_EXPIRE_DIRECT => {
                    debug!(
                        "Automount expire request for {} (token={})",
                        unit_name, packet.wait_queue_token
                    );
                    TriggerEvent::ExpireRequest {
                        token: packet.wait_queue_token,
                    }
                }
                other => {
                    warn!("Unknown automount packet type {} for {}", other, unit_name);
                    continue 'packets;
                }
            };

            // Snapshot the data the trigger handler needs.  The ioctl fd is
            // taken under the lock and may be gone if the instance is being
            // torn down concurrently — in that case drop the trigger entirely.
            let (mount_config, ioctl_fd, where_) = {
                let reg = registry.lock();
                match reg.get(&unit_name) {
                    Some(inst) => {
                        let fd = match inst.ioctl_fd {
                            Some(fd) => fd,
                            None => {
                                warn!(
                                    "Dropping {} trigger: no ioctl fd (teardown in progress?)",
                                    unit_name
                                );
                                continue 'packets;
                            }
                        };
                        (inst.mount_config.clone(), fd, inst.where_.clone())
                    }
                    None => continue 'packets,
                }
            };

            let _ = trigger_tx.send(AutomountTrigger {
                unit_name: unit_name.clone(),
                mount_unit: companion_mount_unit(&unit_name),
                where_,
                mount_config,
                ioctl_fd,
                event,
            });
        }
    }

    // Pipe closed — mark as dead, but only if this reader's fd is still the
    // instance's current pipe (a restart may have replaced it already).
    let mut reg = registry.lock();
    if let Some(inst) = reg.get_mut(&unit_name) {
        if inst.pipe_fd == Some(read_fd) {
            inst.state = AutomountState::Dead;
            inst.pipe_fd = None;
            info!("Automount pipe closed for {}", unit_name);
        }
    }
}

// ---------------------------------------------------------------------------
// Expire handling
// ---------------------------------------------------------------------------

fn do_expire(registry: AutomountRegistry, unit_name: &str) -> Result<()> {
    let ioctl_fd = {
        let reg = registry.lock();
        match reg.get(unit_name).and_then(|inst| inst.ioctl_fd) {
            Some(fd) => fd,
            None => {
                warn!("Cannot expire {}: ioctl fd gone", unit_name);
                return Ok(());
            }
        }
    };

    let dev_autofs_fd = ensure_dev_autofs()?;

    unsafe {
        let mut params = AutofsDevIoctl::with_ioctl_fd(ioctl_fd);

        // Try expire in a loop until EAGAIN.  When the kernel selects an
        // object to expire it sends an expire_direct packet on the pipe and
        // BLOCKS here until the trigger handler acknowledges it.
        loop {
            let rc = libc::ioctl(
                dev_autofs_fd,
                AUTOFS_DEV_IOCTL_EXPIRE,
                &mut params as *mut _ as *mut libc::c_void,
            );
            if rc < 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EAGAIN) {
                    break; // Nothing left to expire
                }
                warn!("AUTOFS_DEV_IOCTL_EXPIRE failed for {}: {}", unit_name, err);
                break;
            }
        }
    }

    Ok(())
}
