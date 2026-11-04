use anyhow::{Context, Result};
use prost::Message;

use crate::error_code::{ErrorCode, SysError};
use crate::ipc::{frame_stream, make_envelope, recv_envelope, send_envelope};
use crate::proto::{
    CommitUnits, RegisterUnits, StagingQuery, StagingQueryResult, UnitRegistrationAck,
};

pub struct UnitFinder {
    socket_path: String,
}

impl UnitFinder {
    /// Build a finder client speaking to System A's IPC socket at an
    /// explicit path (the tests use this to point at an in-process stub).
    pub fn with_socket(socket_path: impl Into<String>) -> Self {
        UnitFinder {
            socket_path: socket_path.into(),
        }
    }

    pub async fn register_units(
        &self,
        name: &str,
        units_json: Vec<u8>,
    ) -> Result<UnitRegistrationAck> {
        let stream = tokio::net::UnixStream::connect(&self.socket_path)
            .await
            .map_err(|e| {
                anyhow::Error::new(SysError::new(ErrorCode::ConnectFailed).with("e", e.to_string()))
            })?;
        let mut framed = frame_stream(stream);

        // Pre-hello IPC version exchange: the allocator refuses peers that
        // speak a different wire version before finder semantics.
        crate::ipc::exchange_version(&mut framed, "system-f").await?;

        let reg_msg = RegisterUnits {
            name: name.to_string(),
            units_json,
        };
        let reg_env = make_envelope(1, "system-f", "system-a", "finder.register_units", reg_msg)?;
        send_envelope(&mut framed, &reg_env).await?;

        let ack_env = recv_envelope(&mut framed)
            .await?
            .ok_or_else(|| anyhow::Error::new(SysError::new(ErrorCode::AckConnectionClosed)))?;

        if ack_env.method != "finder.ack" {
            return Err(anyhow::Error::new(
                SysError::new(ErrorCode::ExpectedFinderAck).with("method", ack_env.method),
            ));
        }

        let ack = UnitRegistrationAck::decode(ack_env.payload.as_slice())
            .context(SysError::new(ErrorCode::AckDecodeFailed))?;
        Ok(ack)
    }

    /// Commit the staging area identified by the caller's UID and `name`.
    pub async fn commit_units(&self, name: &str) -> Result<UnitRegistrationAck> {
        let stream = tokio::net::UnixStream::connect(&self.socket_path)
            .await
            .map_err(|e| {
                anyhow::Error::new(SysError::new(ErrorCode::ConnectFailed).with("e", e.to_string()))
            })?;
        let mut framed = frame_stream(stream);

        // Pre-hello IPC version exchange: the allocator refuses peers that
        // speak a different wire version before finder semantics.
        crate::ipc::exchange_version(&mut framed, "system-f").await?;

        let commit_msg = CommitUnits {
            name: name.to_string(),
            uid: 0,
        };
        let commit_env =
            make_envelope(1, "system-f", "system-a", "finder.commit_units", commit_msg)?;
        send_envelope(&mut framed, &commit_env).await?;

        let ack_env = recv_envelope(&mut framed)
            .await?
            .ok_or_else(|| anyhow::Error::new(SysError::new(ErrorCode::AckConnectionClosed)))?;

        if ack_env.method != "finder.ack" {
            return Err(anyhow::Error::new(
                SysError::new(ErrorCode::ExpectedFinderAck).with("method", ack_env.method),
            ));
        }

        let ack = UnitRegistrationAck::decode(ack_env.payload.as_slice())
            .context(SysError::new(ErrorCode::AckDecodeFailed))?;
        Ok(ack)
    }

    /// Query the staging area identified by the caller's UID and `name`.
    pub async fn query_staging(&self, name: &str) -> Result<StagingQueryResult> {
        let stream = tokio::net::UnixStream::connect(&self.socket_path)
            .await
            .map_err(|e| {
                anyhow::Error::new(SysError::new(ErrorCode::ConnectFailed).with("e", e.to_string()))
            })?;
        let mut framed = frame_stream(stream);

        // Pre-hello IPC version exchange: the allocator refuses peers that
        // speak a different wire version before finder semantics.
        crate::ipc::exchange_version(&mut framed, "system-f").await?;

        let query = StagingQuery {
            name: name.to_string(),
            uid: 0,
        };
        let query_env = make_envelope(1, "system-f", "system-a", "staging.query", query)?;
        send_envelope(&mut framed, &query_env).await?;

        let result_env = recv_envelope(&mut framed)
            .await?
            .ok_or_else(|| anyhow::Error::new(SysError::new(ErrorCode::QueryConnectionClosed)))?;

        if result_env.method != "staging.query_result" {
            return Err(anyhow::Error::new(
                SysError::new(ErrorCode::ExpectedStagingQueryResult)
                    .with("method", result_env.method),
            ));
        }

        let result = StagingQueryResult::decode(result_env.payload.as_slice())
            .context(SysError::new(ErrorCode::StagingQueryDecodeFailed))?;
        Ok(result)
    }
}
