#pragma once

#include "sysa/concepts.hpp"

#include <cstddef>
#include <cstdint>
#include <optional>
#include <span>
#include <string>
#include <string_view>
#include <systema_shared.h>
#include <utility>
#include <vector>

namespace sysa {

/// An owned, NUL-terminated string returned by the C API.
///
/// The C API hands out `char *` allocations released with
/// `sysa_string_free`; this type adopts that allocation directly, so
/// reading an accessor does not copy the text.  It satisfies `StringLike` and
/// `CStringLike` and can be passed straight back to another wrapper call.
class OwnedString {
public:
    OwnedString() noexcept = default;
    explicit OwnedString(char* text) noexcept : text_{text} {}

    ~OwnedString() { reset(); }

    OwnedString(OwnedString&& other) noexcept : text_{std::exchange(other.text_, nullptr)} {}

    OwnedString& operator=(OwnedString&& other) noexcept {
        if (this != &other) {
            reset();
            text_ = std::exchange(other.text_, nullptr);
        }
        return *this;
    }

    OwnedString(OwnedString const&) = delete;
    OwnedString& operator=(OwnedString const&) = delete;

    [[nodiscard]] const char* data() const noexcept { return text_; }
    [[nodiscard]] const char* c_str() const noexcept { return text_; }

    [[nodiscard]] std::size_t size() const noexcept {
        return text_ == nullptr ? 0 : std::char_traits<char>::length(text_);
    }

    [[nodiscard]] bool empty() const noexcept { return text_ == nullptr || *text_ == '\0'; }

    /// Borrow the text; valid while this object is alive and unmodified.
    [[nodiscard]] std::string_view view() const noexcept {
        return text_ == nullptr ? std::string_view{} : std::string_view{text_};
    }

    [[nodiscard]] std::string to_string() const { return std::string{view()}; }

    /// Relinquish ownership of the raw allocation.
    [[nodiscard]] char* release() noexcept { return std::exchange(text_, nullptr); }

private:
    void reset() noexcept {
        if (text_ != nullptr) {
            sysa_string_free(text_);
            text_ = nullptr;
        }
    }

    char* text_ = nullptr;
};

/// An owned byte buffer returned by the C API.
///
/// The buffer is exposed as a span / string view without copying.
class Bytes {
public:
    Bytes() noexcept = default;
    explicit Bytes(std::uint8_t* data, std::size_t size) noexcept : data_{data}, size_{size} {}

    ~Bytes() { reset(); }

    Bytes(Bytes&& other) noexcept : data_{std::exchange(other.data_, nullptr)}, size_{std::exchange(other.size_, 0)} {}

    Bytes& operator=(Bytes&& other) noexcept {
        if (this != &other) {
            reset();
            data_ = std::exchange(other.data_, nullptr);
            size_ = std::exchange(other.size_, 0);
        }
        return *this;
    }

    Bytes(Bytes const&) = delete;
    Bytes& operator=(Bytes const&) = delete;

    [[nodiscard]] const std::uint8_t* data() const noexcept { return data_; }
    [[nodiscard]] std::size_t size() const noexcept { return size_; }
    [[nodiscard]] bool empty() const noexcept { return size_ == 0; }

    /// Borrow the payload as bytes.
    [[nodiscard]] std::span<const std::byte> bytes() const noexcept {
        if (data_ == nullptr) {
            return {};
        }
        return {reinterpret_cast<const std::byte*>(data_), size_};
    }

    /// Borrow the payload as text (JSON payloads are UTF-8).
    [[nodiscard]] std::string_view text() const noexcept {
        if (data_ == nullptr) {
            return {};
        }
        return {reinterpret_cast<const char*>(data_), size_};
    }

    [[nodiscard]] std::vector<std::uint8_t> to_vector() const {
        if (data_ == nullptr) {
            return {};
        }
        return {data_, data_ + size_};
    }

    [[nodiscard]] std::uint8_t* release() noexcept {
        std::uint8_t* data = data_;
        data_ = nullptr;
        size_ = 0;
        return data;
    }

private:
    void reset() noexcept {
        if (data_ != nullptr) {
            sysa_bytes_free(data_, size_);
            data_ = nullptr;
            size_ = 0;
        }
    }

    std::uint8_t* data_ = nullptr;
    std::size_t size_ = 0;
};

/// An owned list of strings returned by the C API.
class StringList {
public:
    StringList() noexcept = default;
    explicit StringList(SysaStringList* handle) noexcept : handle_{handle} {}

    ~StringList() { reset(); }

    StringList(StringList&& other) noexcept : handle_{std::exchange(other.handle_, nullptr)} {}

    StringList& operator=(StringList&& other) noexcept {
        if (this != &other) {
            reset();
            handle_ = std::exchange(other.handle_, nullptr);
        }
        return *this;
    }

    StringList(StringList const&) = delete;
    StringList& operator=(StringList const&) = delete;

    [[nodiscard]] std::size_t size() const noexcept { return handle_ == nullptr ? 0 : sysa_string_list_len(handle_); }

    [[nodiscard]] bool empty() const noexcept { return size() == 0; }

    /// Borrow string `index`; an out-of-range index yields an empty view.
    [[nodiscard]] std::string_view operator[](std::size_t index) const noexcept { return at(index); }

    [[nodiscard]] std::string_view at(std::size_t index) const noexcept {
        if (handle_ == nullptr) {
            return {};
        }
        const char* text = sysa_string_list_get(handle_, index);
        return text == nullptr ? std::string_view{} : std::string_view{text};
    }

    /// Borrow string `index`; `std::nullopt` when the index is out of range.
    [[nodiscard]] std::optional<std::string_view> try_at(std::size_t index) const noexcept {
        if (handle_ == nullptr) {
            return std::nullopt;
        }
        const char* text = sysa_string_list_get(handle_, index);
        return text == nullptr ? std::nullopt : std::optional<std::string_view>{text};
    }

    [[nodiscard]] std::vector<std::string> to_vector() const {
        std::vector<std::string> values;
        values.reserve(size());
        for (std::size_t index = 0; index < size(); ++index) {
            values.emplace_back(at(index));
        }
        return values;
    }

private:
    void reset() noexcept {
        if (handle_ != nullptr) {
            sysa_string_list_free(handle_);
            handle_ = nullptr;
        }
    }

    SysaStringList* handle_ = nullptr;
};

} // namespace sysa
