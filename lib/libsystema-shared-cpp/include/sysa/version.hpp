#pragma once

#include <cstdint>
#include <string_view>
#include <systema_shared.h>

namespace sysa
{

/// The runtime library version reported by the C API.
[[nodiscard]] inline std::string_view version() noexcept
{
    const char* text = sysa_version();
    return text == nullptr ? std::string_view{} : std::string_view{text};
}

/// The ABI revision of the C surface.
[[nodiscard]] inline std::uint32_t abi_version() noexcept
{
    return sysa_abi_version();
}

} // namespace sysa
