use anyhow::{Context, Result};
use prost::Message;

use crate::ipc::{frame_stream, make_envelope, recv_envelope, send_envelope};
use crate::proto::{UnitStateEntry, UnitStateEof, UnitStateListRequest};

/// Result of streaming the System Allocator's cached unit state.
#[derive(Debug, Clone)]
pub struct UnitStateResult {
    /// Number of unit entries streamed (0 on failure).
    pub total: u32,
    /// Human-readable message (empty on success).
    pub message: String,
}

/// Client for `admin.unitstate`: streams a JSON snapshot of every unit's
/// cached state straight from the System Allocator (no worker is involved).
///
/// The admin methods are served on the control port (`control.socket`), not
/// the allocator IPC socket.  Requires the caller to be UID 0 (root) or the
/// UID under which System Allocator is running; otherwise the server responds
/// with a permission-denied EOF.
pub struct UnitStateAdmin {
    socket_path: String,
}

impl UnitStateAdmin {
    pub fn new() -> Self {
        UnitStateAdmin {
            socket_path: crate::paths::instance().control_socket_path.to_string(),
        }
    }

    /// Stream every cached unit, invoking `on_unit` with each entry's name
    /// and JSON payload as it arrives.  Returns after the EOF sentinel.
    pub async fn list<F>(&self, mut on_unit: F) -> Result<UnitStateResult>
    where
        F: FnMut(&str, &[u8]) -> Result<()>,
    {
        let stream = tokio::net::UnixStream::connect(&self.socket_path)
            .await
            .map_err(|e| {
                anyhow::anyhow!(crate::l10n::fmt(
                    crate::l10n::t_("Failed to connect to System A: {e}"),
                    &[("e", &e.to_string())]
                ))
            })?;
        let mut framed = frame_stream(stream);

        let query_env = make_envelope(
            1,
            "",
            "system-a",
            "admin.unitstate",
            UnitStateListRequest {},
        )?;
        send_envelope(&mut framed, &query_env).await?;

        loop {
            let env = recv_envelope(&mut framed).await?.ok_or_else(|| {
                anyhow::anyhow!(crate::l10n::t_(
                    "System A disconnected before unit-state EOF."
                ))
            })?;

            match env.method.as_str() {
                "admin.unitstate.entry" => {
                    let entry = UnitStateEntry::decode(env.payload.as_slice())
                        .context(crate::l10n::t_("Failed to decode UnitStateEntry"))?;
                    if !entry.json.is_empty() {
                        on_unit(&entry.name, &entry.json)?;
                    }
                }
                "admin.unitstate.eof" => {
                    let eof = UnitStateEof::decode(env.payload.as_slice())
                        .context(crate::l10n::t_("Failed to decode UnitStateEof"))?;
                    return Ok(UnitStateResult {
                        total: eof.total,
                        message: eof.message,
                    });
                }
                other => {
                    anyhow::bail!(crate::l10n::fmt(
                        crate::l10n::t_(
                            "Expected 'admin.unitstate.entry'/'admin.unitstate.eof', got '{other}'"
                        ),
                        &[("other", &other.to_string())]
                    ));
                }
            }
        }
    }
}

impl Default for UnitStateAdmin {
    fn default() -> Self {
        Self::new()
    }
}
