//! Error codes for the IPC kernel.
//!
//! `sysa-ipc` reports failures as [`SysError`] values: an [`ErrorCode`]
//! plus the values of the code's `{name}` placeholders.  The kernel carries
//! **no message text at all** — not even an English fallback: the wording
//! of every failure lives in `sysa::errors`, which maps a code to its
//! localized message.  That keeps this crate free of gettext, of the po
//! msgid table, and of any user-facing string.
//!
//! [`SysError`]'s `Display` is a technical rendering (`Code(arg=value)`)
//! meant for tracing logs and protocol fields, never for end users.

use std::fmt;

/// A stable identifier for one IPC-kernel failure.
///
/// The set is public API: callers may match on a code to give a failure
/// their own handling, so variants keep their meaning across releases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    // -- ipc: connection and framing -----------------------------------
    /// `connect_control` failed and the caller lacks the privilege.
    ConnectFailed,
    /// `connect_control` failed with `PermissionDenied`; carries the
    /// cross-domain hint (sudo / `--user`) the server appends.
    ConnectPermissionDenied,
    /// An `Envelope` could not be protobuf-encoded.
    EnvelopeEncodeFailed,
    /// An encoded `Envelope` could not be written to the framed stream.
    EnvelopeSendFailed,
    /// Reading the next frame off the stream failed.
    FrameReceiveFailed,
    /// A received frame was not a valid `Envelope`.
    EnvelopeDecodeFailed,
    /// A method payload could not be protobuf-encoded.
    PayloadEncodeFailed,

    // -- ipc: protocol version exchange --------------------------------
    /// The peer closed the connection before answering `ipc.version`.
    VersionPeerClosed,
    /// The first reply was not `ipc.version.result`.
    VersionUnexpectedReply,
    /// The `ipc.version.result` payload was not decodable.
    VersionResultUnreadable,
    /// This build cannot speak the version the peer announced.
    IncompatibleVersion,
    /// Server-side verdict: the pinned version and the request differ.
    VersionMismatch,

    // -- ipc: SCM_RIGHTS file-descriptor passing -----------------------
    /// The task that hands a listener fd to the peer panicked.
    FdSendTaskPanicked,
    /// The task that receives a listener fd from the peer panicked.
    FdRecvTaskPanicked,
    /// `CMSG_FIRSTHDR` returned null on a received message.
    CmsgHeaderMissing,
    /// `sendmsg` with `SCM_RIGHTS` failed.
    SendmsgFailed,
    /// `recvmsg` with `SCM_RIGHTS` failed.
    RecvmsgFailed,
    /// The fd-passing connection closed while a listener fd was awaited.
    FdpassClosed,
    /// A received control message carried no `SCM_RIGHTS` descriptor.
    NoScmRightsFd,

    // -- finder: registration, commit and staging queries --------------
    /// System A disconnected before answering a registration or commit.
    AckConnectionClosed,
    /// The reply to a registration or commit was not `finder.ack`.
    ExpectedFinderAck,
    /// The `UnitRegistrationAck` payload was not decodable.
    AckDecodeFailed,
    /// System A disconnected before answering a staging query.
    QueryConnectionClosed,
    /// The reply to a staging query was not `staging.query_result`.
    ExpectedStagingQueryResult,
    /// The `StagingQueryResult` payload was not decodable.
    StagingQueryDecodeFailed,

    // -- staging_admin: `admin.staging` --------------------------------
    /// System A disconnected before answering an `admin.staging` request.
    AdminConnectionClosed,
    /// The reply was not `admin.staging.result`.
    ExpectedAdminStagingResult,
    /// The `AdminStagingResult` payload was not decodable.
    AdminStagingDecodeFailed,

    // -- unitstate_admin: `admin.unitstate` ----------------------------
    /// System A disconnected before the unit-state EOF sentinel.
    UnitStateConnectionClosed,
    /// A streamed `admin.unitstate.entry` payload was not decodable.
    UnitStateEntryDecodeFailed,
    /// The `admin.unitstate.eof` payload was not decodable.
    UnitStateEofDecodeFailed,
    /// A streamed frame was neither a unit-state entry nor the EOF sentinel.
    ExpectedUnitStateFrame,

    // -- controller: wire form of a unit definition --------------------
    /// A `UnitConfig` payload delivered to a worker was not decodable.
    UnitConfigDecodeFailed,

    // -- worker_ipc: worker session lifecycle --------------------------
    /// The worker could not connect to System A at the requested path.
    WorkerConnectFailed,
    /// The session to System A is gone.
    ConnectionClosed,
    /// A `unit.cgroup` query did not answer within the caller's budget.
    CgroupQueryTimeout,
    /// System A closed the connection before the registration ack.
    ClosedBeforeAck,
    /// System A refused the worker registration.
    RegistrationRejected,
    /// Writing to the System A socket failed.
    SocketWriteFailed,
    /// An inbound ack payload (worker registration or state-update ack) was
    /// not decodable.
    InboundAckDecodeFailed,
    /// A `unit.cgroup` reply payload was not decodable.
    CgroupReplyDecodeFailed,
    /// The request-id counter wrapped around.
    RequestIdWraparound,
    /// A dispatched method name is not implemented by this worker.
    UnknownMethod,
}

impl ErrorCode {
    /// Every variant, so callers (and tests) can enumerate the code set
    /// without a wildcard match.
    pub const ALL: &'static [ErrorCode] = &[
        ErrorCode::ConnectFailed,
        ErrorCode::ConnectPermissionDenied,
        ErrorCode::EnvelopeEncodeFailed,
        ErrorCode::EnvelopeSendFailed,
        ErrorCode::FrameReceiveFailed,
        ErrorCode::EnvelopeDecodeFailed,
        ErrorCode::PayloadEncodeFailed,
        ErrorCode::VersionPeerClosed,
        ErrorCode::VersionUnexpectedReply,
        ErrorCode::VersionResultUnreadable,
        ErrorCode::IncompatibleVersion,
        ErrorCode::VersionMismatch,
        ErrorCode::FdSendTaskPanicked,
        ErrorCode::FdRecvTaskPanicked,
        ErrorCode::CmsgHeaderMissing,
        ErrorCode::SendmsgFailed,
        ErrorCode::RecvmsgFailed,
        ErrorCode::FdpassClosed,
        ErrorCode::NoScmRightsFd,
        ErrorCode::AckConnectionClosed,
        ErrorCode::ExpectedFinderAck,
        ErrorCode::AckDecodeFailed,
        ErrorCode::QueryConnectionClosed,
        ErrorCode::ExpectedStagingQueryResult,
        ErrorCode::StagingQueryDecodeFailed,
        ErrorCode::AdminConnectionClosed,
        ErrorCode::ExpectedAdminStagingResult,
        ErrorCode::AdminStagingDecodeFailed,
        ErrorCode::UnitStateConnectionClosed,
        ErrorCode::UnitStateEntryDecodeFailed,
        ErrorCode::UnitStateEofDecodeFailed,
        ErrorCode::ExpectedUnitStateFrame,
        ErrorCode::UnitConfigDecodeFailed,
        ErrorCode::WorkerConnectFailed,
        ErrorCode::ConnectionClosed,
        ErrorCode::CgroupQueryTimeout,
        ErrorCode::ClosedBeforeAck,
        ErrorCode::RegistrationRejected,
        ErrorCode::SocketWriteFailed,
        ErrorCode::InboundAckDecodeFailed,
        ErrorCode::CgroupReplyDecodeFailed,
        ErrorCode::RequestIdWraparound,
        ErrorCode::UnknownMethod,
    ];

    /// Placeholder names this code carries, in message order.
    ///
    /// The message itself is in `sysa::errors`; both sides are kept in
    /// sync by a test there.
    pub const fn params(self) -> &'static [&'static str] {
        match self {
            ErrorCode::ConnectFailed => &["e"],
            ErrorCode::ConnectPermissionDenied => &["e"],
            ErrorCode::VersionUnexpectedReply => &["method"],
            ErrorCode::IncompatibleVersion => &["mine", "theirs", "message"],
            ErrorCode::VersionMismatch => &["have", "want"],
            ErrorCode::SendmsgFailed => &["e"],
            ErrorCode::RecvmsgFailed => &["e"],
            ErrorCode::ExpectedFinderAck => &["method"],
            ErrorCode::ExpectedStagingQueryResult => &["method"],
            ErrorCode::ExpectedAdminStagingResult => &["method"],
            ErrorCode::ExpectedUnitStateFrame => &["other"],
            ErrorCode::WorkerConnectFailed => &["path"],
            ErrorCode::CgroupQueryTimeout => &["timeout"],
            ErrorCode::RegistrationRejected => &["message"],
            ErrorCode::UnknownMethod => &["other"],
            _ => &[],
        }
    }
}

/// A failure reported by the IPC kernel: one [`ErrorCode`] plus the values
/// of its placeholders.
///
/// [`Display`](fmt::Display) renders the English template with the values
/// substituted, so logs and `anyhow` chains stay readable without any
/// localization set up.  To show the user a translated message, pass the
/// error to `sysa::errors::localize`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SysError {
    code: ErrorCode,
    args: Vec<(&'static str, String)>,
}

impl SysError {
    /// A code with no placeholder values.
    pub fn new(code: ErrorCode) -> Self {
        SysError {
            code,
            args: Vec::new(),
        }
    }

    /// Attach one placeholder value.
    ///
    /// The name must be one of [`ErrorCode::params`]; a wrong name leaves
    /// its placeholder unfilled and is flagged by the debug assertion.
    pub fn with(mut self, name: &'static str, value: impl Into<String>) -> Self {
        debug_assert!(
            self.code.params().contains(&name),
            "ErrorCode::{:?} has no '{name}' placeholder",
            self.code
        );
        self.args.push((name, value.into()));
        self
    }

    /// The failure code, for callers that branch on it.
    pub fn code(&self) -> ErrorCode {
        self.code
    }

    /// The placeholder values, in the order they were attached.
    pub fn args(&self) -> &[(&'static str, String)] {
        &self.args
    }
}

impl From<ErrorCode> for SysError {
    fn from(code: ErrorCode) -> Self {
        SysError::new(code)
    }
}

impl fmt::Display for SysError {
    /// Technical rendering for logs and protocol fields:
    /// `Code` alone, or `Code(arg=value, ...)` when values are attached.
    ///
    /// Deliberately contains no wording: the user-facing message is
    /// produced by `sysa::errors` from the code.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.code)?;
        if self.args.is_empty() {
            return Ok(());
        }
        f.write_str("(")?;
        for (index, (name, value)) in self.args.iter().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{name}={value}")?;
        }
        f.write_str(")")
    }
}

impl std::error::Error for SysError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_renders_the_code_and_its_values() {
        let err = SysError::new(ErrorCode::WorkerConnectFailed).with("path", "/run/x.sock");
        assert_eq!(err.to_string(), "WorkerConnectFailed(path=/run/x.sock)");
    }

    #[test]
    fn display_renders_the_bare_code_without_values() {
        let err = SysError::new(ErrorCode::ConnectFailed);
        assert_eq!(err.to_string(), "ConnectFailed");
    }

    #[test]
    fn display_separates_multiple_values() {
        let err = SysError::new(ErrorCode::VersionMismatch)
            .with("have", "1")
            .with("want", "2");
        assert_eq!(err.to_string(), "VersionMismatch(have=1, want=2)");
    }

    #[test]
    fn codes_round_trip_through_sys_error() {
        let err: SysError = ErrorCode::ConnectionClosed.into();
        assert_eq!(err.code(), ErrorCode::ConnectionClosed);
        assert!(err.args().is_empty());
    }
}
