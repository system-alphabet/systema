#pragma once

#include "sysa/concepts.hpp"

#include <concepts>
#include <cstdint>
#include <string>
#include <string_view>
#include <utility>

namespace sysa {

/// The kind of a unit, mirroring `libsystema_shared::ir::UnitType`.
enum class UnitKind : std::uint8_t {
    service,
    target,
    mount,
    automount,
    timer,
    socket,
    slice,
    scope,
    swap,
    path,
    device,
    power,
    /// A producer-specific type that has no dedicated enumerator.
    other,
};

/// A unit type: a standard kind or a producer-specific name.
///
/// `UnitIR` reports the type as a string.  This class gives consumers a closed
/// enumeration for the standard kinds while keeping an unknown name verbatim,
/// which is the same shape as the Rust `UnitType::Other(String)` variant.
class UnitType {
public:
    UnitType() noexcept = default;

    explicit UnitType(UnitKind kind) noexcept : kind_{kind} {}

    /// A producer-specific type name (for example `openrc-script`).
    explicit UnitType(std::string name) : kind_{UnitKind::other}, custom_{std::move(name)} {}

    /// Parse a type name; unknown names become `UnitKind::other`.
    template <StringLike S> [[nodiscard]] static UnitType parse(S const& value) {
        if constexpr (std::convertible_to<S const&, const char*>) {
            const char* text = value;
            return parse_view(text == nullptr ? std::string_view{} : std::string_view{text});
        } else {
            return parse_view(std::string_view{value.data(), value.size()});
        }
    }

    [[nodiscard]] UnitKind kind() const noexcept { return kind_; }

    /// The lower-case type name; an unknown type returns its original name.
    [[nodiscard]] std::string_view name() const noexcept {
        switch (kind_) {
        case UnitKind::service:
            return "service";
        case UnitKind::target:
            return "target";
        case UnitKind::mount:
            return "mount";
        case UnitKind::automount:
            return "automount";
        case UnitKind::timer:
            return "timer";
        case UnitKind::socket:
            return "socket";
        case UnitKind::slice:
            return "slice";
        case UnitKind::scope:
            return "scope";
        case UnitKind::swap:
            return "swap";
        case UnitKind::path:
            return "path";
        case UnitKind::device:
            return "device";
        case UnitKind::power:
            return "power";
        case UnitKind::other:
            return custom_;
        }
        return custom_;
    }

    /// Whether the type is one of the standard kinds.
    [[nodiscard]] bool is_standard() const noexcept { return kind_ != UnitKind::other; }

    [[nodiscard]] friend bool operator==(UnitType const& lhs, UnitType const& rhs) noexcept {
        if (lhs.kind_ != rhs.kind_) {
            return false;
        }
        return lhs.kind_ != UnitKind::other || lhs.custom_ == rhs.custom_;
    }

private:
    [[nodiscard]] static UnitType parse_view(std::string_view name) {
        if (name == "service") {
            return UnitType{UnitKind::service};
        }
        if (name == "target") {
            return UnitType{UnitKind::target};
        }
        if (name == "mount") {
            return UnitType{UnitKind::mount};
        }
        if (name == "automount") {
            return UnitType{UnitKind::automount};
        }
        if (name == "timer") {
            return UnitType{UnitKind::timer};
        }
        if (name == "socket") {
            return UnitType{UnitKind::socket};
        }
        if (name == "slice") {
            return UnitType{UnitKind::slice};
        }
        if (name == "scope") {
            return UnitType{UnitKind::scope};
        }
        if (name == "swap") {
            return UnitType{UnitKind::swap};
        }
        if (name == "path") {
            return UnitType{UnitKind::path};
        }
        if (name == "device") {
            return UnitType{UnitKind::device};
        }
        if (name == "power") {
            return UnitType{UnitKind::power};
        }
        return UnitType{std::string{name}};
    }

    UnitKind kind_ = UnitKind::other;
    std::string custom_;
};

} // namespace sysa
