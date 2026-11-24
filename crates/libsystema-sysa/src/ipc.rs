//! Server-side half of the IPC version handshake.
//!
//! The wire contract and every client-side primitive live in
//! [`systema_shared::ipc`]; this module re-exports them unchanged and adds
//! the functions only System A's accept paths call: the version verdict and
//! the pre-hello session gate.  `sysa::ipc::*` therefore keeps exposing the
//! whole pre-split surface.

pub use systema_shared::ipc::*;

use anyhow::Result;
use prost::Message;
use tracing::{debug, warn};

use systema_shared::error_code::{ErrorCode, SysError};
use systema_shared::proto::{Envelope, IpcVersionRequest, IpcVersionResult};

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
/// own version in the reply; `compatible == false` adds the technical
/// rejection detail (`VersionMismatch(have=…, want=…)`) and the caller
/// closes the connection after sending.
pub fn version_result_envelope(request_id: u64, request: &IpcVersionRequest) -> Result<Envelope> {
    let compatible = version_verdict(request);
    let message = if compatible {
        String::new()
    } else {
        // Wire data: the peer renders its own user-facing text from the
        // code, so this field carries only the technical parameters.
        SysError::new(ErrorCode::VersionMismatch)
            .with("have", IPC_VERSION.to_string())
            .with("want", request.version.to_string())
            .to_string()
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

#[cfg(test)]
mod tests {
    use super::*;
    use systema_shared::proto::ManagerHelloRequest;

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
}
