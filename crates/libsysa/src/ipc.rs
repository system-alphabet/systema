use std::os::unix::io::AsRawFd;

use anyhow::{Context, Result};
use bytes::{Bytes, BytesMut};
use futures::{SinkExt, StreamExt};
use prost::Message;
use tokio::net::UnixStream;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use crate::proto::Envelope;

/// A framed Unix stream that sends/receives length-delimited protobuf envelopes.
pub type EnvelopeFramed = Framed<UnixStream, LengthDelimitedCodec>;

/// Wrap a `UnixStream` with the length-delimited codec.
pub fn frame_stream(stream: UnixStream) -> EnvelopeFramed {
    let codec = LengthDelimitedCodec::builder()
        .max_frame_length(16 * 1024 * 1024) // 16 MiB max frame
        .new_codec();
    Framed::new(stream, codec)
}

/// Encode and send an `Envelope` over a framed stream.
pub async fn send_envelope(framed: &mut EnvelopeFramed, envelope: &Envelope) -> Result<()> {
    let mut buf = BytesMut::new();
    envelope
        .encode(&mut buf)
        .context(crate::l10n::t_("Failed to encode Envelope."))?;
    framed
        .send(buf.freeze())
        .await
        .context(crate::l10n::t_("Failed to send Envelope frame."))?;
    Ok(())
}

/// Receive the next `Envelope` from a framed stream.
/// Returns `None` if the stream is closed.
pub async fn recv_envelope(framed: &mut EnvelopeFramed) -> Result<Option<Envelope>> {
    match framed.next().await {
        None => Ok(None),
        Some(result) => {
            let bytes: Bytes = result
                .context(crate::l10n::t_("Frame receive error."))?
                .freeze();
            let envelope =
                Envelope::decode(bytes).context(crate::l10n::t_("Failed to decode Envelope."))?;
            Ok(Some(envelope))
        }
    }
}

/// Build an `Envelope` for a method call or event.
pub fn make_envelope(
    request_id: u64,
    source: impl Into<String>,
    target: impl Into<String>,
    method: impl Into<String>,
    payload: impl Message,
) -> Result<Envelope> {
    let mut buf = BytesMut::new();
    payload
        .encode(&mut buf)
        .context(crate::l10n::t_("Failed to encode payload."))?;
    Ok(Envelope {
        request_id,
        source: source.into(),
        target: target.into(),
        method: method.into(),
        payload: buf.freeze().to_vec(),
    })
}

// ---------------------------------------------------------------------------
// SCM_RIGHTS fd-passing helpers (dedicated connections only — not with Framed)
// ---------------------------------------------------------------------------

/// Send a single file descriptor over a Unix stream via SCM_RIGHTS.
/// The stream must be a dedicated raw connection (not wrapped in `Framed`).
pub async fn send_fd(stream: &UnixStream, fd: std::os::unix::io::RawFd) -> Result<()> {
    let raw = stream.as_raw_fd();
    tokio::task::spawn_blocking(move || send_fd_sync(raw, fd))
        .await
        .context(crate::l10n::t_("SCM_RIGHTS send task panicked."))?
}

/// Receive a single file descriptor from a Unix stream via SCM_RIGHTS.
/// The stream must be a dedicated raw connection (not wrapped in `Framed`).
pub async fn recv_fd(stream: &UnixStream) -> Result<std::os::unix::io::RawFd> {
    let raw = stream.as_raw_fd();
    tokio::task::spawn_blocking(move || recv_fd_sync(raw))
        .await
        .context(crate::l10n::t_("SCM_RIGHTS recv task panicked."))?
}

/// Synchronous SCM_RIGHTS send using libc::sendmsg.
/// Retries `EAGAIN` (non-blocking socket, send buffer full).
fn send_fd_sync(sock_fd: std::os::unix::io::RawFd, fd: std::os::unix::io::RawFd) -> Result<()> {
    unsafe {
        // Linux ignores zero-length sendmsg() on stream sockets entirely
        // (the SCM_RIGHTS cmsg is dropped with it), so always send one
        // payload byte alongside the descriptor.
        let mut pad: u8 = 0;
        let mut iov = libc::iovec {
            iov_base: &mut pad as *mut u8 as *mut libc::c_void,
            iov_len: 1,
        };
        let mut cmsg_buf = [0u8; 24];
        let mut msghdr: libc::msghdr = std::mem::zeroed();

        msghdr.msg_iov = &mut iov;
        msghdr.msg_iovlen = 1;
        msghdr.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
        msghdr.msg_controllen = cmsg_buf.len() as _;

        let cmsg = libc::CMSG_FIRSTHDR(&msghdr);
        if cmsg.is_null() {
            anyhow::bail!(crate::l10n::t_("CMSG_FIRSTHDR returned null."));
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        let fd_size = std::mem::size_of::<libc::c_int>();
        (*cmsg).cmsg_len = libc::CMSG_LEN(fd_size as u32) as _;
        std::ptr::write(libc::CMSG_DATA(cmsg) as *mut libc::c_int, fd);
        msghdr.msg_controllen = libc::CMSG_SPACE(fd_size as u32) as _;

        let ret = loop {
            let r = libc::sendmsg(sock_fd, &msghdr, 0);
            if r < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::WouldBlock {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    continue;
                }
                anyhow::bail!(crate::l10n::fmt(
                    crate::l10n::t_("sendmsg (SCM_RIGHTS) failed: {e}."),
                    &[("e", &e.to_string())]
                ));
            }
            break r;
        };
        debug_assert!(ret >= 0);
    }
    Ok(())
}

/// Synchronous SCM_RIGHTS receive using libc::recvmsg.
///
/// The stream may be non-blocking (tokio UnixStream): `EAGAIN` is retried
/// with a short sleep until data arrives or the peer closes the connection.
fn recv_fd_sync(sock_fd: std::os::unix::io::RawFd) -> Result<std::os::unix::io::RawFd> {
    unsafe {
        let mut data: u8 = 0;
        let mut iov = libc::iovec {
            iov_base: &mut data as *mut u8 as *mut libc::c_void,
            iov_len: 1,
        };
        let mut cmsg_buf = [0u8; 24];
        let mut msghdr: libc::msghdr = std::mem::zeroed();

        msghdr.msg_iov = &mut iov;
        msghdr.msg_iovlen = 1;
        msghdr.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
        msghdr.msg_controllen = cmsg_buf.len() as _;

        let ret = loop {
            let r = libc::recvmsg(sock_fd, &mut msghdr, 0);
            if r < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::WouldBlock {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    continue;
                }
                anyhow::bail!(crate::l10n::fmt(
                    crate::l10n::t_("recvmsg (SCM_RIGHTS) failed: {e}."),
                    &[("e", &e.to_string())]
                ));
            }
            break r;
        };
        if ret == 0 {
            anyhow::bail!(crate::l10n::t_(
                "fdpass connection closed while waiting for a listener fd."
            ));
        }

        let mut received_fd: Option<std::os::unix::io::RawFd> = None;
        let mut cmsg = libc::CMSG_FIRSTHDR(&msghdr);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let fd_ptr = libc::CMSG_DATA(cmsg) as *const libc::c_int;
                received_fd = Some(std::ptr::read(fd_ptr));
                break;
            }
            cmsg = libc::CMSG_NXTHDR(&msghdr, cmsg);
        }

        match received_fd {
            Some(fd) => Ok(fd),
            None => anyhow::bail!(crate::l10n::t_("recvmsg did not contain SCM_RIGHTS fd.")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fd_passes_over_stream_with_data() {
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        let fd = unsafe {
            libc::open(
                b"/dev/null\0".as_ptr() as *const libc::c_char,
                libc::O_RDONLY,
            )
        };
        assert!(fd >= 0);

        let sender = std::thread::spawn(move || send_fd_sync(a.as_raw_fd(), fd).unwrap());
        let received = recv_fd_sync(b.as_raw_fd()).unwrap();
        sender.join().unwrap();

        assert_ne!(received, fd);
        // The received descriptor must be a usable duplicate.
        let flags = unsafe { libc::fcntl(received, libc::F_GETFD, 0) };
        assert!(flags >= 0);
        unsafe {
            libc::close(fd);
            libc::close(received);
        }
    }
}
