#pragma once

#include <cstdint>
#include <expected>
#include <string>
#include <string_view>
#include <systema_shared.h>
#include <utility>

namespace sysa
{

/// Coarse result category of a C API call.
enum class Status : std::uint32_t
{
    ok = SYSA_STATUS_OK,
    invalid_argument = SYSA_STATUS_INVALID_ARGUMENT,
    not_found = SYSA_STATUS_NOT_FOUND,
    io = SYSA_STATUS_IO,
    protocol = SYSA_STATUS_PROTOCOL,
    disconnected = SYSA_STATUS_DISCONNECTED,
    timeout = SYSA_STATUS_TIMEOUT,
    unsupported = SYSA_STATUS_UNSUPPORTED,
    internal = SYSA_STATUS_INTERNAL,
    reentrant = SYSA_STATUS_REENTRANT,
    already_running = SYSA_STATUS_ALREADY_RUNNING,
    not_running = SYSA_STATUS_NOT_RUNNING,
};

static_assert(static_cast<std::uint32_t>(Status::ok) == SYSA_STATUS_OK);
static_assert(static_cast<std::uint32_t>(Status::not_running) == SYSA_STATUS_NOT_RUNNING);

/// Stable IPC failure identifier.  The numeric values match the C ABI.
enum class ErrorCode : std::uint32_t
{
    none = SYSA_ERROR_CODE_NONE,
    connect_failed = SYSA_ERROR_CODE_CONNECT_FAILED,
    connect_permission_denied = SYSA_ERROR_CODE_CONNECT_PERMISSION_DENIED,
    envelope_encode_failed = SYSA_ERROR_CODE_ENVELOPE_ENCODE_FAILED,
    envelope_send_failed = SYSA_ERROR_CODE_ENVELOPE_SEND_FAILED,
    frame_receive_failed = SYSA_ERROR_CODE_FRAME_RECEIVE_FAILED,
    envelope_decode_failed = SYSA_ERROR_CODE_ENVELOPE_DECODE_FAILED,
    payload_encode_failed = SYSA_ERROR_CODE_PAYLOAD_ENCODE_FAILED,
    version_peer_closed = SYSA_ERROR_CODE_VERSION_PEER_CLOSED,
    version_unexpected_reply = SYSA_ERROR_CODE_VERSION_UNEXPECTED_REPLY,
    version_result_unreadable = SYSA_ERROR_CODE_VERSION_RESULT_UNREADABLE,
    incompatible_version = SYSA_ERROR_CODE_INCOMPATIBLE_VERSION,
    version_mismatch = SYSA_ERROR_CODE_VERSION_MISMATCH,
    fd_send_task_panicked = SYSA_ERROR_CODE_FD_SEND_TASK_PANICKED,
    fd_recv_task_panicked = SYSA_ERROR_CODE_FD_RECV_TASK_PANICKED,
    cmsg_header_missing = SYSA_ERROR_CODE_CMSG_HEADER_MISSING,
    sendmsg_failed = SYSA_ERROR_CODE_SENDMSG_FAILED,
    recvmsg_failed = SYSA_ERROR_CODE_RECVMSG_FAILED,
    fdpass_closed = SYSA_ERROR_CODE_FDPASS_CLOSED,
    no_scm_rights_fd = SYSA_ERROR_CODE_NO_SCM_RIGHTS_FD,
    ack_connection_closed = SYSA_ERROR_CODE_ACK_CONNECTION_CLOSED,
    expected_finder_ack = SYSA_ERROR_CODE_EXPECTED_FINDER_ACK,
    ack_decode_failed = SYSA_ERROR_CODE_ACK_DECODE_FAILED,
    query_connection_closed = SYSA_ERROR_CODE_QUERY_CONNECTION_CLOSED,
    expected_staging_query_result = SYSA_ERROR_CODE_EXPECTED_STAGING_QUERY_RESULT,
    staging_query_decode_failed = SYSA_ERROR_CODE_STAGING_QUERY_DECODE_FAILED,
    admin_connection_closed = SYSA_ERROR_CODE_ADMIN_CONNECTION_CLOSED,
    expected_admin_staging_result = SYSA_ERROR_CODE_EXPECTED_ADMIN_STAGING_RESULT,
    admin_staging_decode_failed = SYSA_ERROR_CODE_ADMIN_STAGING_DECODE_FAILED,
    unit_state_connection_closed = SYSA_ERROR_CODE_UNIT_STATE_CONNECTION_CLOSED,
    unit_state_entry_decode_failed = SYSA_ERROR_CODE_UNIT_STATE_ENTRY_DECODE_FAILED,
    unit_state_eof_decode_failed = SYSA_ERROR_CODE_UNIT_STATE_EOF_DECODE_FAILED,
    expected_unit_state_frame = SYSA_ERROR_CODE_EXPECTED_UNIT_STATE_FRAME,
    unit_config_decode_failed = SYSA_ERROR_CODE_UNIT_CONFIG_DECODE_FAILED,
    worker_connect_failed = SYSA_ERROR_CODE_WORKER_CONNECT_FAILED,
    connection_closed = SYSA_ERROR_CODE_CONNECTION_CLOSED,
    cgroup_query_timeout = SYSA_ERROR_CODE_CGROUP_QUERY_TIMEOUT,
    closed_before_ack = SYSA_ERROR_CODE_CLOSED_BEFORE_ACK,
    registration_rejected = SYSA_ERROR_CODE_REGISTRATION_REJECTED,
    socket_write_failed = SYSA_ERROR_CODE_SOCKET_WRITE_FAILED,
    inbound_ack_decode_failed = SYSA_ERROR_CODE_INBOUND_ACK_DECODE_FAILED,
    cgroup_reply_decode_failed = SYSA_ERROR_CODE_CGROUP_REPLY_DECODE_FAILED,
    request_id_wraparound = SYSA_ERROR_CODE_REQUEST_ID_WRAPAROUND,
    unknown_method = SYSA_ERROR_CODE_UNKNOWN_METHOD,
};

static_assert(static_cast<std::uint32_t>(ErrorCode::none) == SYSA_ERROR_CODE_NONE);
static_assert(static_cast<std::uint32_t>(ErrorCode::unknown_method) == SYSA_ERROR_CODE_UNKNOWN_METHOD);

[[nodiscard]] inline constexpr std::string_view status_name(Status status) noexcept;
[[nodiscard]] inline constexpr std::string_view error_code_name(ErrorCode code) noexcept;

/// A failure reported by the C API.
class Error
{
public:
    Error(Status status, ErrorCode code, std::string message)
        : status_{status}, code_{code}, message_{std::move(message)}
    {
    }

    [[nodiscard]] Status status() const noexcept { return status_; }
    [[nodiscard]] ErrorCode code() const noexcept { return code_; }
    [[nodiscard]] std::string_view message() const noexcept { return message_; }

    /// Human-readable diagnostic, suitable for logs but not for end users.
    [[nodiscard]] std::string to_string() const
    {
        std::string text{status_name(status_)};
        if (code_ != ErrorCode::none)
        {
            text += " [";
            text += error_code_name(code_);
            text += ']';
        }
        if (!message_.empty())
        {
            text += ": ";
            text += message_;
        }
        return text;
    }

    [[nodiscard]] bool operator==(Error const&) const = default;

private:
    Status status_;
    ErrorCode code_;
    std::string message_;
};

/// The wrapper's fallible return type.
template <typename T> using Expected = std::expected<T, Error>;

/// A fallible operation without a value.
using Result = std::expected<void, Error>;

[[nodiscard]] inline constexpr std::string_view status_name(Status status) noexcept
{
    switch (status)
    {
    case Status::ok:
        return "ok";
    case Status::invalid_argument:
        return "invalid_argument";
    case Status::not_found:
        return "not_found";
    case Status::io:
        return "io";
    case Status::protocol:
        return "protocol";
    case Status::disconnected:
        return "disconnected";
    case Status::timeout:
        return "timeout";
    case Status::unsupported:
        return "unsupported";
    case Status::internal:
        return "internal";
    case Status::reentrant:
        return "reentrant";
    case Status::already_running:
        return "already_running";
    case Status::not_running:
        return "not_running";
    }
    return "unknown";
}

[[nodiscard]] inline constexpr std::string_view error_code_name(ErrorCode code) noexcept
{
    switch (code)
    {
    case ErrorCode::none:
        return "none";
    case ErrorCode::connect_failed:
        return "connect_failed";
    case ErrorCode::connect_permission_denied:
        return "connect_permission_denied";
    case ErrorCode::envelope_encode_failed:
        return "envelope_encode_failed";
    case ErrorCode::envelope_send_failed:
        return "envelope_send_failed";
    case ErrorCode::frame_receive_failed:
        return "frame_receive_failed";
    case ErrorCode::envelope_decode_failed:
        return "envelope_decode_failed";
    case ErrorCode::payload_encode_failed:
        return "payload_encode_failed";
    case ErrorCode::version_peer_closed:
        return "version_peer_closed";
    case ErrorCode::version_unexpected_reply:
        return "version_unexpected_reply";
    case ErrorCode::version_result_unreadable:
        return "version_result_unreadable";
    case ErrorCode::incompatible_version:
        return "incompatible_version";
    case ErrorCode::version_mismatch:
        return "version_mismatch";
    case ErrorCode::fd_send_task_panicked:
        return "fd_send_task_panicked";
    case ErrorCode::fd_recv_task_panicked:
        return "fd_recv_task_panicked";
    case ErrorCode::cmsg_header_missing:
        return "cmsg_header_missing";
    case ErrorCode::sendmsg_failed:
        return "sendmsg_failed";
    case ErrorCode::recvmsg_failed:
        return "recvmsg_failed";
    case ErrorCode::fdpass_closed:
        return "fdpass_closed";
    case ErrorCode::no_scm_rights_fd:
        return "no_scm_rights_fd";
    case ErrorCode::ack_connection_closed:
        return "ack_connection_closed";
    case ErrorCode::expected_finder_ack:
        return "expected_finder_ack";
    case ErrorCode::ack_decode_failed:
        return "ack_decode_failed";
    case ErrorCode::query_connection_closed:
        return "query_connection_closed";
    case ErrorCode::expected_staging_query_result:
        return "expected_staging_query_result";
    case ErrorCode::staging_query_decode_failed:
        return "staging_query_decode_failed";
    case ErrorCode::admin_connection_closed:
        return "admin_connection_closed";
    case ErrorCode::expected_admin_staging_result:
        return "expected_admin_staging_result";
    case ErrorCode::admin_staging_decode_failed:
        return "admin_staging_decode_failed";
    case ErrorCode::unit_state_connection_closed:
        return "unit_state_connection_closed";
    case ErrorCode::unit_state_entry_decode_failed:
        return "unit_state_entry_decode_failed";
    case ErrorCode::unit_state_eof_decode_failed:
        return "unit_state_eof_decode_failed";
    case ErrorCode::expected_unit_state_frame:
        return "expected_unit_state_frame";
    case ErrorCode::unit_config_decode_failed:
        return "unit_config_decode_failed";
    case ErrorCode::worker_connect_failed:
        return "worker_connect_failed";
    case ErrorCode::connection_closed:
        return "connection_closed";
    case ErrorCode::cgroup_query_timeout:
        return "cgroup_query_timeout";
    case ErrorCode::closed_before_ack:
        return "closed_before_ack";
    case ErrorCode::registration_rejected:
        return "registration_rejected";
    case ErrorCode::socket_write_failed:
        return "socket_write_failed";
    case ErrorCode::inbound_ack_decode_failed:
        return "inbound_ack_decode_failed";
    case ErrorCode::cgroup_reply_decode_failed:
        return "cgroup_reply_decode_failed";
    case ErrorCode::request_id_wraparound:
        return "request_id_wraparound";
    case ErrorCode::unknown_method:
        return "unknown_method";
    }
    return "unknown";
}

namespace detail
{

[[nodiscard]] inline constexpr Status to_status(SysaStatus status) noexcept
{
    return static_cast<Status>(status);
}

[[nodiscard]] inline constexpr SysaStatus to_c_status(Status status) noexcept
{
    return static_cast<SysaStatus>(status);
}

/// Map a C++ error to the C callback return value.
///
/// `ok` is not a legal failure status; an error carrying it degrades to
/// `internal` instead of accidentally telling the library to continue.
[[nodiscard]] inline SysaStatus error_to_c_status(Error const& error) noexcept
{
    return error.status() == Status::ok ? SYSA_STATUS_INTERNAL : to_c_status(error.status());
}

/// Copy and clear the thread-local failure recorded by the C API.
[[nodiscard]] inline Error take_error(Status status)
{
    const auto code = static_cast<ErrorCode>(sysa_last_error_code());
    const char* raw_message = sysa_last_error_message();
    std::string message = raw_message == nullptr ? std::string{status_name(status)} : std::string{raw_message};
    sysa_clear_error();
    return Error{status, code, std::move(message)};
}

/// Copy the thread-local failure, preferring the status recorded by the C API.
[[nodiscard]] inline Error take_last_error(Status fallback)
{
    const auto raw_status = sysa_last_error_status();
    return take_error(raw_status == SYSA_STATUS_OK ? fallback : to_status(raw_status));
}

[[nodiscard]] inline Result status_to_result(SysaStatus raw_status)
{
    if (raw_status == SYSA_STATUS_OK)
    {
        sysa_clear_error();
        return {};
    }
    return std::unexpected(take_error(to_status(raw_status)));
}

[[nodiscard]] inline Error internal_error(std::string message)
{
    return Error{Status::internal, ErrorCode::none, std::move(message)};
}

inline void clear_last_error() noexcept
{
    sysa_clear_error();
}

} // namespace detail

} // namespace sysa
