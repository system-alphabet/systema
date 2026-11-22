#pragma once

#include "sysa/common.hpp"
#include "sysa/concepts.hpp"
#include "sysa/detail.hpp"
#include "sysa/error.hpp"
#include "sysa/unit_status.hpp"

#include <cstddef>
#include <cstdint>
#include <systema_shared.h>
#include <vector>

namespace sysa
{

/// A non-owning publisher handle for a running worker.
///
/// Mirrors `libsystema_shared::worker_ipc::EventPublisher`: `Worker::publisher()`
/// hands one out, a controller can copy it and keep it in its handlers, and
/// every call is routed through the worker's IPC loop.  The handle does not
/// own the worker; it stays valid until the `Worker` is destroyed.
class EventPublisher
{
public:
    EventPublisher() noexcept = default;
    explicit EventPublisher(SysaWorker* worker) noexcept : worker_{worker} {}

    [[nodiscard]] explicit operator bool() const noexcept { return worker_ != nullptr; }
    [[nodiscard]] SysaWorker* native_handle() const noexcept { return worker_; }

    /// Publish a `unit.state_update` snapshot (`full_snapshot` on reconnect).
    template <SequenceOf<UnitStatus> Statuses>
    Result publish_unit_states(Statuses const& statuses, bool full_snapshot) const
    {
        std::vector<const SysaUnitStatus*> pointers;
        if constexpr (std::ranges::sized_range<Statuses>)
        {
            pointers.reserve(static_cast<std::size_t>(std::ranges::size(statuses)));
        }
        for (auto const& status : statuses)
        {
            pointers.push_back(status.native_handle());
        }

        detail::clear_last_error();
        const auto raw_status =
            sysa_worker_publish_unit_states(worker_, pointers.data(), pointers.size(), full_snapshot ? 1 : 0);
        return detail::status_to_result(raw_status);
    }

    /// Ask System A where a unit's cgroup lives (protobuf `UnitCgroupResult`).
    template <StringLike UnitName>
    [[nodiscard]] Expected<Bytes> query_unit_cgroup(UnitName const& unit_name, std::uint32_t timeout_ms = 0) const
    {
        detail::CString name_text{unit_name};
        detail::clear_last_error();
        std::uint8_t* data = nullptr;
        std::size_t size = 0;
        const auto raw_status = sysa_worker_query_unit_cgroup(worker_, name_text.get(), timeout_ms, &data, &size);
        if (raw_status != SYSA_STATUS_OK)
        {
            return std::unexpected(detail::take_error(detail::to_status(raw_status)));
        }
        return Bytes{data, size};
    }

    /// Send a fire-and-forget envelope with a raw payload.
    template <StringLike Method, ByteSequence Payload>
    Result send_envelope_bytes(Method const& method, Payload const& payload) const
    {
        detail::CString method_text{method};
        detail::clear_last_error();
        const auto raw_status = sysa_worker_send_envelope_bytes(worker_, method_text.get(), detail::byte_data(payload),
                                                                detail::byte_size(payload));
        return detail::status_to_result(raw_status);
    }

    /// Send a reply envelope with a raw payload.
    template <StringLike Method, ByteSequence Payload>
    Result send_reply(std::uint64_t request_id, Method const& method, Payload const& payload) const
    {
        detail::CString method_text{method};
        detail::clear_last_error();
        const auto raw_status = sysa_worker_send_reply(worker_, request_id, method_text.get(),
                                                       detail::byte_data(payload), detail::byte_size(payload));
        return detail::status_to_result(raw_status);
    }

    /// Subscribe to `event.publish` notifications; an empty list means all units.
    template <StringSequence Names> Result subscribe_units(Names const& names) const
    {
        detail::CStringArray array{names};
        detail::clear_last_error();
        const auto raw_status = sysa_worker_subscribe_units(worker_, array.data(), array.size());
        return detail::status_to_result(raw_status);
    }

    /// Cancel `event.publish` subscriptions; an empty list cancels all of them.
    template <StringSequence Names> Result unsubscribe_units(Names const& names) const
    {
        detail::CStringArray array{names};
        detail::clear_last_error();
        const auto raw_status = sysa_worker_unsubscribe_units(worker_, array.data(), array.size());
        return detail::status_to_result(raw_status);
    }

private:
    SysaWorker* worker_ = nullptr;
};

} // namespace sysa
