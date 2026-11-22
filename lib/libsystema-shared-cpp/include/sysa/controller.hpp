#pragma once

#include "sysa/detail.hpp"
#include "sysa/error.hpp"
#include "sysa/unit_status.hpp"

#include <cstddef>
#include <cstdint>
#include <functional>
#include <span>
#include <string_view>
#include <systema_shared.h>
#include <utility>
#include <vector>

namespace sysa
{

/// The controller table a worker dispatches method calls to.
///
/// Handlers run on blocking threads owned by the C API, serialized per
/// worker.  They may block; the wrapper catches every C++ exception before it
/// can cross the C ABI.
class Controller
{
public:
    using StatusHandler = std::function<Expected<UnitStatus>(std::string_view unit_name)>;
    using StartHandler = std::function<Result(std::string_view unit_name, std::span<const std::byte> config,
                                              std::string_view invocation_id)>;
    using StopHandler = std::function<Result(std::string_view unit_name)>;
    using ReloadHandler = std::function<Result(std::string_view unit_name, std::span<const std::byte> config)>;
    using SyncStateHandler = std::function<Expected<std::vector<UnitStatus>>()>;

    Controller() = default;

    Controller& on_status(StatusHandler handler)
    {
        status_ = std::move(handler);
        return *this;
    }

    Controller& on_start(StartHandler handler)
    {
        start_ = std::move(handler);
        return *this;
    }

    Controller& on_stop(StopHandler handler)
    {
        stop_ = std::move(handler);
        return *this;
    }

    Controller& on_restart(StartHandler handler)
    {
        restart_ = std::move(handler);
        return *this;
    }

    Controller& on_reload(ReloadHandler handler)
    {
        reload_ = std::move(handler);
        return *this;
    }

    Controller& on_sync_state(SyncStateHandler handler)
    {
        sync_state_ = std::move(handler);
        return *this;
    }

private:
    friend class Worker;

    [[nodiscard]] SysaController make_table() noexcept
    {
        SysaController table{};
        table.userdata = this;
        table.status = status_ ? &Controller::status_thunk : nullptr;
        table.start = start_ ? &Controller::start_thunk : nullptr;
        table.stop = stop_ ? &Controller::stop_thunk : nullptr;
        table.restart = restart_ ? &Controller::restart_thunk : nullptr;
        table.reload = reload_ ? &Controller::reload_thunk : nullptr;
        table.sync_state = sync_state_ ? &Controller::sync_state_thunk : nullptr;
        return table;
    }

    static SysaStatus status_thunk(void* userdata, const char* unit_name, SysaUnitStatus** out) noexcept
    {
        auto* self = static_cast<Controller*>(userdata);
        if (self == nullptr || !self->status_ || out == nullptr)
        {
            return SYSA_STATUS_INVALID_ARGUMENT;
        }
        try
        {
            auto result = self->status_(detail::string_view_of(unit_name));
            if (!result)
            {
                return detail::error_to_c_status(result.error());
            }
            *out = result->release();
            return SYSA_STATUS_OK;
        }
        catch (...)
        {
            return SYSA_STATUS_INTERNAL;
        }
    }

    static SysaStatus start_thunk(void* userdata, const char* unit_name, const std::uint8_t* config,
                                  std::size_t config_len, const char* invocation_id) noexcept
    {
        auto* self = static_cast<Controller*>(userdata);
        if (self == nullptr || !self->start_)
        {
            return SYSA_STATUS_INVALID_ARGUMENT;
        }
        try
        {
            auto result = self->start_(detail::string_view_of(unit_name), detail::byte_span(config, config_len),
                                       detail::string_view_of(invocation_id));
            return result ? SYSA_STATUS_OK : detail::error_to_c_status(result.error());
        }
        catch (...)
        {
            return SYSA_STATUS_INTERNAL;
        }
    }

    static SysaStatus stop_thunk(void* userdata, const char* unit_name) noexcept
    {
        auto* self = static_cast<Controller*>(userdata);
        if (self == nullptr || !self->stop_)
        {
            return SYSA_STATUS_INVALID_ARGUMENT;
        }
        try
        {
            auto result = self->stop_(detail::string_view_of(unit_name));
            return result ? SYSA_STATUS_OK : detail::error_to_c_status(result.error());
        }
        catch (...)
        {
            return SYSA_STATUS_INTERNAL;
        }
    }

    static SysaStatus restart_thunk(void* userdata, const char* unit_name, const std::uint8_t* config,
                                    std::size_t config_len, const char* invocation_id) noexcept
    {
        auto* self = static_cast<Controller*>(userdata);
        if (self == nullptr || !self->restart_)
        {
            return SYSA_STATUS_INVALID_ARGUMENT;
        }
        try
        {
            auto result = self->restart_(detail::string_view_of(unit_name), detail::byte_span(config, config_len),
                                         detail::string_view_of(invocation_id));
            return result ? SYSA_STATUS_OK : detail::error_to_c_status(result.error());
        }
        catch (...)
        {
            return SYSA_STATUS_INTERNAL;
        }
    }

    static SysaStatus reload_thunk(void* userdata, const char* unit_name, const std::uint8_t* config,
                                   std::size_t config_len) noexcept
    {
        auto* self = static_cast<Controller*>(userdata);
        if (self == nullptr || !self->reload_)
        {
            return SYSA_STATUS_INVALID_ARGUMENT;
        }
        try
        {
            auto result = self->reload_(detail::string_view_of(unit_name), detail::byte_span(config, config_len));
            return result ? SYSA_STATUS_OK : detail::error_to_c_status(result.error());
        }
        catch (...)
        {
            return SYSA_STATUS_INTERNAL;
        }
    }

    static SysaStatus sync_state_thunk(void* userdata, SysaUnitStatus*** out, std::size_t* count) noexcept
    {
        auto* self = static_cast<Controller*>(userdata);
        if (self == nullptr || !self->sync_state_ || out == nullptr || count == nullptr)
        {
            return SYSA_STATUS_INVALID_ARGUMENT;
        }
        try
        {
            auto result = self->sync_state_();
            if (!result)
            {
                return detail::error_to_c_status(result.error());
            }
            auto& statuses = *result;
            SysaUnitStatus** array = nullptr;
            const auto raw_status = sysa_unit_status_array_alloc(statuses.size(), &array);
            if (raw_status != SYSA_STATUS_OK)
            {
                return raw_status;
            }
            for (std::size_t index = 0; index < statuses.size(); ++index)
            {
                array[index] = statuses[index].release();
            }
            *out = array;
            *count = statuses.size();
            return SYSA_STATUS_OK;
        }
        catch (...)
        {
            return SYSA_STATUS_INTERNAL;
        }
    }

    StatusHandler status_;
    StartHandler start_;
    StopHandler stop_;
    StartHandler restart_;
    ReloadHandler reload_;
    SyncStateHandler sync_state_;
};

} // namespace sysa
