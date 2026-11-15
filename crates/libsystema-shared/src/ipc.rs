use std::os::unix::io::AsRawFd;
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::{Bytes, BytesMut};
use futures::{SinkExt, StreamExt};
use prost::Message;
use tokio::net::UnixStream;
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use tracing::debug;

use crate::error_code::{ErrorCode, SysError};
use crate::proto::{Envelope, IpcVersionRequest, IpcVersionResult};

/// A framed Unix stream that sends/receives length-delimited protobuf envelopes.
pub type EnvelopeFramed = Framed<UnixStream, LengthDelimitedCodec>;

/// Connect to a control-port bus socket (`control.socket`).
///
/// A kernel permission refusal — an unprivileged caller reaching for the
/// *system* instance's socket — gets the cross-domain hint appended: P1
/// keeps the two domains strictly separate, so the system domain is
/// reachable only with elevated privileges, while `--user` targets the
/// caller's own per-user instance.  Everything else keeps the plain
/// message, so a merely missing instance still reports `ENOENT`.
pub async fn connect_control(path: &str) -> Result<UnixStream> {
    match UnixStream::connect(path).await {
        Ok(stream) => Ok(stream),
        Err(e) => {
            let code = if e.kind() == std::io::ErrorKind::PermissionDenied {
                ErrorCode::ConnectPermissionDenied
            } else {
                ErrorCode::ConnectFailed
            };
            Err(anyhow::Error::new(
                SysError::new(code).with("e", e.to_string()),
            ))
        }
    }
}

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
        .context(SysError::new(ErrorCode::EnvelopeEncodeFailed))?;
    framed
        .send(buf.freeze())
        .await
        .context(SysError::new(ErrorCode::EnvelopeSendFailed))?;
    Ok(())
}

/// Receive the next `Envelope` from a framed stream.
/// Returns `None` if the stream is closed.
pub async fn recv_envelope(framed: &mut EnvelopeFramed) -> Result<Option<Envelope>> {
    match framed.next().await {
        None => Ok(None),
        Some(result) => {
            let bytes: Bytes = result
                .context(SysError::new(ErrorCode::FrameReceiveFailed))?
                .freeze();
            let envelope =
                Envelope::decode(bytes).context(SysError::new(ErrorCode::EnvelopeDecodeFailed))?;
            Ok(Some(envelope))
        }
    }
}

/// Upper bound for draining a one-shot control session after its terminal
/// reply.  A healthy server sees our FIN, stops pushing and closes within one
/// round trip; the bound only protects against a wedged peer.
pub const CONTROL_DRAIN_TIMEOUT: Duration = Duration::from_millis(500);

/// Finish a one-shot control session without turning the close into an RST on
/// the peer.
///
/// A control session is also an event stream: the server keeps pushing
/// `job.*`/`unit.*` envelopes after the `{method}.result` the caller came
/// for.  Closing the socket with those frames still queued makes Linux set
/// `ECONNRESET` on the peer (`unix_release_sock()` marks the peer errored
/// when the closing socket's receive queue is non-empty).  Half-close the
/// write side first — the server then reads EOF, stops pushing and closes —
/// and drain the frames that were already in flight before dropping.
pub async fn finish_control_session(framed: &mut EnvelopeFramed, drain_timeout: Duration) {
    use tokio::io::AsyncWriteExt;

    // `send_envelope` flushes before returning, so the write buffer is empty
    // here: the FIN cannot truncate a request frame.
    if let Err(e) = framed.get_mut().shutdown().await {
        debug!("control session shutdown failed: {e}");
        return;
    }
    let drain = async {
        while let Some(frame) = framed.next().await {
            if frame.is_err() {
                break;
            }
        }
    };
    let _ = tokio::time::timeout(drain_timeout, drain).await;
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
        .context(SysError::new(ErrorCode::PayloadEncodeFailed))?;
    Ok(Envelope {
        request_id,
        source: source.into(),
        target: target.into(),
        method: method.into(),
        payload: buf.freeze().to_vec(),
    })
}

// ---------------------------------------------------------------------------
// IPC protocol version exchange (wire layer, before any session semantics)
// ---------------------------------------------------------------------------

/// The IPC protocol version of the envelope/framing wire layer in this
/// build.  The allocator currently pins a single version: a peer that
/// announces anything else gets `ipc.version.result{compatible:false}` and
/// its connection is closed before any session semantics — hello,
/// registration, admin requests — ever start.  A min/max negotiation may
/// widen this later (`doc/ipc-api-stability.md` §4.1).
pub const IPC_VERSION: u32 = 1;

/// First envelope of any new session — client → server, before hello.
pub const METHOD_VERSION: &str = "ipc.version";

/// The verdict to [`METHOD_VERSION`] — server → client, echoed `request_id`.
pub const METHOD_VERSION_RESULT: &str = "ipc.version.result";

/// Client-side pre-hello version exchange.
///
/// Announces [`IPC_VERSION`] and awaits the verdict.  Compatibility holds
/// only when the reply is `ipc.version.result`, its `compatible` is true
/// *and* the echoed server version is one we speak (for now: equal).  Any
/// other reply — `manager.error`, an old server's unknown-method rejection,
/// EOF — is an incompatibility, and the caller must drop the stream either
/// way.  Returns the server's version so callers can log the negotiated
/// pair.
pub async fn exchange_version(framed: &mut EnvelopeFramed, client: &str) -> Result<u32> {
    let request = make_envelope(
        0,
        client,
        "system-a",
        METHOD_VERSION,
        IpcVersionRequest {
            version: IPC_VERSION,
            client: client.to_string(),
        },
    )?;
    send_envelope(framed, &request).await?;

    let reply = recv_envelope(framed)
        .await?
        .ok_or_else(|| anyhow::Error::new(SysError::new(ErrorCode::VersionPeerClosed)))?;
    if reply.method != METHOD_VERSION_RESULT {
        return Err(anyhow::Error::new(
            SysError::new(ErrorCode::VersionUnexpectedReply).with("method", reply.method.clone()),
        ));
    }
    let result = IpcVersionResult::decode(reply.payload.as_slice())
        .context(SysError::new(ErrorCode::VersionResultUnreadable))?;
    if !result.compatible || result.version != IPC_VERSION {
        return Err(anyhow::Error::new(
            SysError::new(ErrorCode::IncompatibleVersion)
                .with("mine", IPC_VERSION.to_string())
                .with("theirs", result.version.to_string())
                .with("message", result.message),
        ));
    }
    Ok(result.version)
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
        .context(SysError::new(ErrorCode::FdSendTaskPanicked))?
}

/// Receive a single file descriptor from a Unix stream via SCM_RIGHTS.
/// The stream must be a dedicated raw connection (not wrapped in `Framed`).
pub async fn recv_fd(stream: &UnixStream) -> Result<std::os::unix::io::RawFd> {
    let raw = stream.as_raw_fd();
    tokio::task::spawn_blocking(move || recv_fd_sync(raw))
        .await
        .context(SysError::new(ErrorCode::FdRecvTaskPanicked))?
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
            return Err(anyhow::Error::new(SysError::new(
                ErrorCode::CmsgHeaderMissing,
            )));
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
                return Err(anyhow::Error::new(
                    SysError::new(ErrorCode::SendmsgFailed).with("e", e.to_string()),
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
                return Err(anyhow::Error::new(
                    SysError::new(ErrorCode::RecvmsgFailed).with("e", e.to_string()),
                ));
            }
            break r;
        };
        if ret == 0 {
            return Err(anyhow::Error::new(SysError::new(ErrorCode::FdpassClosed)));
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
            None => Err(anyhow::Error::new(SysError::new(ErrorCode::NoScmRightsFd))),
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

#[cfg(test)]
mod control_close_tests {
    use super::*;

    #[tokio::test]
    async fn finish_control_session_half_closes_and_drains() {
        let (client_stream, server_stream) = tokio::net::UnixStream::pair().unwrap();
        let mut client = frame_stream(client_stream);

        let server = tokio::spawn(async move {
            let mut framed = frame_stream(server_stream);
            for method in ["job.new", "unit.changed"] {
                let env = make_envelope(0, "system-a", "", method, ()).unwrap();
                send_envelope(&mut framed, &env).await.unwrap();
            }
            // The client must send FIN (EOF here) before dropping the socket,
            // so its receive queue is empty when it closes.
            let eof = recv_envelope(&mut framed).await.unwrap();
            assert!(eof.is_none(), "client must half-close before dropping");
        });

        finish_control_session(&mut client, Duration::from_millis(500)).await;
        drop(client);

        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("server task must finish")
            .expect("server task must not panic");
    }
}

#[cfg(test)]
mod version_exchange_tests {
    use super::*;

    async fn duplex() -> (EnvelopeFramed, EnvelopeFramed) {
        let (a, b) = tokio::net::UnixStream::pair().unwrap();
        (frame_stream(a), frame_stream(b))
    }

    #[tokio::test]
    async fn unexpected_reply_to_exchange_is_incompatibility() {
        let (mut client, mut server) = duplex().await;
        let server = tokio::spawn(async move {
            // A legacy server that does not know the exchange answers with
            // its own first-frame rejection and closes.
            let first = recv_envelope(&mut server).await.unwrap().unwrap();
            let error = make_envelope(1, "system-a", "", "manager.error", ()).unwrap();
            drop(first);
            send_envelope(&mut server, &error).await.unwrap();
            drop(server);
        });

        let err = exchange_version(&mut client, "tester").await.unwrap_err();
        let code = err.downcast_ref::<SysError>().map(|e| e.code());
        assert_eq!(code, Some(ErrorCode::VersionUnexpectedReply));
        drop(client);
        let _ = server.await;
    }
}
