#pragma once

#include "sysa/common.hpp"
#include "sysa/concepts.hpp"
#include "sysa/detail.hpp"
#include "sysa/error.hpp"

#include <cstddef>
#include <cstdint>
#include <string_view>
#include <systema_shared.h>
#include <utility>

namespace sysa
{

/// One control-port event pushed by System A.
enum class ControlEventKind : std::uint32_t
{
    unit_changed = SYSA_CONTROL_EVENT_KIND_UNIT_CHANGED,
    unit_new = SYSA_CONTROL_EVENT_KIND_UNIT_NEW,
    unit_removed = SYSA_CONTROL_EVENT_KIND_UNIT_REMOVED,
    unit_metrics = SYSA_CONTROL_EVENT_KIND_UNIT_METRICS,
    job_new = SYSA_CONTROL_EVENT_KIND_JOB_NEW,
    job_completed = SYSA_CONTROL_EVENT_KIND_JOB_COMPLETED,
};

/// One server-pushed event: the classified kind plus its protobuf payload.
struct ControlEvent
{
    ControlEventKind kind = ControlEventKind::unit_changed;
    Bytes payload;
};

/// Control-port client for `manager.*` on `control.socket`.
///
/// The client performs the version + `manager.hello` handshake in `create`.
/// Requests block the calling thread; events are pulled with `next_event`, so
/// no callback is ever invoked from a library thread.
class ControlClient
{
public:
    ControlClient(ControlClient&& other) noexcept : handle_{std::exchange(other.handle_, nullptr)} {}

    ControlClient& operator=(ControlClient&& other) noexcept
    {
        if (this != &other)
        {
            reset();
            handle_ = std::exchange(other.handle_, nullptr);
        }
        return *this;
    }

    ControlClient(ControlClient const&) = delete;
    ControlClient& operator=(ControlClient const&) = delete;

    ~ControlClient() { reset(); }

    /// Connect to `socket_path` and declare `flavor` as the bridge identity.
    template <StringLike SocketPath, StringLike Flavor>
    [[nodiscard]] static Expected<ControlClient> create(SocketPath const& socket_path, Flavor const& flavor)
    {
        return create_impl(socket_path, flavor, nullptr);
    }

    /// Connect as a supervised bridge identified by `worker_id`.
    template <StringLike SocketPath, StringLike Flavor, StringLike WorkerId>
    [[nodiscard]] static Expected<ControlClient> create(SocketPath const& socket_path, Flavor const& flavor,
                                                        WorkerId const& worker_id)
    {
        detail::CString worker_text{worker_id};
        return create_impl(socket_path, flavor, worker_text.get());
    }

    /// The bridge identity reported during the handshake.
    [[nodiscard]] std::string_view flavor() const noexcept
    {
        return detail::string_view_of(sysa_control_client_flavor(handle_));
    }

    /// Issue one `manager.*` request and borrow its protobuf reply.
    template <StringLike Method, TextLike Request>
    [[nodiscard]] Expected<Bytes> call(Method const& method, Request const& request) const
    {
        detail::CString method_text{method};
        detail::clear_last_error();
        std::uint8_t* data = nullptr;
        std::size_t size = 0;
        const auto raw_status = sysa_control_client_call(handle_, method_text.get(), detail::text_data(request),
                                                         detail::text_size(request), &data, &size);
        if (raw_status != SYSA_STATUS_OK)
        {
            return std::unexpected(detail::take_error(detail::to_status(raw_status)));
        }
        return Bytes{data, size};
    }

    /// Enqueue a start / stop / restart / reload job (protobuf `EnqueueJobResult`).
    template <StringLike Name, StringLike JobType, StringLike Mode>
    [[nodiscard]] Expected<Bytes> enqueue(Name const& name, JobType const& job_type, Mode const& mode,
                                          bool reload_if_possible = false) const
    {
        detail::CString name_text{name};
        detail::CString type_text{job_type};
        detail::CString mode_text{mode};
        detail::clear_last_error();
        std::uint8_t* data = nullptr;
        std::size_t size = 0;
        const auto raw_status = sysa_control_client_enqueue(handle_, name_text.get(), type_text.get(), mode_text.get(),
                                                            reload_if_possible ? 1 : 0, &data, &size);
        if (raw_status != SYSA_STATUS_OK)
        {
            return std::unexpected(detail::take_error(detail::to_status(raw_status)));
        }
        return Bytes{data, size};
    }

    /// Fetch the full current unit set (protobuf `ListSnapshotsResult`).
    [[nodiscard]] Expected<Bytes> list_snapshots() const { return take_reply(sysa_control_client_list_snapshots); }

    /// Fetch one unit's snapshot (protobuf `UnitSnapshotResult`).
    template <StringLike Name> [[nodiscard]] Expected<Bytes> unit_snapshot(Name const& name) const
    {
        detail::CString name_text{name};
        return take_named_reply(sysa_control_client_unit_snapshot, name_text.get());
    }

    /// List the current running jobs (protobuf `ListJobsResult`).
    [[nodiscard]] Expected<Bytes> list_jobs() const { return take_reply(sysa_control_client_list_jobs); }

    /// Ask System A to load one unit (protobuf `LoadUnitResult`).
    template <StringLike Name> [[nodiscard]] Expected<Bytes> load_unit(Name const& name) const
    {
        detail::CString name_text{name};
        return take_named_reply(sysa_control_client_load_unit, name_text.get());
    }

    /// Resolve a PID to a canonical unit name (protobuf `GetUnitByPidResult`).
    [[nodiscard]] Expected<Bytes> get_unit_by_pid(std::uint32_t pid) const
    {
        detail::clear_last_error();
        std::uint8_t* data = nullptr;
        std::size_t size = 0;
        const auto raw_status = sysa_control_client_get_unit_by_pid(handle_, pid, &data, &size);
        if (raw_status != SYSA_STATUS_OK)
        {
            return std::unexpected(detail::take_error(detail::to_status(raw_status)));
        }
        return Bytes{data, size};
    }

    /// Resolve an invocation ID to a canonical unit name (protobuf `GetUnitByPidResult`).
    template <StringLike InvocationId>
    [[nodiscard]] Expected<Bytes> get_unit_by_invocation(InvocationId const& invocation_id) const
    {
        detail::CString id_text{invocation_id};
        return take_named_reply(sysa_control_client_get_unit_by_invocation, id_text.get());
    }

    /// Wait for the next server-pushed event.
    ///
    /// `timeout_ms` of zero waits indefinitely; a positive value bounds the
    /// wait and yields a `timeout` error when no event arrives.
    [[nodiscard]] Expected<ControlEvent> next_event(std::uint32_t timeout_ms = 0) const
    {
        detail::clear_last_error();
        SysaControlEventKind kind = SYSA_CONTROL_EVENT_KIND_UNIT_CHANGED;
        std::uint8_t* data = nullptr;
        std::size_t size = 0;
        const auto raw_status = sysa_control_client_next_event(handle_, timeout_ms, &kind, &data, &size);
        if (raw_status != SYSA_STATUS_OK)
        {
            return std::unexpected(detail::take_error(detail::to_status(raw_status)));
        }
        return ControlEvent{static_cast<ControlEventKind>(kind), Bytes{data, size}};
    }

    [[nodiscard]] SysaControlClient* native_handle() const noexcept { return handle_; }

private:
    using ReplyFn = SysaStatus (*)(const SysaControlClient*, std::uint8_t**, std::size_t*);
    using NamedReplyFn = SysaStatus (*)(const SysaControlClient*, const char*, std::uint8_t**, std::size_t*);

    explicit ControlClient(SysaControlClient* handle) noexcept : handle_{handle} {}

    template <StringLike SocketPath, StringLike Flavor>
    static Expected<ControlClient> create_impl(SocketPath const& socket_path, Flavor const& flavor,
                                               const char* worker_id)
    {
        detail::CString path_text{socket_path};
        detail::CString flavor_text{flavor};
        detail::clear_last_error();
        SysaControlClient* handle = nullptr;
        const auto raw_status = sysa_control_client_new(path_text.get(), flavor_text.get(), worker_id, &handle);
        if (raw_status != SYSA_STATUS_OK)
        {
            return std::unexpected(detail::take_error(detail::to_status(raw_status)));
        }
        return ControlClient{handle};
    }

    [[nodiscard]] Expected<Bytes> take_reply(ReplyFn reply) const
    {
        detail::clear_last_error();
        std::uint8_t* data = nullptr;
        std::size_t size = 0;
        const auto raw_status = reply(handle_, &data, &size);
        if (raw_status != SYSA_STATUS_OK)
        {
            return std::unexpected(detail::take_error(detail::to_status(raw_status)));
        }
        return Bytes{data, size};
    }

    [[nodiscard]] Expected<Bytes> take_named_reply(NamedReplyFn reply, const char* text) const
    {
        detail::clear_last_error();
        std::uint8_t* data = nullptr;
        std::size_t size = 0;
        const auto raw_status = reply(handle_, text, &data, &size);
        if (raw_status != SYSA_STATUS_OK)
        {
            return std::unexpected(detail::take_error(detail::to_status(raw_status)));
        }
        return Bytes{data, size};
    }

    void reset() noexcept
    {
        if (handle_ != nullptr)
        {
            sysa_control_client_free(handle_);
            handle_ = nullptr;
        }
    }

    SysaControlClient* handle_ = nullptr;
};

} // namespace sysa
