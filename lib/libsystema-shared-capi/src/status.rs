//! C-visible status codes and the mirror of the IPC [`ErrorCode`] set.

use systema_shared::error_code::ErrorCode;

/// Result of every C entry point.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SystemaStatus {
    Ok = 0,
    InvalidArgument = 1,
    NotFound = 2,
    Io = 3,
    Protocol = 4,
    Disconnected = 5,
    Timeout = 6,
    Unsupported = 7,
    Internal = 8,
    Reentrant = 9,
    AlreadyRunning = 10,
    NotRunning = 11,
}

/// Stable C mirror of [`ErrorCode`].
///
/// Variants and their values never change meaning.  The mapping in
/// [`map_code`] is exhaustive on purpose: a new [`ErrorCode`] breaks this
/// crate's build until the C header is updated too.
#[repr(u32)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SystemaErrorCode {
    None = 0,
    ConnectFailed = 1,
    ConnectPermissionDenied = 2,
    EnvelopeEncodeFailed = 3,
    EnvelopeSendFailed = 4,
    FrameReceiveFailed = 5,
    EnvelopeDecodeFailed = 6,
    PayloadEncodeFailed = 7,
    VersionPeerClosed = 8,
    VersionUnexpectedReply = 9,
    VersionResultUnreadable = 10,
    IncompatibleVersion = 11,
    VersionMismatch = 12,
    FdSendTaskPanicked = 13,
    FdRecvTaskPanicked = 14,
    CmsgHeaderMissing = 15,
    SendmsgFailed = 16,
    RecvmsgFailed = 17,
    FdpassClosed = 18,
    NoScmRightsFd = 19,
    AckConnectionClosed = 20,
    ExpectedFinderAck = 21,
    AckDecodeFailed = 22,
    QueryConnectionClosed = 23,
    ExpectedStagingQueryResult = 24,
    StagingQueryDecodeFailed = 25,
    AdminConnectionClosed = 26,
    ExpectedAdminStagingResult = 27,
    AdminStagingDecodeFailed = 28,
    UnitStateConnectionClosed = 29,
    UnitStateEntryDecodeFailed = 30,
    UnitStateEofDecodeFailed = 31,
    ExpectedUnitStateFrame = 32,
    UnitConfigDecodeFailed = 33,
    WorkerConnectFailed = 34,
    ConnectionClosed = 35,
    CgroupQueryTimeout = 36,
    ClosedBeforeAck = 37,
    RegistrationRejected = 38,
    SocketWriteFailed = 39,
    InboundAckDecodeFailed = 40,
    CgroupReplyDecodeFailed = 41,
    RequestIdWraparound = 42,
    UnknownMethod = 43,
}

/// Map an IPC error code to its C code and the coarse status it belongs to.
pub(crate) fn map_code(code: ErrorCode) -> (SystemaStatus, SystemaErrorCode) {
    use SystemaErrorCode as C;
    use SystemaStatus as S;
    match code {
        ErrorCode::ConnectFailed => (S::Io, C::ConnectFailed),
        ErrorCode::ConnectPermissionDenied => (S::Io, C::ConnectPermissionDenied),
        ErrorCode::EnvelopeEncodeFailed => (S::Protocol, C::EnvelopeEncodeFailed),
        ErrorCode::EnvelopeSendFailed => (S::Protocol, C::EnvelopeSendFailed),
        ErrorCode::FrameReceiveFailed => (S::Protocol, C::FrameReceiveFailed),
        ErrorCode::EnvelopeDecodeFailed => (S::Protocol, C::EnvelopeDecodeFailed),
        ErrorCode::PayloadEncodeFailed => (S::Protocol, C::PayloadEncodeFailed),
        ErrorCode::VersionPeerClosed => (S::Disconnected, C::VersionPeerClosed),
        ErrorCode::VersionUnexpectedReply => (S::Protocol, C::VersionUnexpectedReply),
        ErrorCode::VersionResultUnreadable => (S::Protocol, C::VersionResultUnreadable),
        ErrorCode::IncompatibleVersion => (S::Protocol, C::IncompatibleVersion),
        ErrorCode::VersionMismatch => (S::Protocol, C::VersionMismatch),
        ErrorCode::FdSendTaskPanicked => (S::Internal, C::FdSendTaskPanicked),
        ErrorCode::FdRecvTaskPanicked => (S::Internal, C::FdRecvTaskPanicked),
        ErrorCode::CmsgHeaderMissing => (S::Protocol, C::CmsgHeaderMissing),
        ErrorCode::SendmsgFailed => (S::Io, C::SendmsgFailed),
        ErrorCode::RecvmsgFailed => (S::Io, C::RecvmsgFailed),
        ErrorCode::FdpassClosed => (S::Disconnected, C::FdpassClosed),
        ErrorCode::NoScmRightsFd => (S::Protocol, C::NoScmRightsFd),
        ErrorCode::AckConnectionClosed => (S::Disconnected, C::AckConnectionClosed),
        ErrorCode::ExpectedFinderAck => (S::Protocol, C::ExpectedFinderAck),
        ErrorCode::AckDecodeFailed => (S::Protocol, C::AckDecodeFailed),
        ErrorCode::QueryConnectionClosed => (S::Disconnected, C::QueryConnectionClosed),
        ErrorCode::ExpectedStagingQueryResult => (S::Protocol, C::ExpectedStagingQueryResult),
        ErrorCode::StagingQueryDecodeFailed => (S::Protocol, C::StagingQueryDecodeFailed),
        ErrorCode::AdminConnectionClosed => (S::Disconnected, C::AdminConnectionClosed),
        ErrorCode::ExpectedAdminStagingResult => (S::Protocol, C::ExpectedAdminStagingResult),
        ErrorCode::AdminStagingDecodeFailed => (S::Protocol, C::AdminStagingDecodeFailed),
        ErrorCode::UnitStateConnectionClosed => (S::Disconnected, C::UnitStateConnectionClosed),
        ErrorCode::UnitStateEntryDecodeFailed => (S::Protocol, C::UnitStateEntryDecodeFailed),
        ErrorCode::UnitStateEofDecodeFailed => (S::Protocol, C::UnitStateEofDecodeFailed),
        ErrorCode::ExpectedUnitStateFrame => (S::Protocol, C::ExpectedUnitStateFrame),
        ErrorCode::UnitConfigDecodeFailed => (S::Protocol, C::UnitConfigDecodeFailed),
        ErrorCode::WorkerConnectFailed => (S::Io, C::WorkerConnectFailed),
        ErrorCode::ConnectionClosed => (S::Disconnected, C::ConnectionClosed),
        ErrorCode::CgroupQueryTimeout => (S::Timeout, C::CgroupQueryTimeout),
        ErrorCode::ClosedBeforeAck => (S::Disconnected, C::ClosedBeforeAck),
        ErrorCode::RegistrationRejected => (S::Protocol, C::RegistrationRejected),
        ErrorCode::SocketWriteFailed => (S::Io, C::SocketWriteFailed),
        ErrorCode::InboundAckDecodeFailed => (S::Protocol, C::InboundAckDecodeFailed),
        ErrorCode::CgroupReplyDecodeFailed => (S::Protocol, C::CgroupReplyDecodeFailed),
        ErrorCode::RequestIdWraparound => (S::Internal, C::RequestIdWraparound),
        ErrorCode::UnknownMethod => (S::Unsupported, C::UnknownMethod),
    }
}
