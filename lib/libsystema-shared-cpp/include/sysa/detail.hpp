#pragma once

#include "sysa/common.hpp"
#include "sysa/concepts.hpp"
#include "sysa/error.hpp"

#include <concepts>
#include <cstddef>
#include <cstdint>
#include <ranges>
#include <span>
#include <string>
#include <string_view>
#include <systema_shared.h>
#include <vector>

namespace sysa::detail {

/// Adapt a `StringLike` argument to the NUL-terminated form expected by the C
/// API.  Already terminated inputs are used in place; the rest are copied
/// into the adapter's own storage.
template <StringLike S> class CString {
public:
    explicit CString(S const& value) {
        if constexpr (std::convertible_to<S const&, const char*>) {
            pointer_ = value;
        } else if constexpr (CStringLike<S>) {
            pointer_ = value.c_str();
        } else {
            if (value.size() != 0) {
                storage_.assign(value.data(), value.size());
            }
            pointer_ = storage_.c_str();
        }
    }

    CString(CString const&) = delete;
    CString& operator=(CString const&) = delete;
    CString(CString&&) = delete;
    CString& operator=(CString&&) = delete;

    [[nodiscard]] const char* get() const noexcept { return pointer_; }

private:
    const char* pointer_ = nullptr;
    std::string storage_;
};

/// Copy a `StringLike` argument into an owning string.
template <StringLike S> [[nodiscard]] inline std::string to_string(S const& value) {
    if constexpr (std::convertible_to<S const&, const char*>) {
        const char* text = value;
        return text == nullptr ? std::string{} : std::string{text};
    } else {
        if (value.size() == 0) {
            return {};
        }
        return std::string{value.data(), value.size()};
    }
}

/// Build the `const char *const *` array used by the C API.
///
/// NUL-terminated elements are borrowed for the duration of the call;
/// the remaining elements are copied into stable storage owned by this
/// adapter.
template <StringSequence S> class CStringArray {
public:
    explicit CStringArray(S const& values) {
        using element = std::ranges::range_value_t<S>;

        if constexpr (std::ranges::sized_range<S>) {
            const auto count = static_cast<std::size_t>(std::ranges::size(values));
            owned_.reserve(count);
            pointers_.reserve(count);
        }

        if constexpr (!CStringLike<element>) {
            for (auto const& value : values) {
                if (value.size() == 0) {
                    owned_.emplace_back();
                } else {
                    owned_.emplace_back(value.data(), value.size());
                }
            }
        }

        std::size_t owned_index = 0;
        for (auto const& value : values) {
            if constexpr (std::convertible_to<element const&, const char*>) {
                pointers_.push_back(value);
            } else if constexpr (CStringLike<element>) {
                pointers_.push_back(value.c_str());
            } else {
                pointers_.push_back(owned_[owned_index].c_str());
                ++owned_index;
            }
        }
    }

    CStringArray(CStringArray const&) = delete;
    CStringArray& operator=(CStringArray const&) = delete;
    CStringArray(CStringArray&&) = delete;
    CStringArray& operator=(CStringArray&&) = delete;

    [[nodiscard]] const char* const* data() const noexcept { return pointers_.data(); }
    [[nodiscard]] std::size_t size() const noexcept { return pointers_.size(); }

private:
    std::vector<std::string> owned_;
    std::vector<const char*> pointers_;
};

[[nodiscard]] inline std::string_view string_view_of(const char* text) noexcept {
    return text == nullptr ? std::string_view{} : std::string_view{text};
}

[[nodiscard]] inline std::span<const std::byte> byte_span(const std::uint8_t* data, std::size_t size) noexcept {
    if (data == nullptr) {
        return {};
    }
    return {reinterpret_cast<const std::byte*>(data), size};
}

template <ByteSequence S> [[nodiscard]] inline const std::uint8_t* byte_data(S const& bytes) noexcept {
    return reinterpret_cast<const std::uint8_t*>(std::ranges::data(bytes));
}

template <ByteSequence S> [[nodiscard]] inline std::size_t byte_size(S const& bytes) noexcept {
    return static_cast<std::size_t>(std::ranges::size(bytes));
}

/// The data pointer of a JSON / textual argument.
template <TextLike T> [[nodiscard]] inline const std::uint8_t* text_data(T const& text) noexcept {
    if constexpr (std::convertible_to<T const&, const char*>) {
        const char* pointer = text;
        return reinterpret_cast<const std::uint8_t*>(pointer);
    } else if constexpr (ByteSequence<T>) {
        return byte_data(text);
    } else {
        return reinterpret_cast<const std::uint8_t*>(text.data());
    }
}

/// The length of a JSON / textual argument.
///
/// Character buffers lose a trailing NUL: string literals are `char[N]` and
/// would otherwise forward the terminator as part of the payload.
template <TextLike T> [[nodiscard]] inline std::size_t text_size(T const& text) noexcept {
    if constexpr (std::convertible_to<T const&, const char*>) {
        const char* pointer = text;
        return pointer == nullptr ? 0 : std::char_traits<char>::length(pointer);
    } else if constexpr (ByteSequence<T>) {
        std::size_t size = byte_size(text);
        if constexpr (std::same_as<std::remove_cv_t<std::ranges::range_value_t<T>>, char>) {
            const char* data = std::ranges::data(text);
            while (size != 0 && data[size - 1] == '\0') {
                --size;
            }
        }
        return size;
    } else {
        return static_cast<std::size_t>(text.size());
    }
}

} // namespace sysa::detail
