#pragma once

#include "sysa/common.hpp"
#include "sysa/concepts.hpp"
#include "sysa/detail.hpp"
#include "sysa/error.hpp"

#include <cstddef>
#include <cstdint>
#include <optional>
#include <string_view>
#include <systema_shared.h>
#include <utility>

namespace sysa {

/// One staging area reported by `StagingAdmin::list`.
struct StagingEntry {
    std::uint32_t uid = 0;
    std::string_view name;
};

/// An owned list of `(uid, name)` staging areas.
class StagingList {
public:
    StagingList() noexcept = default;
    explicit StagingList(SysaStagingList* handle) noexcept : handle_{handle} {}

    ~StagingList() { reset(); }

    StagingList(StagingList&& other) noexcept : handle_{std::exchange(other.handle_, nullptr)} {}

    StagingList& operator=(StagingList&& other) noexcept {
        if (this != &other) {
            reset();
            handle_ = std::exchange(other.handle_, nullptr);
        }
        return *this;
    }

    StagingList(StagingList const&) = delete;
    StagingList& operator=(StagingList const&) = delete;

    [[nodiscard]] std::size_t size() const noexcept { return handle_ == nullptr ? 0 : sysa_staging_list_len(handle_); }

    [[nodiscard]] bool empty() const noexcept { return size() == 0; }

    /// Borrow entry `index`; `std::nullopt` when the index is out of range.
    [[nodiscard]] std::optional<StagingEntry> try_at(std::size_t index) const noexcept {
        if (handle_ == nullptr) {
            return std::nullopt;
        }
        const char* name = sysa_staging_list_name(handle_, index);
        if (name == nullptr) {
            return std::nullopt;
        }
        return StagingEntry{sysa_staging_list_uid(handle_, index), std::string_view{name}};
    }

private:
    void reset() noexcept {
        if (handle_ != nullptr) {
            sysa_staging_list_free(handle_);
            handle_ = nullptr;
        }
    }

    SysaStagingList* handle_ = nullptr;
};

/// Staging-area administration client (`admin.staging` on `control.socket`).
class StagingAdmin {
public:
    StagingAdmin(StagingAdmin&& other) noexcept : handle_{std::exchange(other.handle_, nullptr)} {}

    StagingAdmin& operator=(StagingAdmin&& other) noexcept {
        if (this != &other) {
            reset();
            handle_ = std::exchange(other.handle_, nullptr);
        }
        return *this;
    }

    StagingAdmin(StagingAdmin const&) = delete;
    StagingAdmin& operator=(StagingAdmin const&) = delete;

    ~StagingAdmin() { reset(); }

    /// Create a staging-admin client for the given control socket path.
    template <StringLike SocketPath> [[nodiscard]] static Expected<StagingAdmin> create(SocketPath const& socket_path) {
        detail::CString path_text{socket_path};
        detail::clear_last_error();
        SysaStagingAdmin* handle = sysa_staging_admin_new(path_text.get());
        if (handle == nullptr) {
            return std::unexpected(detail::take_last_error(Status::invalid_argument));
        }
        return StagingAdmin{handle};
    }

    /// List every staging area as `(uid, name)` pairs.
    [[nodiscard]] Expected<StagingList> list() const {
        detail::clear_last_error();
        SysaStagingList* list = nullptr;
        const auto raw_status = sysa_staging_admin_list(handle_, &list);
        if (raw_status != SYSA_STATUS_OK) {
            return std::unexpected(detail::take_error(detail::to_status(raw_status)));
        }
        return StagingList{list};
    }

    /// Query every staging area owned by a UID (protobuf `AdminStagingResult`).
    [[nodiscard]] Expected<Bytes> query_by_uid(std::uint32_t uid) const {
        detail::clear_last_error();
        std::uint8_t* data = nullptr;
        std::size_t size = 0;
        const auto raw_status = sysa_staging_admin_query_by_uid(handle_, uid, &data, &size);
        if (raw_status != SYSA_STATUS_OK) {
            return std::unexpected(detail::take_error(detail::to_status(raw_status)));
        }
        return Bytes{data, size};
    }

    /// Run a staging-area query by name (protobuf `AdminStagingResult`).
    template <StringLike Name> [[nodiscard]] Expected<Bytes> query_by_name(Name const& name) const {
        detail::CString name_text{name};
        detail::clear_last_error();
        std::uint8_t* data = nullptr;
        std::size_t size = 0;
        const auto raw_status = sysa_staging_admin_query_by_name(handle_, name_text.get(), &data, &size);
        if (raw_status != SYSA_STATUS_OK) {
            return std::unexpected(detail::take_error(detail::to_status(raw_status)));
        }
        return Bytes{data, size};
    }

    /// Run a staging-area query by `(uid, name)` (protobuf `AdminStagingResult`).
    template <StringLike Name> [[nodiscard]] Expected<Bytes> query(std::uint32_t uid, Name const& name) const {
        detail::CString name_text{name};
        detail::clear_last_error();
        std::uint8_t* data = nullptr;
        std::size_t size = 0;
        const auto raw_status = sysa_staging_admin_query(handle_, uid, name_text.get(), &data, &size);
        if (raw_status != SYSA_STATUS_OK) {
            return std::unexpected(detail::take_error(detail::to_status(raw_status)));
        }
        return Bytes{data, size};
    }

    /// Query every staging area (protobuf `AdminStagingResult`).
    [[nodiscard]] Expected<Bytes> query_all() const {
        detail::clear_last_error();
        std::uint8_t* data = nullptr;
        std::size_t size = 0;
        const auto raw_status = sysa_staging_admin_query_all(handle_, &data, &size);
        if (raw_status != SYSA_STATUS_OK) {
            return std::unexpected(detail::take_error(detail::to_status(raw_status)));
        }
        return Bytes{data, size};
    }

    /// Commit a staging area; an empty `name` commits every area of `uid`.
    template <StringLike Name> [[nodiscard]] Expected<Bytes> commit(std::uint32_t uid, Name const& name) const {
        detail::CString name_text{name};
        detail::clear_last_error();
        std::uint8_t* data = nullptr;
        std::size_t size = 0;
        const auto raw_status = sysa_staging_admin_commit(handle_, uid, name_text.get(), &data, &size);
        if (raw_status != SYSA_STATUS_OK) {
            return std::unexpected(detail::take_error(detail::to_status(raw_status)));
        }
        return Bytes{data, size};
    }

    [[nodiscard]] SysaStagingAdmin* native_handle() const noexcept { return handle_; }

private:
    explicit StagingAdmin(SysaStagingAdmin* handle) noexcept : handle_{handle} {}

    void reset() noexcept {
        if (handle_ != nullptr) {
            sysa_staging_admin_free(handle_);
            handle_ = nullptr;
        }
    }

    SysaStagingAdmin* handle_ = nullptr;
};

} // namespace sysa
