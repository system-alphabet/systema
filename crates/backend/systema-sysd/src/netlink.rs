//! Hotplug notification.
//!
//! On Linux we bind a `NETLINK_KOBJECT_UEVENT` socket and wake the engine on
//! any uevent; we don't parse the payload — the engine re-scans `/dev` +
//! sysfs and reconciles, which is correct even though it's rescan-triggered.
//! Netlink is an optimisation: polling alone is always correct.
//!
//! On non-Linux this module is a no-op (poll-only).

#[cfg(any(target_os = "linux", target_os = "android"))]
use std::io;

use tokio::sync::mpsc::UnboundedSender;
use tracing::debug;
#[cfg(any(target_os = "linux", target_os = "android"))]
use tracing::warn;

/// Spawn a background thread that pushes `()` to `tx` whenever a kernel
/// uevent arrives.  Returns true if the watcher is running.
pub fn spawn_uevent_watcher(tx: UnboundedSender<()>) -> bool {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        spawn_linux_watcher(tx)
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let _ = tx;
        debug!("netlink uevents not available on this platform; polling only");
        false
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn spawn_linux_watcher(tx: UnboundedSender<()>) -> bool {
    use std::os::fd::FromRawFd;

    let fd = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC,
            libc::NETLINK_KOBJECT_UEVENT,
        )
    };
    if fd < 0 {
        warn!(
            "Cannot open netlink uevent socket ({}); falling back to polling",
            io::Error::last_os_error()
        );
        return false;
    }

    let mut ads: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    ads.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    ads.nl_pid = 0;
    ads.nl_groups = 1;
    let rc = unsafe {
        libc::bind(
            fd,
            &ads as *const libc::sockaddr_nl as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        let err = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        warn!("netlink uevent bind failed ({err}); falling back to polling");
        return false;
    }

    // Move the fd into the watcher thread; the socket object keeps it open.
    let socket = unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) };
    std::thread::Builder::new()
        .name("systema-d-uevent".into())
        .spawn(move || watch_loop(socket, tx))
        .map(|_| {
            debug!("netlink uevent watcher started");
            true
        })
        .unwrap_or_else(|e| {
            warn!("Cannot spawn uevent watcher thread: {e}");
            false
        })
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn watch_loop(socket: std::os::unix::net::UnixStream, tx: UnboundedSender<()>) {
    use std::os::fd::AsRawFd;
    let mut buf = [0u8; 4096];
    loop {
        let n = unsafe {
            libc::recv(
                socket.as_raw_fd(),
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                libc::MSG_DONTWAIT | libc::MSG_TRUNC,
            )
        };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock {
                std::thread::sleep(std::time::Duration::from_millis(50));
                continue;
            }
            debug!("netlink uevent read error, exiting watcher: {err}");
            return;
        }
        if n == 0 {
            // Not ready — sleep briefly and keep reading.
            std::thread::sleep(std::time::Duration::from_millis(50));
            continue;
        }
        let events = buf[..n as usize].iter().filter(|&&b| b == 0).count();
        debug!("uevent packet received ({events} events)");
        if tx.send(()).is_err() {
            return;
        }
    }
}