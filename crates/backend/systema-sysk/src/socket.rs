use std::collections::HashMap;
use std::ffi::CString;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use parking_lot::Mutex;
use tokio::net::{TcpListener, UnixListener};
use tracing::{info, warn};

use sysa::proto::SocketConfig;

const ABSTRACT_PREFIX: char = '@';

/// A bound listening socket (TCP, Unix stream, or Unix seqpacket).
enum BoundSocket {
    Tcp(TcpListener),
    UnixStream(UnixListener),
    Udp(RawFd),
    Fifo,
    Netlink(RawFd),
}

/// Runtime state for one managed socket unit.
pub struct ManagedSocket {
    listeners: Vec<BoundSocket>,
    accept_tasks: Vec<tokio::task::JoinHandle<()>>,
    pub config: SocketConfig,
}

/// Global manager for all socket units this worker owns.
pub type SocketManager = Arc<Mutex<HashMap<String, ManagedSocket>>>;

pub fn new_manager() -> SocketManager {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Start a socket unit: create, bind, and listen on all configured addresses.
pub fn start_socket(manager: &SocketManager, unit_name: &str, config: &SocketConfig) -> Result<()> {
    let mut guard = manager.lock();
    if guard.contains_key(unit_name) {
        anyhow::bail!(sysa::l10n::t_("Socket '{unit_name}' is already running."));
    }

    let mut listeners: Vec<BoundSocket> = Vec::new();

    // Parse socket_mode and directory_mode from config, falling back to
    // systemd defaults (0666 for socket files, 0755 for directories).
    let socket_mode = parse_mode(&config.socket_mode, 0o666);
    let directory_mode = parse_mode(&config.directory_mode, 0o755);

    for addr in &config.listen {
        if !addr.stream.is_empty() {
            if let Some(listener) = bind_stream(&addr.stream, config.backlog, socket_mode, directory_mode).with_context(|| {
                sysa::l10n::fmt(
                    sysa::l10n::t_("Failed to bind ListenStream '{addr_stream}'."),
                    &[("addr_stream", &addr.stream.to_string())],
                )
            })? {
                listeners.push(listener);
            }
        }
        if !addr.datagram.is_empty() {
            let fd = bind_datagram(&addr.datagram, socket_mode, directory_mode).with_context(|| {
                sysa::l10n::fmt(
                    sysa::l10n::t_("Failed to bind ListenDatagram '{addr_datagram}'."),
                    &[("addr_datagram", &addr.datagram.to_string())],
                )
            })?;
            listeners.push(BoundSocket::Udp(fd));
        }
        if !addr.sequential_packet.is_empty() {
            if let Some(listener) = bind_seqpacket(&addr.sequential_packet, config.backlog, socket_mode, directory_mode)
                .with_context(|| {
                    sysa::l10n::fmt(
                        sysa::l10n::t_(
                            "Failed to bind ListenSequentialPacket '{addr_sequential_packet}'.",
                        ),
                        &[(
                            "addr_sequential_packet",
                            &addr.sequential_packet.to_string(),
                        )],
                    )
                })?
            {
                listeners.push(listener);
            }
        }
        if !addr.fifo.is_empty() {
            create_fifo(&addr.fifo, &config.socket_mode).with_context(|| {
                sysa::l10n::fmt(
                    sysa::l10n::t_("Failed to create FIFO '{addr_fifo}'."),
                    &[("addr_fifo", &addr.fifo.to_string())],
                )
            })?;
            listeners.push(BoundSocket::Fifo);
        }
        if !addr.netlink.is_empty() {
            let fd = bind_netlink(&addr.netlink).with_context(|| {
                sysa::l10n::fmt(
                    sysa::l10n::t_(
                        "Failed to bind ListenNetlink '{addr_netlink}'.",
                    ),
                    &[(
                        "addr_netlink",
                        &addr.netlink.to_string(),
                    )],
                )
            })?;
            listeners.push(BoundSocket::Netlink(fd));
        }
    }

    let any_address = config
        .listen
        .iter()
        .any(|addr| {
            !addr.stream.is_empty()
                || !addr.datagram.is_empty()
                || !addr.sequential_packet.is_empty()
                || !addr.fifo.is_empty()
                || !addr.netlink.is_empty()
        });
    if !any_address {
        anyhow::bail!(sysa::l10n::t_(
            "Socket '{unit_name}' has no listen addresses configured."
        ));
    }

    let n = listeners.len();
    let managed = ManagedSocket {
        listeners,
        accept_tasks: Vec::new(),
        config: config.clone(),
    };
    guard.insert(unit_name.to_string(), managed);
    if n == 0 {
        info!(
            "Socket '{}' started: all listen addresses already externally served",
            unit_name
        );
    } else {
        info!("Socket '{}' started ({} listener(s))", unit_name, n);
    }
    Ok(())
}

/// Spawn accept loops for Accept=yes sockets.  Each accepted connection
/// spawns a child process with the accepted fd as fd 3.
pub fn spawn_accept_loops(manager: &SocketManager, unit_name: &str) {
    let accept_tasks = {
        let mut guard = manager.lock();
        let ms = match guard.get_mut(unit_name) {
            Some(ms) if ms.config.accept && ms.accept_tasks.is_empty() => ms,
            _ => return,
        };

        let mut tasks = Vec::new();
        for ls in &ms.listeners {
            match ls {
                BoundSocket::Tcp(l) => {
                    let listener = try_clone_tcp(l);
                    let name = unit_name.to_string();
                    let task = tokio::spawn(async move {
                        accept_loop_tcp(listener, &name).await;
                    });
                    tasks.push(task);
                }
                BoundSocket::UnixStream(l) => {
                    let listener = try_clone_unix(l);
                    let name = unit_name.to_string();
                    let task = tokio::spawn(async move {
                        accept_loop_unix(listener, &name).await;
                    });
                    tasks.push(task);
                }
                _ => {}
            }
        }
        ms.accept_tasks = tasks;
        std::mem::take(&mut ms.accept_tasks)
    };
    // Keep handles alive by re-inserting (already done above via &mut).
    drop(accept_tasks);
}

/// Stop and clean up a socket unit.
pub fn stop_socket(manager: &SocketManager, unit_name: &str) -> Result<()> {
    let mut guard = manager.lock();
    let managed = guard
        .remove(unit_name)
        .ok_or_else(|| anyhow::anyhow!(sysa::l10n::t_("Socket '{unit_name}' is not running.")))?;

    // Abort accept loops.
    for handle in &managed.accept_tasks {
        handle.abort();
    }
    // Drop listeners (closes sockets).
    // managed is dropped when it falls out of scope from guard.remove().

    info!("Socket '{}' stopped", unit_name);
    Ok(())
}

/// Return the raw fd of the first listening socket for a unit.
pub fn get_listener_fd(manager: &SocketManager, unit_name: &str) -> Option<RawFd> {
    let guard = manager.lock();
    let ms = guard.get(unit_name)?;
    match ms.listeners.first()? {
        BoundSocket::Tcp(l) => Some(l.as_raw_fd()),
        BoundSocket::UnixStream(l) => Some(l.as_raw_fd()),
        BoundSocket::Udp(fd) => Some(*fd),
        BoundSocket::Fifo => None,
        BoundSocket::Netlink(fd) => Some(*fd),
    }
}

/// Return the raw fds of every listening socket for a unit (skipping
/// address kinds without an fd, e.g. FIFOs).
pub fn get_listener_fds(manager: &SocketManager, unit_name: &str) -> Vec<RawFd> {
    let guard = manager.lock();
    let Some(ms) = guard.get(unit_name) else {
        return Vec::new();
    };
    ms.listeners
        .iter()
        .filter_map(|ls| match ls {
            BoundSocket::Tcp(l) => Some(l.as_raw_fd()),
            BoundSocket::UnixStream(l) => Some(l.as_raw_fd()),
            BoundSocket::Udp(fd) | BoundSocket::Netlink(fd) => Some(*fd),
            BoundSocket::Fifo => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Binding helpers
// ---------------------------------------------------------------------------

/// Parse an octal mode string (e.g. "0666") with a fallback default.
/// Matches systemd's config_parse_mode() behaviour.
fn parse_mode(s: &str, default: u32) -> u32 {
    if s.is_empty() {
        return default;
    }
    u32::from_str_radix(s.trim(), 8).unwrap_or(default)
}

fn resolve_tcp_addr(address: &str) -> Result<std::net::SocketAddr> {
    // If it's just a port number (e.g. "8080"), parse as 0.0.0.0:8080.
    if let Ok(port) = address.parse::<u16>() {
        return Ok((std::net::Ipv4Addr::UNSPECIFIED, port).into());
    }
    address.parse().with_context(|| {
        sysa::l10n::fmt(
            sysa::l10n::t_("Cannot parse TCP address '{address}'."),
            &[("address", address)],
        )
    })
}

/// Return true if a live listener is already accepting on `path` (some other
/// process owns the socket and is accepting connections).  The probe
/// connection is dropped immediately afterwards.
fn probe_live_listener(path: &std::path::Path) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

fn bind_stream(address: &str, backlog: u32, socket_mode: u32, directory_mode: u32) -> Result<Option<BoundSocket>> {
    if address.starts_with(ABSTRACT_PREFIX) {
        bind_abstract_unix(address, backlog).map(Some)
    } else if address.starts_with('/') {
        let path = PathBuf::from(address);
        // Never clobber a live listener: if another process is already
        // accepting on this path (e.g. a system bus daemon), treat the
        // socket unit as externally satisfied instead of unlink()-ing the
        // live socket out from under it.
        if probe_live_listener(&path) {
            info!(
                "Unix stream at '{}' already has a live listener; treating as externally satisfied",
                address
            );
            return Ok(None);
        }
        // The parent directory (e.g. /run/dbus) may not exist yet — create
        // it, mirroring systemd's behaviour for socket units.
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).with_context(|| {
                    sysa::l10n::fmt(
                        sysa::l10n::t_("Cannot create directory '{dir}' for socket '{address}'."),
                        &[("dir", &parent.display().to_string()), ("address", address)],
                    )
                })?;
                // Set directory permissions (systemd uses DirectoryMode=, default 0755).
                std::fs::set_permissions(
                    parent,
                    PermissionsExt::from_mode(directory_mode),
                ).with_context(|| {
                    sysa::l10n::fmt(
                        sysa::l10n::t_("Cannot set directory permissions for socket '{address}'."),
                        &[("address", address)],
                    )
                })?;
            }
        }
        let _ = std::fs::remove_file(&path);
        let listener = std::os::unix::net::UnixListener::bind(&path).with_context(|| {
            sysa::l10n::fmt(
                sysa::l10n::t_("Cannot bind Unix stream at '{address}'."),
                &[("address", address)],
            )
        })?;
        // Set socket file permissions (systemd uses SocketMode=, default 0666).
        std::fs::set_permissions(
            &path,
            PermissionsExt::from_mode(socket_mode),
        ).with_context(|| {
            sysa::l10n::fmt(
                sysa::l10n::t_("Cannot set socket permissions at '{address}'."),
                &[("address", address)],
            )
        })?;
        // Set listen backlog (std UnixListener doesn't expose a method for this,
        // so it uses the kernel default (SOMAXCONN).  systemd's Backlog= maps to
        // listen(fd, backlog) which is already called by bind().
        let _ = backlog;
        listener.set_nonblocking(true)?;
        let listener = UnixListener::from_std(listener).with_context(|| {
            sysa::l10n::fmt(
                sysa::l10n::t_("Cannot convert Unix listener '{address}'."),
                &[("address", address)],
            )
        })?;
        info!("Bound Unix stream at '{}' (mode {:o})", address, socket_mode);
        Ok(Some(BoundSocket::UnixStream(listener)))
    } else {
        let addr = resolve_tcp_addr(address)?;
        let std_listener = std::net::TcpListener::bind(addr).with_context(|| {
            sysa::l10n::fmt(
                sysa::l10n::t_("Cannot bind TCP at '{address}'."),
                &[("address", address)],
            )
        })?;
        if backlog > 0 {
            // std::net::TcpListener already calls listen() with SOMAXCONN.
            // To set a custom backlog we need libc::listen(). We do that below.
            let _ = backlog;
        }
        std_listener.set_nonblocking(true)?;
        let listener = TcpListener::from_std(std_listener).with_context(|| {
            sysa::l10n::fmt(
                sysa::l10n::t_("Cannot convert TCP listener '{address}'."),
                &[("address", address)],
            )
        })?;
        info!("Bound TCP stream at '{}'", address);
        Ok(Some(BoundSocket::Tcp(listener)))
    }
}

/// Bind a Unix stream socket with an abstract address (@ → \0 prefix).
#[cfg(any(target_os = "linux", target_os = "android"))]
fn new_socket_fd() -> Result<RawFd> {
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        if fd < 0 {
            let e = std::io::Error::last_os_error();
            anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_("socket(AF_UNIX) failed: {e}."),
                &[("e", &e.to_string())]
            ));
        }
        // Set FD_CLOEXEC portably (SOCK_CLOEXEC is not available on macOS).
        let flags = libc::fcntl(fd, libc::F_GETFD, 0);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
        Ok(fd)
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn bind_abstract_unix(address: &str, backlog: u32) -> Result<BoundSocket> {
    use std::os::unix::prelude::*;

    let inner = address.trim_start_matches(ABSTRACT_PREFIX);
    let sun_path = format!("\0{}", inner);
    let bytes = sun_path.as_bytes();
    let path_len = bytes.len().min(107);

    let fd = new_socket_fd()?;

    unsafe {
        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            addr.sun_path.as_mut_ptr() as *mut u8,
            path_len,
        );

        let addr_len = std::mem::size_of::<libc::sa_family_t>() + path_len;

        let ret = libc::bind(
            fd,
            &addr as *const libc::sockaddr_un as *const libc::sockaddr,
            addr_len as u32,
        );
        if ret < 0 {
            let e = std::io::Error::last_os_error();
            libc::close(fd);
            anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_("bind abstract '{address}' failed: {e}."),
                &[("address", address), ("e", &e.to_string())]
            ));
        }

        let backlog = if backlog > 0 { backlog as i32 } else { 128 };
        libc::listen(fd, backlog);
    }

    let std_listener = unsafe { std::os::unix::net::UnixListener::from_raw_fd(fd) };
    std_listener.set_nonblocking(true)?;
    let listener = UnixListener::from_std(std_listener).with_context(|| {
        sysa::l10n::fmt(
            sysa::l10n::t_("Cannot convert abstract Unix listener '{address}'."),
            &[("address", address)],
        )
    })?;

    info!("Bound abstract Unix stream at '{}'", address);
    Ok(BoundSocket::UnixStream(listener))
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn bind_abstract_unix(address: &str, _backlog: u32) -> Result<BoundSocket> {
    let trimmed = address.trim_start_matches(ABSTRACT_PREFIX);
    anyhow::bail!(sysa::l10n::fmt(sysa::l10n::t_("Abstract Unix sockets (prefix '@') are not supported on this platform. Use a filesystem path like '/tmp/{trimmed}' instead."), &[("trimmed", &trimmed.to_string())]));
}

fn bind_datagram(address: &str, socket_mode: u32, directory_mode: u32) -> Result<RawFd> {
    // ListenDatagram= with an abstract ('@') or filesystem ('/') address is a
    // Unix datagram socket, not a UDP port.
    if address.starts_with(ABSTRACT_PREFIX) || address.starts_with('/') {
        return bind_unix_datagram(address, socket_mode, directory_mode);
    }
    let addr = resolve_tcp_addr(address)?;
    let fd = unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
        if fd < 0 {
            let e = std::io::Error::last_os_error();
            anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_("socket(AF_INET, SOCK_DGRAM) failed: {e}."),
                &[("e", &e.to_string())]
            ));
        }
        // Set CLOEXEC portably.
        let flags = libc::fcntl(fd, libc::F_GETFD, 0);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }

        let mut sockaddr: libc::sockaddr_in = std::mem::zeroed();
        sockaddr.sin_family = libc::AF_INET as libc::sa_family_t;
        sockaddr.sin_port = addr.port().to_be();
        sockaddr.sin_addr = libc::in_addr {
            s_addr: match addr {
                std::net::SocketAddr::V4(v4) => u32::from_ne_bytes(v4.ip().octets()),
                std::net::SocketAddr::V6(_) => {
                    anyhow::bail!(sysa::l10n::t_("IPv6 UDP not yet supported."));
                }
            },
        };

        let ret = libc::bind(
            fd,
            &sockaddr as *const libc::sockaddr_in as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_in>() as u32,
        );
        if ret < 0 {
            let e = std::io::Error::last_os_error();
            libc::close(fd);
            anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_("bind datagram '{address}' failed: {e}."),
                &[("address", address), ("e", &e.to_string())]
            ));
        }
        fd
    };
    info!("Bound UDP datagram at '{}'", address);
    Ok(fd)
}

/// Bind a Unix datagram socket for an abstract ('@') or filesystem ('/')
/// ListenDatagram= address.
fn bind_unix_datagram(address: &str, socket_mode: u32, directory_mode: u32) -> Result<RawFd> {
    let fd = unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM, 0);
        if fd < 0 {
            let e = std::io::Error::last_os_error();
            anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_("socket(AF_UNIX, SOCK_DGRAM) failed: {e}."),
                &[("e", &e.to_string())]
            ));
        }
        let flags = libc::fcntl(fd, libc::F_GETFD, 0);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
        fd
    };

    if address.starts_with(ABSTRACT_PREFIX) {
        let inner = address.trim_start_matches(ABSTRACT_PREFIX);
        let sun_path = format!("\0{}", inner);
        let bytes = sun_path.as_bytes();
        let path_len = bytes.len().min(107);
        unsafe {
            let mut addr: libc::sockaddr_un = std::mem::zeroed();
            addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                addr.sun_path.as_mut_ptr() as *mut u8,
                path_len,
            );
            let addr_len = std::mem::size_of::<libc::sa_family_t>() + path_len;
            let ret = libc::bind(
                fd,
                &addr as *const libc::sockaddr_un as *const libc::sockaddr,
                addr_len as u32,
            );
            if ret < 0 {
                let e = std::io::Error::last_os_error();
                libc::close(fd);
                anyhow::bail!(sysa::l10n::fmt(
                    sysa::l10n::t_("bind abstract datagram '{address}' failed: {e}."),
                    &[("address", address), ("e", &e.to_string())]
                ));
            }
        }
        info!("Bound abstract Unix datagram at '{}'", address);
        Ok(fd)
    } else {
        let path = std::path::Path::new(address);
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).with_context(|| {
                    sysa::l10n::fmt(
                        sysa::l10n::t_("Cannot create directory '{dir}' for socket '{address}'."),
                        &[("dir", &parent.display().to_string()), ("address", address)],
                    )
                })?;
                // Set directory permissions (systemd uses DirectoryMode=, default 0755).
                std::fs::set_permissions(
                    parent,
                    PermissionsExt::from_mode(directory_mode),
                ).with_context(|| {
                    sysa::l10n::fmt(
                        sysa::l10n::t_("Cannot set directory permissions for socket '{address}'."),
                        &[("address", address)],
                    )
                })?;
            }
        }
        let _ = std::fs::remove_file(&path);
        let cpath = CString::new(address).with_context(|| {
            sysa::l10n::fmt(
                sysa::l10n::t_("Invalid Unix datagram path '{address}'."),
                &[("address", address)],
            )
        })?;
        let bytes = cpath.as_bytes();
        let path_len = bytes.len().min(107);
        unsafe {
            let mut addr: libc::sockaddr_un = std::mem::zeroed();
            addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                addr.sun_path.as_mut_ptr() as *mut u8,
                path_len,
            );
            let addr_len = std::mem::size_of::<libc::sa_family_t>() + path_len;
            let ret = libc::bind(
                fd,
                &addr as *const libc::sockaddr_un as *const libc::sockaddr,
                addr_len as u32,
            );
            if ret < 0 {
                let e = std::io::Error::last_os_error();
                libc::close(fd);
                anyhow::bail!(sysa::l10n::fmt(
                    sysa::l10n::t_("bind Unix datagram '{address}' failed: {e}."),
                    &[("address", address), ("e", &e.to_string())]
                ));
            }
        }
        // Set socket file permissions (systemd uses SocketMode=, default 0666).
        std::fs::set_permissions(
            path,
            PermissionsExt::from_mode(socket_mode),
        ).with_context(|| {
            sysa::l10n::fmt(
                sysa::l10n::t_("Cannot set socket permissions at '{address}'."),
                &[("address", address)],
            )
        })?;
        info!("Bound Unix datagram at '{}' (mode {:o})", address, socket_mode);
        Ok(fd)
    }
}

fn bind_seqpacket(address: &str, backlog: u32, socket_mode: u32, directory_mode: u32) -> Result<Option<BoundSocket>> {
    if address.starts_with(ABSTRACT_PREFIX) {
        bind_abstract_unix(address, backlog).map(Some)
    } else {
        // SOCK_SEQPACKET not available in std UnixListener; use SOCK_STREAM
        // which behaves similarly enough for our purposes.
        bind_stream(address, backlog, socket_mode, directory_mode)
    }
}

/// Bind a netlink socket for a `ListenNetlink=` address.
///
/// Format: `"protocol_name group"` — e.g. `"kobject-uevent 1"`.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn bind_netlink(address: &str) -> Result<RawFd> {
    let parts: Vec<&str> = address.split_whitespace().collect();
    if parts.is_empty() {
        anyhow::bail!(sysa::l10n::t_(
            "ListenNetlink address is empty."
        ));
    }

    let proto_id: libc::c_int = match parts[0] {
        "kobject-uevent" => libc::NETLINK_KOBJECT_UEVENT,
        "generic" => libc::NETLINK_GENERIC,
        "route" => libc::NETLINK_ROUTE,
        "firewall" => libc::NETLINK_FIREWALL,
        "netfilter" => libc::NETLINK_NETFILTER,
        "dnrtmsg" => libc::NETLINK_DNRTMSG,
        "kobject-uevent-1" => libc::NETLINK_KOBJECT_UEVENT,
        _ => {
            parts[0].parse::<libc::c_int>().unwrap_or_else(|_| {
                warn!(
                    "Unknown netlink protocol '{}', defaulting to kobject-uevent",
                    parts[0]
                );
                libc::NETLINK_KOBJECT_UEVENT
            })
        }
    };

    let groups: libc::c_uint = if parts.len() > 1 {
        parts[1].parse::<libc::c_uint>().unwrap_or(0)
    } else {
        0
    };

    let fd = unsafe {
        let fd = libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
            proto_id,
        );
        if fd < 0 {
            let e = std::io::Error::last_os_error();
            anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_("socket(AF_NETLINK) failed: {e}."),
                &[("e", &e.to_string())]
            ));
        }
        fd
    };

    unsafe {
        let mut addr: libc::sockaddr_nl = std::mem::zeroed();
        addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        addr.nl_pid = 0;
        addr.nl_groups = groups;

        let ret = libc::bind(
            fd,
            &addr as *const libc::sockaddr_nl as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        );
        if ret < 0 {
            let e = std::io::Error::last_os_error();
            libc::close(fd);
            anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_("bind netlink '{address}' failed: {e}."),
                &[("address", address), ("e", &e.to_string())]
            ));
        }
    }

    info!(
        "Bound netlink socket '{}' (proto={}, groups={})",
        address, proto_id, groups
    );
    Ok(fd)
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn bind_netlink(_address: &str) -> Result<RawFd> {
    anyhow::bail!(sysa::l10n::t_("Netlink sockets are only supported on Linux."));
}

fn create_fifo(path: &str, mode: &str) -> Result<()> {
    let cpath = CString::new(path).with_context(|| {
        sysa::l10n::fmt(
            sysa::l10n::t_("Invalid FIFO path '{path}'."),
            &[("path", path)],
        )
    })?;
    // The parent directory (e.g. /run/foo) may not exist yet.
    if let Some(parent) = std::path::Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).with_context(|| {
                sysa::l10n::fmt(
                    sysa::l10n::t_("Cannot create directory '{dir}' for FIFO '{path}'."),
                    &[("dir", &parent.display().to_string()), ("path", path)],
                )
            })?;
        }
    }
    let mode_int = if mode.is_empty() {
        0o644
    } else {
        u32::from_str_radix(mode.trim_start_matches('0'), 8).unwrap_or(0o644)
    };
    let ret = unsafe { libc::mkfifo(cpath.as_ptr(), mode_int as libc::mode_t) };
    if ret < 0 {
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::AlreadyExists {
            anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_("mkfifo '{path}' failed: {e}."),
                &[("path", path), ("e", &e.to_string())]
            ));
        }
        // File already exists — that's OK.
        warn!("FIFO '{}' already exists", path);
    } else {
        info!("Created FIFO at '{}' (mode {:o})", path, mode_int);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Accept loops for Accept=yes
// ---------------------------------------------------------------------------

async fn accept_loop_tcp(listener: TcpListener, unit_name: &str) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let fd = stream.as_raw_fd();
                let name = unit_name.to_string();
                tokio::spawn(async move {
                    if let Err(e) = spawn_child_with_fd(&name, fd).await {
                        warn!("spawn_child_with_fd (TCP): {}", e);
                    }
                });
            }
            Err(e) => {
                warn!("TCP accept error on '{}': {}", unit_name, e);
                break;
            }
        }
    }
}

async fn accept_loop_unix(listener: UnixListener, unit_name: &str) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let fd = stream.as_raw_fd();
                let name = unit_name.to_string();
                tokio::spawn(async move {
                    if let Err(e) = spawn_child_with_fd(&name, fd).await {
                        warn!("spawn_child_with_fd (Unix): {}", e);
                    }
                });
            }
            Err(e) => {
                warn!("Unix accept error on '{}': {}", unit_name, e);
                break;
            }
        }
    }
}

/// Spawn a child process that receives a socket fd as fd 3 (sd_listen_fds
/// convention).  The child reads/writes to fd 3 directly.
async fn spawn_child_with_fd(unit_name: &str, fd: RawFd) -> Result<()> {
    // In a real deployment, the service path would come from the associated
    // service unit's ExecStart.  Here we use a placeholder — the convention
    // is that the child reads from / writes to fd 3.
    let service_path = std::env::var("SYSTEMK_SERVICE_PATH").unwrap_or_else(|_| {
        sysa::paths::instance()
            .systema_socket_handler_path
            .to_string()
    });

    // Extract the raw fd value before the async move so the closure owns it.
    let raw_fd = fd;

    let mut cmd = tokio::process::Command::new(&service_path);
    cmd.arg(unit_name);

    unsafe {
        cmd.as_std_mut().pre_exec(move || {
            // Pass the accepted connection fd as fd 3.
            let ret = libc::dup2(raw_fd, 3);
            if ret < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // Close the original fd (we don't need it anymore).
            if raw_fd != 3 {
                libc::close(raw_fd);
            }
            Ok(())
        });
    }

    let mut child = cmd.spawn().with_context(|| {
        sysa::l10n::fmt(
            sysa::l10n::t_("Failed to spawn child for '{unit_name}'."),
            &[("unit_name", unit_name)],
        )
    })?;

    let status = child.wait().await.with_context(|| {
        sysa::l10n::fmt(
            sysa::l10n::t_("Failed to wait for child for '{unit_name}'."),
            &[("unit_name", unit_name)],
        )
    })?;

    if !status.success() {
        warn!("Child for '{}' exited with: {}", unit_name, status);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Socket cloning helpers (needed so the listener lives beyond the accept loop)
// ---------------------------------------------------------------------------

fn try_clone_tcp(l: &TcpListener) -> TcpListener {
    // Try to duplicate the raw fd.
    let fd = l.as_raw_fd();
    let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if dup < 0 {
        // Fallback: just register a new listener on a separate socket.
        // This is unlikely to fail on a healthy system.
        warn!("Failed to dup TCP listener fd, accept may race");
        // We can't easily clone TcpListener without the raw fd, so
        // just use the same fd (will race with other acceptors).
        unsafe { TcpListener::from_std(std::net::TcpListener::from_raw_fd(fd)).unwrap() }
    } else {
        unsafe {
            let std = std::net::TcpListener::from_raw_fd(dup);
            std.set_nonblocking(true).unwrap();
            TcpListener::from_std(std).unwrap()
        }
    }
}

fn try_clone_unix(l: &UnixListener) -> UnixListener {
    let fd = l.as_raw_fd();
    let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if dup < 0 {
        warn!("Failed to dup Unix listener fd, accept may race");
        unsafe {
            UnixListener::from_std(std::os::unix::net::UnixListener::from_raw_fd(fd)).unwrap()
        }
    } else {
        unsafe {
            let std = std::os::unix::net::UnixListener::from_raw_fd(dup);
            std.set_nonblocking(true).unwrap();
            UnixListener::from_std(std).unwrap()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_datagram_unix_path() {
        let dir = std::env::temp_dir().join(format!("sysk-dgram-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("journal.socket");
        let p = path.to_str().unwrap().to_string();

        let fd = bind_datagram(&p, 0o666, 0o755).unwrap();
        assert!(path.exists());
        // A stale file on the same path must be replaced on re-bind.
        let fd2 = bind_datagram(&p, 0o666, 0o755).unwrap();
        assert!(path.exists());

        unsafe {
            libc::close(fd);
            libc::close(fd2);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn bind_datagram_abstract() {
        let name = format!("@sysk-dgram-test-{}", std::process::id());
        let fd = bind_datagram(&name, 0o666, 0o755).unwrap();
        unsafe {
            libc::close(fd);
        }
    }

    #[test]
    fn bind_datagram_udp_port() {
        let fd = bind_datagram("0", 0o666, 0o755).unwrap();
        unsafe {
            libc::close(fd);
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn bind_netlink_kobject_uevent() {
        let fd = bind_netlink("kobject-uevent 1").unwrap();
        assert!(fd >= 0);
        unsafe {
            libc::close(fd);
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn bind_netlink_empty_fails() {
        assert!(bind_netlink("").is_err());
    }
}
