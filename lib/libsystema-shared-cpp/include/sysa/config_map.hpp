#pragma once

#include "sysa/common.hpp"
#include "sysa/concepts.hpp"
#include "sysa/detail.hpp"
#include "sysa/error.hpp"

#include <cstdint>
#include <string>
#include <string_view>
#include <systema_shared.h>
#include <utility>

namespace sysa {

class UnitIR;

/// The flattened type-specific directive map (`ConfigMap`).
///
/// The C API only exposes JSON round-tripping; the typed getters map directly
/// to `ir::config::get_*` in `libsystema-shared`.
class ConfigMap {
public:
    ConfigMap(ConfigMap&& other) noexcept : handle_{std::exchange(other.handle_, nullptr)} {}

    ConfigMap& operator=(ConfigMap&& other) noexcept {
        if (this != &other) {
            reset();
            handle_ = std::exchange(other.handle_, nullptr);
        }
        return *this;
    }

    ConfigMap(ConfigMap const&) = delete;
    ConfigMap& operator=(ConfigMap const&) = delete;

    ~ConfigMap() { reset(); }

    /// Parse a JSON object.
    template <TextLike Json> [[nodiscard]] static Expected<ConfigMap> from_json(Json const& json) {
        detail::clear_last_error();
        SysaConfigMap* handle = nullptr;
        const auto raw_status = sysa_config_map_parse_json(detail::text_data(json), detail::text_size(json), &handle);
        if (raw_status != SYSA_STATUS_OK) {
            return std::unexpected(detail::take_error(detail::to_status(raw_status)));
        }
        if (handle == nullptr) {
            return std::unexpected(detail::internal_error("config map parsing returned no map"));
        }
        return ConfigMap{handle};
    }

    /// Serialize the map back to JSON.
    [[nodiscard]] Expected<Bytes> to_json() const {
        detail::clear_last_error();
        std::uint8_t* data = nullptr;
        std::size_t size = 0;
        const auto raw_status = sysa_config_map_to_json(handle_, &data, &size);
        if (raw_status != SYSA_STATUS_OK) {
            return std::unexpected(detail::take_error(detail::to_status(raw_status)));
        }
        return Bytes{data, size};
    }

    /// Read a string directive; `not_found` when the key is absent.
    template <StringLike Key> [[nodiscard]] Expected<OwnedString> get_string(Key const& key) const {
        detail::CString key_text{key};
        detail::clear_last_error();
        char* text = nullptr;
        const auto raw_status = sysa_config_map_get_string(handle_, key_text.get(), &text);
        if (raw_status != SYSA_STATUS_OK) {
            return std::unexpected(detail::take_error(detail::to_status(raw_status)));
        }
        return OwnedString{text};
    }

    /// Read a repeated directive into a new list (empty when absent).
    template <StringLike Key> [[nodiscard]] Expected<StringList> get_strings(Key const& key) const {
        detail::CString key_text{key};
        detail::clear_last_error();
        SysaStringList* list = nullptr;
        const auto raw_status = sysa_config_map_get_strings(handle_, key_text.get(), &list);
        if (raw_status != SYSA_STATUS_OK) {
            return std::unexpected(detail::take_error(detail::to_status(raw_status)));
        }
        return StringList{list};
    }

    /// Read a boolean directive (absent keys read as false).
    template <StringLike Key> [[nodiscard]] Expected<bool> get_bool(Key const& key) const {
        detail::CString key_text{key};
        detail::clear_last_error();
        std::uint8_t value = 0;
        const auto raw_status = sysa_config_map_get_bool(handle_, key_text.get(), &value);
        if (raw_status != SYSA_STATUS_OK) {
            return std::unexpected(detail::take_error(detail::to_status(raw_status)));
        }
        return value != 0;
    }

    /// Read an unsigned directive; `not_found` when absent or not an integer.
    template <StringLike Key> [[nodiscard]] Expected<std::uint32_t> get_u32(Key const& key) const {
        detail::CString key_text{key};
        detail::clear_last_error();
        std::uint32_t value = 0;
        const auto raw_status = sysa_config_map_get_u32(handle_, key_text.get(), &value);
        if (raw_status != SYSA_STATUS_OK) {
            return std::unexpected(detail::take_error(detail::to_status(raw_status)));
        }
        return value;
    }

    /// Read a signed directive; `not_found` when absent or not an integer.
    template <StringLike Key> [[nodiscard]] Expected<std::int32_t> get_i32(Key const& key) const {
        detail::CString key_text{key};
        detail::clear_last_error();
        std::int32_t value = 0;
        const auto raw_status = sysa_config_map_get_i32(handle_, key_text.get(), &value);
        if (raw_status != SYSA_STATUS_OK) {
            return std::unexpected(detail::take_error(detail::to_status(raw_status)));
        }
        return value;
    }

    /// Read a file-mode directive; `not_found` when absent or not a mode.
    template <StringLike Key> [[nodiscard]] Expected<std::uint32_t> get_mode(Key const& key) const {
        detail::CString key_text{key};
        detail::clear_last_error();
        std::uint32_t value = 0;
        const auto raw_status = sysa_config_map_get_mode(handle_, key_text.get(), &value);
        if (raw_status != SYSA_STATUS_OK) {
            return std::unexpected(detail::take_error(detail::to_status(raw_status)));
        }
        return value;
    }

    /// Serialize one raw JSON value; `not_found` when the key is absent.
    template <StringLike Key> [[nodiscard]] Expected<Bytes> get_json(Key const& key) const {
        detail::CString key_text{key};
        detail::clear_last_error();
        std::uint8_t* data = nullptr;
        std::size_t size = 0;
        const auto raw_status = sysa_config_map_get_json(handle_, key_text.get(), &data, &size);
        if (raw_status != SYSA_STATUS_OK) {
            return std::unexpected(detail::take_error(detail::to_status(raw_status)));
        }
        return Bytes{data, size};
    }

    /// `get_string` with a fallback used when the key is absent.
    template <StringLike Key, StringLike Fallback>
    [[nodiscard]] Expected<std::string> get_string_or(Key const& key, Fallback const& fallback) const {
        auto value = get_string(key);
        if (value) {
            return value->to_string();
        }
        if (value.error().status() == Status::not_found) {
            return detail::to_string(fallback);
        }
        return std::unexpected(value.error());
    }

    /// `get_u32` with a fallback used when the key is absent.
    template <StringLike Key>
    [[nodiscard]] Expected<std::uint32_t> get_u32_or(Key const& key, std::uint32_t fallback) const {
        auto value = get_u32(key);
        if (value) {
            return value;
        }
        if (value.error().status() == Status::not_found) {
            return fallback;
        }
        return std::unexpected(value.error());
    }

    /// `get_i32` with a fallback used when the key is absent.
    template <StringLike Key>
    [[nodiscard]] Expected<std::int32_t> get_i32_or(Key const& key, std::int32_t fallback) const {
        auto value = get_i32(key);
        if (value) {
            return value;
        }
        if (value.error().status() == Status::not_found) {
            return fallback;
        }
        return std::unexpected(value.error());
    }

    [[nodiscard]] SysaConfigMap* native_handle() const noexcept { return handle_; }

private:
    friend class UnitIR;

    explicit ConfigMap(SysaConfigMap* handle) noexcept : handle_{handle} {}

    void reset() noexcept {
        if (handle_ != nullptr) {
            sysa_config_map_free(handle_);
            handle_ = nullptr;
        }
    }

    SysaConfigMap* handle_ = nullptr;
};

} // namespace sysa
