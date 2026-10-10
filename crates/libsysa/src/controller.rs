use std::collections::HashMap;

use anyhow::Context;

/// Decode a `UnitConfig` from its protobuf-encoded bytes.
pub fn decode_unit_config(bytes: &[u8]) -> anyhow::Result<crate::proto::UnitConfig> {
    use prost::Message;
    crate::proto::UnitConfig::decode(bytes).context(crate::l10n::t_("failed to decode UnitConfig"))
}

/// Standardised runtime status of a unit, returned by `status()`.
#[derive(Debug, Clone, Default)]
pub struct UnitStatus {
    pub unit_name: String,
    pub active_state: String,
    pub sub_state: String,
    pub main_pid: u32,
    pub invocation_id: String,
    pub extensions: HashMap<String, String>,
}

impl UnitStatus {
    /// Build a UnitStatus from a protobuf `UnitStatus` message.
    pub fn from_proto(proto: crate::proto::UnitStatus) -> Self {
        UnitStatus {
            unit_name: proto.unit_name,
            active_state: proto.active_state,
            sub_state: proto.sub_state,
            main_pid: proto.main_pid,
            invocation_id: proto.invocation_id,
            extensions: proto.extensions,
        }
    }

    /// Convert into a protobuf `UnitStatus` message.
    pub fn into_proto(self) -> crate::proto::UnitStatus {
        crate::proto::UnitStatus {
            unit_name: self.unit_name,
            active_state: self.active_state,
            sub_state: self.sub_state,
            main_pid: self.main_pid,
            invocation_id: self.invocation_id,
            extensions: self.extensions,
        }
    }

    /// Serialize to protobuf bytes.
    pub fn encode_to_vec(&self) -> Vec<u8> {
        use prost::Message;
        let proto = self.clone().into_proto();
        let mut buf = bytes::BytesMut::new();
        if proto.encode(&mut buf).is_ok() {
            buf.to_vec()
        } else {
            Vec::new()
        }
    }

    /// Deserialize from protobuf bytes.
    pub fn decode_from(bytes: &[u8]) -> Option<Self> {
        use prost::Message;
        let proto = crate::proto::UnitStatus::decode(bytes).ok()?;
        Some(Self::from_proto(proto))
    }
}

/// Trait that every unit worker implements.
///
/// System A and its D-Bus layer call these methods generically — they have
/// no knowledge of concrete unit types.  The IPC layer routes each call to
/// the worker that manages the target unit.
#[async_trait::async_trait]
pub trait UnitController: Send + Sync {
    /// Query the current runtime state of a unit.
    async fn status(&self, unit_name: &str) -> anyhow::Result<UnitStatus>;

    /// Start a unit with the given serialised configuration.
    ///
    /// `invocation_id` is a UUID v4 string that identifies this activation.
    /// Workers set `INVOCATION_ID` on the child process when applicable.
    async fn start(
        &self,
        unit_name: &str,
        config: &[u8],
        invocation_id: &str,
    ) -> anyhow::Result<()>;

    /// Stop a unit.
    async fn stop(&self, unit_name: &str) -> anyhow::Result<()>;

    /// Restart (stop then start) a unit.
    async fn restart(
        &self,
        unit_name: &str,
        config: &[u8],
        invocation_id: &str,
    ) -> anyhow::Result<()>;

    /// Reload a unit's configuration without restarting.
    async fn reload(&self, unit_name: &str, config: &[u8]) -> anyhow::Result<()>;

    /// Return the runtime state of all managed units for state synchronization.
    ///
    /// The default implementation returns an empty list.
    async fn sync_state(&self) -> Vec<UnitStatus> {
        Vec::new()
    }
}
