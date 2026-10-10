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

#[repr(C)]
#[derive(Debug, Default)]
struct AutofsDevIoctl {
    ver_major: u32,
    ver_minor: u32,
    size: u32,
    ioctlfd: i32,
    arg1: u64,
    arg2: u64,
}

const AUTOFS_TYPE: u8 = 0xf9;
const AUTOFS_DEV_IOCTL_SIZEOF: usize = std::mem::size_of::<AutofsDevIoctl>();
const AUTOFS_DEV_IOCTL_VERSION: libc::Ioctl = iowr(AUTOFS_TYPE, 0x00, AUTOFS_DEV_IOCTL_SIZEOF);
const AUTOFS_DEV_IOCTL_OPENMOUNT: libc::Ioctl = iowr(AUTOFS_TYPE, 0x04, AUTOFS_DEV_IOCTL_SIZEOF);
const AUTOFS_DEV_IOCTL_PROTOVER: libc::Ioctl = iowr(AUTOFS_TYPE, 0x01, AUTOFS_DEV_IOCTL_SIZEOF);
const AUTOFS_DEV_IOCTL_PROTOSUBVER: libc::Ioctl = iowr(AUTOFS_TYPE, 0x02, AUTOFS_DEV_IOCTL_SIZEOF);
const AUTOFS_DEV_IOCTL_TIMEOUT: libc::Ioctl = iowr(AUTOFS_TYPE, 0x0b, AUTOFS_DEV_IOCTL_SIZEOF);
const AUTOFS_DEV_IOCTL_EXPIRE: libc::Ioctl = iowr(AUTOFS_TYPE, 0x0c, AUTOFS_DEV_IOCTL_SIZEOF);
const AUTOFS_DEV_IOCTL_ACK: libc::Ioctl = iowr(AUTOFS_TYPE, 0x09, AUTOFS_DEV_IOCTL_SIZEOF);
const AUTOFS_DEV_IOCTL_FAIL: libc::Ioctl = iowr(AUTOFS_TYPE, 0x0d, AUTOFS_DEV_IOCTL_SIZEOF);

const AUTOFS_DEV_IOCTL_OPENMOUNT_SIZEOF: usize = AUTOFS_DEV_IOCTL_SIZEOF + 256; // room for path

// ---------------------------------------------------------------------------
// Autofs v5 packet
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Debug, Clone)]
struct AutofsPacket {
    // autofs_v5_packet_union header + v5_packet body
    // From linux/auto_fs.h: struct autofs_v5_packet has: wait_queue_token, len, name, dev_t, pid, uid, gid, ...
    proto: u32,
    type_: u32,
    wait_queue_token: u32,
    len: u32,
    name: [u8; 256],
    dev_t: u64,
    pid: u32,
    uid: u32,
    gid: u32,
}

const AUTOFS_PTYPE_MISSING_DIRECT: u32 = 5;
const AUTOFS_PTYPE_EXPIRE_DIRECT: u32 = 2;

// ---------------------------------------------------------------------------
// Dev autofs fd (global, shared across all automount instances)
// ---------------------------------------------------------------------------

static mut DEV_AUTOFS_FD: i32 = -1;

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
            anyhow::bail!(sysa::l10n::t_(
                "Failed to open /dev/autofs (is autofs kernel module loaded?)"
            ));
        }

        // Verify version
        let mut params: AutofsDevIoctl = Default::default();
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
    let _path_c = CString::new(where_).unwrap();
    let path_bytes = where_.as_bytes();

    // Allocate buffer: struct + path + null
    let buf_size = AUTOFS_DEV_IOCTL_OPENMOUNT_SIZEOF;
    let mut buf = vec![0u8; buf_size];

    unsafe {
        let params = &mut *(buf.as_mut_ptr() as *mut AutofsDevIoctl);
        params.size = (AUTOFS_DEV_IOCTL_SIZEOF + path_bytes.len() + 1) as u32;
        params.ioctlfd = -1;
        params.arg1 = dev_id; // openmount.devid = dev_id

        // Copy path after the struct
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
        let mut params: AutofsDevIoctl = AutofsDevIoctl {
            ioctlfd: ioctl_fd,
            ..Default::default()
        };

        let rc = libc::ioctl(
            dev_autofs_fd,
            AUTOFS_DEV_IOCTL_PROTOVER,
            &mut params as *mut _ as *mut libc::c_void,
        );
        if rc < 0 {
            anyhow::bail!(sysa::l10n::t_("AUTOFS_DEV_IOCTL_PROTOVER failed"));
        }
        let major = params.arg1 as u32;

        let mut params2: AutofsDevIoctl = AutofsDevIoctl {
            ioctlfd: ioctl_fd,
            ..Default::default()
        };
        let rc = libc::ioctl(
            dev_autofs_fd,
            AUTOFS_DEV_IOCTL_PROTOSUBVER,
            &mut params2 as *mut _ as *mut libc::c_void,
        );
        if rc < 0 {
            anyhow::bail!(sysa::l10n::t_("AUTOFS_DEV_IOCTL_PROTOSUBVER failed"));
        }
        let minor = params2.arg1 as u32;

        debug!("Autofs protocol version {}.{}", major, minor);
    }
    Ok(())
}

fn set_autofs_timeout(dev_autofs_fd: i32, ioctl_fd: i32, timeout_sec: u32) -> Result<()> {
    unsafe {
        let mut params: AutofsDevIoctl = AutofsDevIoctl {
            ioctlfd: ioctl_fd,
            arg1: timeout_sec as u64,
            ..Default::default()
        };

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
        let mut params: AutofsDevIoctl = AutofsDevIoctl {
            ioctlfd: ioctl_fd,
            arg1: token as u64,
            ..Default::default()
        };
        let cmd = if success {
            AUTOFS_DEV_IOCTL_ACK
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

    let dev_autofs_fd = ensure_dev_autofs()?;

    // Create mount point directory.
    let dir_mode = if config.directory_mode.is_empty() {
        "0755"
    } else {
        &config.directory_mode
    };
    let _ = std::process::Command::new("mkdir")
        .arg("-p")
        .arg(&where_)
        .status();
    let _ = std::process::Command::new("chmod")
        .arg(dir_mode)
        .arg(&where_)
        .status();

    // Mount autofs.
    mount_autofs(pipe_fds[1], &where_, &config.extra_options)?;

    // Close write end in parent.
    unsafe {
        libc::close(pipe_fds[1]);
    }

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

    loop {
        let mut guard = match async_fd.readable().await {
            Ok(g) => g,
            Err(_) => break,
        };

        let mut packet: AutofsPacket = unsafe { std::mem::zeroed() };
        let packet_size = std::mem::size_of::<AutofsPacket>();
        let n = unsafe {
            libc::read(
                read_fd,
                &mut packet as *mut _ as *mut libc::c_void,
                packet_size,
            )
        };

        guard.retain_ready();
        drop(guard);

        if n <= 0 {
            if n < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::WouldBlock {
                    continue;
                }
                warn!("Error reading automount pipe for {}: {}", unit_name, err);
            }
            // EOF or error — pipe closed
            break;
        }

        let event = match packet.type_ {
            AUTOFS_PTYPE_MISSING_DIRECT => {
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
                continue;
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
                            continue;
                        }
                    };
                    (inst.mount_config.clone(), fd, inst.where_.clone())
                }
                None => continue,
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
        let mut params: AutofsDevIoctl = AutofsDevIoctl {
            ioctlfd: ioctl_fd,
            ..Default::default()
        };

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
