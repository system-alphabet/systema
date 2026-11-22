#pragma once

#include <concepts>
#include <cstddef>
#include <ranges>
#include <type_traits>

namespace sysa {

/// A string argument accepted by the C++ wrapper.
///
/// The concept deliberately avoids `std::string`: any type exposing the
/// STL-compatible `data()` / `size()` pair works, as does a plain
/// NUL-terminated `char` pointer.
template <typename T>
concept StringLike = std::convertible_to<T const&, const char*> || requires(T const& value) {
    { value.data() } -> std::convertible_to<const char*>;
    { value.size() } -> std::convertible_to<std::size_t>;
};

/// A string whose storage is already NUL-terminated.
///
/// Such arguments can be forwarded to the C API without a temporary copy.
template <typename T>
concept CStringLike = std::convertible_to<T const&, const char*> || requires(T const& value) {
    { value.c_str() } -> std::convertible_to<const char*>;
};

/// A contiguous sequence of single-byte values (`char`, `std::byte`,
/// `std::uint8_t`, ...).  `std::string`, `std::vector<std::byte>`,
/// `std::span<const std::uint8_t>` and native arrays all satisfy it.
template <typename T>
concept ByteSequence = std::ranges::contiguous_range<T> && requires {
    typename std::ranges::range_value_t<T>;
    requires sizeof(std::ranges::range_value_t<T>) == 1;
    requires std::integral<std::ranges::range_value_t<T>> ||
                 std::same_as<std::remove_cv_t<std::ranges::range_value_t<T>>, std::byte>;
};

/// Text accepted by the JSON entry points.
///
/// Either a `StringLike` string or a contiguous byte sequence.  Character
/// inputs are measured as C strings, so a string literal never carries its
/// trailing NUL into the C API.
template <typename T>
concept TextLike = StringLike<T> || ByteSequence<T>;

/// A sequence whose element type is exactly `Element`.
template <typename T, typename Element>
concept SequenceOf =
    std::ranges::input_range<T> && std::same_as<std::remove_cvref_t<std::ranges::range_value_t<T>>, Element>;

/// A sequence of strings, used for the C API's `const char *const *` arrays.
///
/// A forward range is required because the adapter walks the sequence twice:
/// once to copy non-NUL-terminated strings and once to build the pointer array.
template <typename T>
concept StringSequence = std::ranges::forward_range<T> && StringLike<std::ranges::range_value_t<T>>;

/// A string-to-string map, used for `UnitStatus` extensions.
template <typename T>
concept MapLike = requires(T const& map) {
    typename T::key_type;
    typename T::mapped_type;
    requires std::input_iterator<decltype(map.begin())>;
    requires std::input_iterator<decltype(map.end())>;
    { map.size() } -> std::convertible_to<std::size_t>;
    requires StringLike<typename T::key_type>;
    requires StringLike<typename T::mapped_type>;
};

} // namespace sysa
