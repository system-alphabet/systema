#pragma once

#include "sysa/common.hpp"
#include "sysa/concepts.hpp"
#include "sysa/config_map.hpp"
#include "sysa/detail.hpp"
#include "sysa/error.hpp"
#include "sysa/unit_type.hpp"

#include <cstdint>
#include <optional>
#include <string>
#include <string_view>
#include <systema_shared.h>
#include <utility>

namespace sysa {

/// A single unit's intermediate representation (`UnitIR`).
///
/// Only the C API's accessors are wrapped; fields the C surface does not
/// expose (dependencies, conditions, ...) remain available through JSON
/// round-tripping with `to_json()`.
class UnitIR {
public:
    UnitIR(UnitIR&& other) noexcept : handle_{std::exchange(other.handle_, nullptr)} {}

    UnitIR& operator=(UnitIR&& other) noexcept {
        if (this != &other) {
            reset();
            handle_ = std::exchange(other.handle_, nullptr);
        }
        return *this;
    }

    UnitIR(UnitIR const&) = delete;
    UnitIR& operator=(UnitIR const&) = delete;

    ~UnitIR() { reset(); }

    /// Parse a unit IR from JSON.
    template <TextLike Json> [[nodiscard]] static Expected<UnitIR> from_json(Json const& json) {
        detail::clear_last_error();
        SysaUnitIR* handle = nullptr;
        const auto raw_status = sysa_unit_ir_from_json(detail::text_data(json), detail::text_size(json), &handle);
        if (raw_status != SYSA_STATUS_OK) {
            return std::unexpected(detail::take_error(detail::to_status(raw_status)));
        }
        if (handle == nullptr) {
            return std::unexpected(detail::internal_error("unit IR parsing returned no IR"));
        }
        return UnitIR{handle};
    }

    /// Serialize the IR back to JSON.
    [[nodiscard]] Expected<Bytes> to_json() const {
        detail::clear_last_error();
        std::uint8_t* data = nullptr;
        std::size_t size = 0;
        const auto raw_status = sysa_unit_ir_to_json(handle_, &data, &size);
        if (raw_status != SYSA_STATUS_OK) {
            return std::unexpected(detail::take_error(detail::to_status(raw_status)));
        }
        return Bytes{data, size};
    }

    /// The canonical unit ID.
    [[nodiscard]] Expected<OwnedString> id() const { return read_required(&id_accessor); }

    /// The unit type; `std::nullopt` when the IR does not report one.
    [[nodiscard]] Expected<std::optional<OwnedString>> unit_type() const { return read_optional(&unit_type_accessor); }

    /// The parsed unit type; `std::nullopt` when the IR does not report one.
    [[nodiscard]] Expected<std::optional<UnitType>> unit_kind() const {
        auto value = unit_type();
        if (!value) {
            return std::unexpected(value.error());
        }
        if (!*value) {
            return std::optional<UnitType>{};
        }
        return std::optional<UnitType>{UnitType::parse((**value).view())};
    }

    /// The human-readable description.
    [[nodiscard]] Expected<std::optional<OwnedString>> description() const {
        return read_optional(&description_accessor);
    }

    /// The source format (`systemd`, `sysv`, ...).
    [[nodiscard]] Expected<std::optional<OwnedString>> source_format() const {
        return read_optional(&source_format_accessor);
    }

    /// The source file path.
    [[nodiscard]] Expected<std::optional<OwnedString>> source_path() const {
        return read_optional(&source_path_accessor);
    }

    /// The pinned owner worker; `std::nullopt` when type-based routing applies.
    [[nodiscard]] Expected<std::optional<OwnedString>> owner() const { return read_optional(&owner_accessor); }

    /// The alias target when this record is an alias.
    [[nodiscard]] Expected<std::optional<OwnedString>> alias_of() const { return read_optional(&alias_of_accessor); }

    /// The parent slice.
    [[nodiscard]] Expected<std::optional<OwnedString>> slice() const { return read_optional(&slice_accessor); }

    /// The template this IR belongs to.
    [[nodiscard]] Expected<std::optional<OwnedString>> template_of() const {
        return read_optional(&template_of_accessor);
    }

    /// The reported alias list.
    [[nodiscard]] Expected<StringList> aliases() const {
        detail::clear_last_error();
        SysaStringList* list = nullptr;
        const auto raw_status = sysa_unit_ir_aliases(handle_, &list);
        if (raw_status != SYSA_STATUS_OK) {
            return std::unexpected(detail::take_error(detail::to_status(raw_status)));
        }
        return StringList{list};
    }

    /// The type-specific directive map; `std::nullopt` when not reported.
    [[nodiscard]] Expected<std::optional<ConfigMap>> config() const {
        detail::clear_last_error();
        SysaConfigMap* map = nullptr;
        const auto raw_status = sysa_unit_ir_config(handle_, &map);
        if (raw_status == SYSA_STATUS_NOT_FOUND) {
            if (map != nullptr) {
                sysa_config_map_free(map);
            }
            sysa_clear_error();
            return std::optional<ConfigMap>{};
        }
        if (raw_status != SYSA_STATUS_OK) {
            if (map != nullptr) {
                sysa_config_map_free(map);
            }
            return std::unexpected(detail::take_error(detail::to_status(raw_status)));
        }
        ConfigMap value{map};
        return std::optional<ConfigMap>{std::move(value)};
    }

    [[nodiscard]] bool is_template() const noexcept { return sysa_unit_ir_is_template(handle_) != 0; }

    [[nodiscard]] bool is_template_instance() const noexcept { return sysa_unit_ir_is_template_instance(handle_) != 0; }

    /// The template parameter list (empty when the IR has no template).
    [[nodiscard]] Expected<StringList> template_parameters() const {
        detail::clear_last_error();
        SysaStringList* list = nullptr;
        const auto raw_status = sysa_unit_ir_template_parameters(handle_, &list);
        if (raw_status != SYSA_STATUS_OK) {
            return std::unexpected(detail::take_error(detail::to_status(raw_status)));
        }
        return StringList{list};
    }

    /// Pin the owning worker.
    template <StringLike Owner> Result set_owner(Owner const& owner) {
        detail::CString owner_text{owner};
        detail::clear_last_error();
        const auto raw_status = sysa_unit_ir_set_owner(handle_, owner_text.get());
        return detail::status_to_result(raw_status);
    }

    /// Clear the owner pin and restore type-based routing.
    Result clear_owner() {
        detail::clear_last_error();
        const auto raw_status = sysa_unit_ir_set_owner(handle_, nullptr);
        return detail::status_to_result(raw_status);
    }

    [[nodiscard]] SysaUnitIR* native_handle() const noexcept { return handle_; }

private:
    using StringAccessor = SysaStatus (*)(const SysaUnitIR*, char**);

    explicit UnitIR(SysaUnitIR* handle) noexcept : handle_{handle} {}

    [[nodiscard]] Expected<OwnedString> read_required(StringAccessor accessor) const {
        detail::clear_last_error();
        char* text = nullptr;
        const auto raw_status = accessor(handle_, &text);
        if (raw_status != SYSA_STATUS_OK) {
            if (text != nullptr) {
                sysa_string_free(text);
            }
            return std::unexpected(detail::take_error(detail::to_status(raw_status)));
        }
        return OwnedString{text};
    }

    [[nodiscard]] Expected<std::optional<OwnedString>> read_optional(StringAccessor accessor) const {
        detail::clear_last_error();
        char* text = nullptr;
        const auto raw_status = accessor(handle_, &text);
        if (raw_status == SYSA_STATUS_NOT_FOUND) {
            if (text != nullptr) {
                sysa_string_free(text);
            }
            sysa_clear_error();
            return std::optional<OwnedString>{};
        }
        if (raw_status != SYSA_STATUS_OK) {
            if (text != nullptr) {
                sysa_string_free(text);
            }
            return std::unexpected(detail::take_error(detail::to_status(raw_status)));
        }
        return std::optional<OwnedString>{std::in_place, text};
    }

    static SysaStatus id_accessor(const SysaUnitIR* ir, char** out) { return sysa_unit_ir_id(ir, out); }

    static SysaStatus unit_type_accessor(const SysaUnitIR* ir, char** out) { return sysa_unit_ir_unit_type(ir, out); }

    static SysaStatus description_accessor(const SysaUnitIR* ir, char** out) {
        return sysa_unit_ir_description(ir, out);
    }

    static SysaStatus source_format_accessor(const SysaUnitIR* ir, char** out) {
        return sysa_unit_ir_source_format(ir, out);
    }

    static SysaStatus source_path_accessor(const SysaUnitIR* ir, char** out) {
        return sysa_unit_ir_source_path(ir, out);
    }

    static SysaStatus owner_accessor(const SysaUnitIR* ir, char** out) { return sysa_unit_ir_owner(ir, out); }

    static SysaStatus alias_of_accessor(const SysaUnitIR* ir, char** out) { return sysa_unit_ir_alias_of(ir, out); }

    static SysaStatus slice_accessor(const SysaUnitIR* ir, char** out) { return sysa_unit_ir_slice(ir, out); }

    static SysaStatus template_of_accessor(const SysaUnitIR* ir, char** out) {
        return sysa_unit_ir_template_of(ir, out);
    }

    void reset() noexcept {
        if (handle_ != nullptr) {
            sysa_unit_ir_free(handle_);
            handle_ = nullptr;
        }
    }

    SysaUnitIR* handle_ = nullptr;
};

} // namespace sysa
