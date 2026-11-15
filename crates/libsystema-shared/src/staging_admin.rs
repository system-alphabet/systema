use anyhow::{Context, Result};
use prost::Message;

use crate::error_code::{ErrorCode, SysError};
use crate::ipc::{connect_control, frame_stream, make_envelope, recv_envelope, send_envelope};
use crate::proto::{AdminStagingOp, AdminStagingResult};

/// Administration tool for querying staging areas.
///
/// All operations require the caller to be UID 0 (root) or the UID under
/// which System Allocator is running; otherwise the server returns a
/// permission-denied error.
pub struct StagingAdmin {
    socket_path: String,
}

impl StagingAdmin {
    /// Build a staging-admin client speaking to System A's control socket
    /// at an explicit path.
    pub fn with_socket(socket_path: impl Into<String>) -> Self {
        StagingAdmin {
            socket_path: socket_path.into(),
        }
    }

    /// List every staging area as (UID, name) pairs.
    pub async fn list(&self) -> Result<Vec<(u32, String)>> {
        let result = self.send_op("list", 0, "").await?;
        if !result.success {
            anyhow::bail!("{}", result.message);
        }
        Ok(result
            .entries
            .into_iter()
            .map(|e| (e.uid, e.name))
            .collect())
    }

    /// Query every staging area owned by a UID (returns raw JSON bytes).
    pub async fn query_by_uid(&self, uid: u32) -> Result<AdminStagingResult> {
        self.send_op("by_uid", uid, "").await
    }

    /// Query every staging area with the given name (returns raw JSON bytes).
    pub async fn query_by_name(&self, name: &str) -> Result<AdminStagingResult> {
        self.send_op("by_name", 0, name).await
    }

    /// Query the staging area identified by the (uid, name) pair.
    pub async fn query(&self, uid: u32, name: &str) -> Result<AdminStagingResult> {
        self.send_op("by_id", uid, name).await
    }

    /// Query all staging areas with full data.
    pub async fn query_all(&self) -> Result<AdminStagingResult> {
        self.send_op("all", 0, "").await
    }

    /// Commit a staging area into the active unit set.
    ///
    /// With a non-empty `name` only the `(uid, name)` area is merged and
    /// consumed; with an empty `name` every area owned by `uid` is committed.
    pub async fn commit(&self, uid: u32, name: &str) -> Result<AdminStagingResult> {
        self.send_op("commit", uid, name).await
    }

    async fn send_op(&self, op: &str, uid: u32, name: &str) -> Result<AdminStagingResult> {
        let stream = connect_control(&self.socket_path).await?;
        let mut framed = frame_stream(stream);

        // Pre-hello IPC version exchange: the allocator refuses peers on a
        // different wire version before any admin semantics run.
        crate::ipc::exchange_version(&mut framed, "staging-admin").await?;

        let query = AdminStagingOp {
            op: op.to_string(),
            uid,
            name: name.to_string(),
        };
        let query_env = make_envelope(1, "", "system-a", "admin.staging", query)?;
        send_envelope(&mut framed, &query_env).await?;

        let result_env = recv_envelope(&mut framed)
            .await?
            .ok_or_else(|| anyhow::Error::new(SysError::new(ErrorCode::AdminConnectionClosed)))?;

        if result_env.method != "admin.staging.result" {
            return Err(anyhow::Error::new(
                SysError::new(ErrorCode::ExpectedAdminStagingResult)
                    .with("method", result_env.method),
            ));
        }

        let result = AdminStagingResult::decode(result_env.payload.as_slice())
            .context(SysError::new(ErrorCode::AdminStagingDecodeFailed))?;
        Ok(result)
    }
}
