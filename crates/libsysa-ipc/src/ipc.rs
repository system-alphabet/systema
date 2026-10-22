use std::os::unix::io::AsRawFd;

use anyhow::{Context, Result};
use bytes::{Bytes, BytesMut};
use futures::{SinkExt, StreamExt};
use prost::Message;
use tokio::net::UnixStream;
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use tracing::{debug, warn};

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
            let template = if e.kind() == std::io::ErrorKind::PermissionDenied {
                crate::l10n::t_(
                    "Failed to connect to System A: {e}. System-domain operations need root: re-run with sudo, or pass --user to manage the per-user instance.",
                )
            } else {
                crate::l10n::t_("Failed to connect to System A: {e}")
            };
            Err(anyhow::anyhow!(crate::l10n::fmt(
                template,
                &[("e", &e.to_string())]
            )))
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

    let reply = recv_envelope(framed).await?.ok_or_else(|| {
        anyhow::anyhow!(crate::l10n::t_(
            "peer closed the connection during the IPC version exchange"
        ))
    })?;
    if reply.method != METHOD_VERSION_RESULT {
        anyhow::bail!(crate::l10n::fmt(
            crate::l10n::t_("unexpected reply '{method}' during the IPC version exchange"),
            &[("method", &reply.method)]
        ));
    }
    let result = IpcVersionResult::decode(reply.payload.as_slice())
        .context(crate::l10n::t_("unreadable ipc.version.result payload"))?;
    if !result.compatible || result.version != IPC_VERSION {
        anyhow::bail!(crate::l10n::fmt(
            crate::l10n::t_(
                "incompatible IPC version: we speak {mine}, the peer announced {theirs}: {message}"
            ),
            &[
                ("mine", &IPC_VERSION.to_string()),
                ("theirs", &result.version.to_string()),
                ("message", &result.message)
            ]
        ));
    }
    Ok(result.version)
}

/// Decode the leading version-exchange frame on the server side, when the
/// session's first envelope is one.  `None` — this peer skipped the
/// exchange (legacy): the caller proceeds with its existing first-frame
/// semantics unchanged.
pub fn version_request_from(env: &Envelope) -> Option<IpcVersionRequest> {
    if env.method != METHOD_VERSION {
        return None;
    }
    IpcVersionRequest::decode(env.payload.as_slice()).ok()
}

/// Whether the server accepts the client's announced version (for now the
/// allocator pins [`IPC_VERSION`]; only an equal version passes).
pub fn version_verdict(request: &IpcVersionRequest) -> bool {
    request.version == IPC_VERSION
}

/// Build the server's `ipc.version.result`.  The server always reports its
/// own version in the reply; `compatible == false` adds the l10n rejection
/// text and the caller closes the connection after sending.
pub fn version_result_envelope(request_id: u64, request: &IpcVersionRequest) -> Result<Envelope> {
    let compatible = version_verdict(request);
    let message = if compatible {
        String::new()
    } else {
        crate::l10n::fmt(
            crate::l10n::t_("this build pins IPC version {have}; the peer announced {want}"),
            &[
                ("have", &IPC_VERSION.to_string()),
                ("want", &request.version.to_string()),
            ],
        )
    };
    make_envelope(
        request_id,
        "system-a",
        request.client.as_str(),
        METHOD_VERSION_RESULT,
        IpcVersionResult {
            version: IPC_VERSION,
            compatible,
            message,
        },
    )
}

/// Server-side pre-hello gate for a framed full-duplex session (worker
/// plane).  When the first envelope is a version exchange, the verdict is
/// written and — if refused — `Ok(None)` is returned for the caller to
/// drop the connection; otherwise the actual first session frame is read
/// and returned.  Legacy first envelopes (no exchange) pass through
/// untouched, so old peers keep working over the same accept path.
pub async fn gate_session_first(
    framed: &mut EnvelopeFramed,
    first: Envelope,
) -> Result<Option<Envelope>> {
    let Some(request) = version_request_from(&first) else {
        debug!(
            "peer '{}' skipped the IPC version exchange (legacy)",
            first.source
        );
        return Ok(Some(first));
    };
    let reply = version_result_envelope(first.request_id, &request)?;
    if send_envelope(framed, &reply).await.is_err() {
        return Ok(None);
    }
    if !version_verdict(&request) {
        warn!(
            "IPC version {} announced by '{}' refused (this build pins {}); closing",
            request.version, request.client, IPC_VERSION
        );
        return Ok(None);
    }
    debug!(
        "IPC version {} negotiated with '{}'",
        request.version, request.client
    );
    Ok(recv_envelope(framed).await?)
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

#[cfg(test)]
mod version_exchange_tests {
    use super::*;
    use crate::proto::ManagerHelloRequest;

    async fn duplex() -> (EnvelopeFramed, EnvelopeFramed) {
        let (a, b) = tokio::net::UnixStream::pair().unwrap();
        (frame_stream(a), frame_stream(b))
    }

    #[tokio::test]
    async fn incompatible_announcement_refuses_and_closes() {
        let (mut client, mut server) = duplex().await;

        let request = make_envelope(
            7,
            "rogue",
            "system-a",
            METHOD_VERSION,
            IpcVersionRequest {
                version: 99,
                client: "rogue".to_string(),
            },
        )
        .unwrap();
        send_envelope(&mut client, &request).await.unwrap();

        let first = recv_envelope(&mut server).await.unwrap().unwrap();
        // Refused: the gate says close (None) after the verdict went out.
        assert!(gate_session_first(&mut server, first)
            .await
            .unwrap()
            .is_none());
        let verdict = recv_envelope(&mut client).await.unwrap().unwrap();
        assert_eq!(verdict.method, METHOD_VERSION_RESULT);
        let result = IpcVersionResult::decode(verdict.payload.as_slice()).unwrap();
        assert!(!result.compatible);
        assert_eq!(result.version, IPC_VERSION);
        assert!(!result.message.is_empty());
    }

    #[tokio::test]
    async fn compatible_exchange_returns_then_the_real_first_frame() {
        let (mut client, mut server) = duplex().await;
        let server = tokio::spawn(async move {
            let first = recv_envelope(&mut server).await.unwrap().unwrap();
            let continuation = gate_session_first(&mut server, first)
                .await
                .unwrap()
                .unwrap();
            (continuation, server)
        });

        let negotiated = exchange_version(&mut client, "tester").await.unwrap();
        assert_eq!(negotiated, IPC_VERSION);

        // Client deems itself compatible and sends its hello; the server
        // gate hands the hello through as the session's first envelope
        // ("communication established" semantics stay with hello).
        let hello = make_envelope(
            1,
            "tester",
            "system-a",
            "manager.hello",
            ManagerHelloRequest {
                flavor: "tester".to_string(),
                version: "0.0.0".to_string(),
                worker_id: String::new(),
                ..Default::default()
            },
        )
        .unwrap();
        send_envelope(&mut client, &hello).await.unwrap();

        let (continuation, server) = server.await.unwrap();
        assert_eq!(continuation.method, "manager.hello");
        assert_eq!(continuation.request_id, 1);
        drop(server);
    }

    #[tokio::test]
    async fn legacy_first_frame_passes_through_untouched() {
        let (_client, mut server) = duplex().await;
        let legacy = Envelope {
            request_id: 3,
            source: "legacy".to_string(),
            target: "system-a".to_string(),
            method: "worker.register".to_string(),
            payload: Vec::new(),
        };
        let gated = gate_session_first(&mut server, legacy)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(gated.method, "worker.register");
        assert_eq!(gated.request_id, 3);
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
        assert!(err.to_string().contains("exchange"));
        drop(client);
        let _ = server.await;
    }
}
