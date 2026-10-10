use anyhow::{Context, Result};
use prost::Message;

use crate::ipc::{frame_stream, make_envelope, recv_envelope, send_envelope};
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
    pub fn new() -> Self {
        StagingAdmin {
            socket_path: crate::paths::instance().control_socket_path.to_string(),
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
        let stream = tokio::net::UnixStream::connect(&self.socket_path)
            .await
            .map_err(|e| {
                anyhow::anyhow!(crate::l10n::fmt(
                    crate::l10n::t_("Failed to connect to System A: {e}"),
                    &[("e", &e.to_string())]
                ))
            })?;
        let mut framed = frame_stream(stream);

        let query = AdminStagingOp {
            op: op.to_string(),
            uid,
            name: name.to_string(),
        };
        let query_env = make_envelope(1, "", "system-a", "admin.staging", query)?;
        send_envelope(&mut framed, &query_env).await?;

        let result_env = recv_envelope(&mut framed).await?.ok_or_else(|| {
            anyhow::anyhow!(crate::l10n::t_(
                "System A disconnected before admin result."
            ))
        })?;

        if result_env.method != "admin.staging.result" {
            anyhow::bail!(crate::l10n::fmt(
                crate::l10n::t_("Expected 'admin.staging.result', got '{method}'"),
                &[("method", &(result_env.method).to_string())]
            ));
        }

        let result = AdminStagingResult::decode(result_env.payload.as_slice())
            .context(crate::l10n::t_("Failed to decode AdminStagingResult"))?;
        Ok(result)
    }
}

impl Default for StagingAdmin {
    fn default() -> Self {
        Self::new()
    }
}
