#pragma once

#include "sysa/common.hpp"
#include "sysa/concepts.hpp"
#include "sysa/detail.hpp"
#include "sysa/error.hpp"

#include <cstddef>
#include <cstdint>
#include <optional>
#include <span>
#include <string>
#include <string_view>
#include <systema_shared.h>
#include <unordered_map>
#include <utility>
#include <vector>

namespace sysa
{

/// One extension entry carried by a `UnitStatus`.
struct Extension
{
    std::string_view key;
    std::string_view value;
};

/// The standard runtime status model of a unit.
///
/// String accessors borrow the status object's storage and stay valid until
/// the status is freed or modified.  `encode` / `decode` exchange the
/// protobuf representation used on the wire.
class UnitStatus
{
public:
    UnitStatus(UnitStatus&& other) noexcept : handle_{std::exchange(other.handle_, nullptr)} {}

    UnitStatus& operator=(UnitStatus&& other) noexcept
    {
        if (this != &other)
        {
            reset();
            handle_ = std::exchange(other.handle_, nullptr);
        }
        return *this;
    }

    UnitStatus(UnitStatus const&) = delete;
    UnitStatus& operator=(UnitStatus const&) = delete;

    ~UnitStatus() { reset(); }

    /// Build a status for an allocated status pointer.
    template <StringLike Name, StringLike ActiveState, StringLike SubState, StringLike InvocationId>
    [[nodiscard]] static Expected<UnitStatus> create(Name const& unit_name, ActiveState const& active_state,
                                                     SubState const& sub_state, std::uint32_t main_pid,
                                                     InvocationId const& invocation_id)
    {
        detail::CString name_text{unit_name};
        detail::CString active_text{active_state};
        detail::CString sub_text{sub_state};
        detail::CString invocation_text{invocation_id};

        detail::clear_last_error();
        SysaUnitStatus* handle =
            sysa_unit_status_new(name_text.get(), active_text.get(), sub_text.get(), main_pid, invocation_text.get());
        if (handle == nullptr)
        {
            return std::unexpected(detail::take_last_error(Status::invalid_argument));
        }
        return UnitStatus{handle};
    }

    /// Add or replace one extension entry.
    template <StringLike Key, StringLike Value> Result set_extension(Key const& key, Value const& value)
    {
        detail::CString key_text{key};
        detail::CString value_text{value};
        detail::clear_last_error();
        const auto raw_status = sysa_unit_status_set_extension(handle_, key_text.get(), value_text.get());
        return detail::status_to_result(raw_status);
    }

    /// Add or replace every entry of a string-to-string map.
    template <MapLike Extensions> Result set_extensions(Extensions const& extensions)
    {
        for (auto const& [key, value] : extensions)
        {
            auto result = set_extension(key, value);
            if (!result)
            {
                return result;
            }
        }
        return {};
    }

    [[nodiscard]] std::string_view unit_name() const noexcept
    {
        return detail::string_view_of(sysa_unit_status_unit_name(handle_));
    }

    [[nodiscard]] std::string_view active_state() const noexcept
    {
        return detail::string_view_of(sysa_unit_status_active_state(handle_));
    }

    [[nodiscard]] std::string_view sub_state() const noexcept
    {
        return detail::string_view_of(sysa_unit_status_sub_state(handle_));
    }

    [[nodiscard]] std::string_view invocation_id() const noexcept
    {
        return detail::string_view_of(sysa_unit_status_invocation_id(handle_));
    }

    [[nodiscard]] std::uint32_t main_pid() const noexcept { return sysa_unit_status_main_pid(handle_); }

    [[nodiscard]] std::size_t extension_count() const noexcept { return sysa_unit_status_extension_count(handle_); }

    [[nodiscard]] std::optional<std::string_view> extension_key(std::size_t index) const noexcept
    {
        const char* key = sysa_unit_status_extension_key(handle_, index);
        return key == nullptr ? std::nullopt : std::optional<std::string_view>{key};
    }

    [[nodiscard]] std::optional<std::string_view> extension_value(std::size_t index) const noexcept
    {
        const char* value = sysa_unit_status_extension_value(handle_, index);
        return value == nullptr ? std::nullopt : std::optional<std::string_view>{value};
    }

    [[nodiscard]] std::optional<Extension> extension(std::size_t index) const noexcept
    {
        const char* key = sysa_unit_status_extension_key(handle_, index);
        if (key == nullptr)
        {
            return std::nullopt;
        }
        const char* value = sysa_unit_status_extension_value(handle_, index);
        if (value == nullptr)
        {
            return std::nullopt;
        }
        return Extension{key, value};
    }

    /// Borrow every extension entry.
    [[nodiscard]] std::vector<Extension> extensions() const
    {
        std::vector<Extension> entries;
        entries.reserve(extension_count());
        for (std::size_t index = 0; index < extension_count(); ++index)
        {
            if (auto entry = extension(index))
            {
                entries.push_back(*entry);
            }
        }
        return entries;
    }

    /// Copy every extension entry into an owning map.
    [[nodiscard]] std::unordered_map<std::string, std::string> extension_map() const
    {
        std::unordered_map<std::string, std::string> entries;
        entries.reserve(extension_count());
        for (std::size_t index = 0; index < extension_count(); ++index)
        {
            if (auto entry = extension(index))
            {
                entries.emplace(std::string{entry->key}, std::string{entry->value});
            }
        }
        return entries;
    }

    /// Serialize as a `UnitStatus` protobuf message.
    [[nodiscard]] Expected<Bytes> encode() const
    {
        detail::clear_last_error();
        std::uint8_t* data = nullptr;
        std::size_t size = 0;
        const auto raw_status = sysa_unit_status_encode(handle_, &data, &size);
        if (raw_status != SYSA_STATUS_OK)
        {
            return std::unexpected(detail::take_error(detail::to_status(raw_status)));
        }
        return Bytes{data, size};
    }

    /// Parse a `UnitStatus` protobuf message.
    template <ByteSequence Data> [[nodiscard]] static Expected<UnitStatus> decode(Data const& data)
    {
        detail::clear_last_error();
        SysaUnitStatus* handle = nullptr;
        const auto raw_status = sysa_unit_status_decode(detail::byte_data(data), detail::byte_size(data), &handle);
        if (raw_status != SYSA_STATUS_OK)
        {
            return std::unexpected(detail::take_error(detail::to_status(raw_status)));
        }
        if (handle == nullptr)
        {
            return std::unexpected(detail::internal_error("unit status decoding returned no status"));
        }
        return UnitStatus{handle};
    }

    /// Relinquish ownership, e.g. when handing a status to a `sync_state`
    /// callback.
    [[nodiscard]] SysaUnitStatus* release() noexcept { return std::exchange(handle_, nullptr); }

    [[nodiscard]] SysaUnitStatus* native_handle() const noexcept { return handle_; }

private:
    explicit UnitStatus(SysaUnitStatus* handle) noexcept : handle_{handle} {}

    void reset() noexcept
    {
        if (handle_ != nullptr)
        {
            sysa_unit_status_free(handle_);
            handle_ = nullptr;
        }
    }

    SysaUnitStatus* handle_ = nullptr;
};

} // namespace sysa
