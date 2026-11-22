#pragma once

#include "sysa/common.hpp"
#include "sysa/concepts.hpp"
#include "sysa/detail.hpp"
#include "sysa/error.hpp"

#include <cstddef>
#include <cstdint>
#include <functional>
#include <span>
#include <string_view>
#include <systema_shared.h>
#include <utility>

namespace sysa {

/// One cached unit streamed by `UnitStateAdmin::list`.
///
/// The name and JSON payload are borrowed for the duration of the callback.
struct UnitStateEntry {
    std::string_view name;
    std::span<const std::byte> json;
};

/// Summary returned after an `admin.unitstate` stream ends.
struct UnitStateResult {
    std::uint32_t total = 0;
};

/// Unit-state administration client (`admin.unitstate` on `control.socket`).
class UnitStateAdmin {
public:
    using EntryHandler = std::function<Result(UnitStateEntry const& entry)>;

    UnitStateAdmin(UnitStateAdmin&& other) noexcept : handle_{std::exchange(other.handle_, nullptr)} {}

    UnitStateAdmin& operator=(UnitStateAdmin&& other) noexcept {
        if (this != &other) {
            reset();
            handle_ = std::exchange(other.handle_, nullptr);
        }
        return *this;
    }

    UnitStateAdmin(UnitStateAdmin const&) = delete;
    UnitStateAdmin& operator=(UnitStateAdmin const&) = delete;

    ~UnitStateAdmin() { reset(); }

    /// Create a unit-state admin client for the given control socket path.
    template <StringLike SocketPath>
    [[nodiscard]] static Expected<UnitStateAdmin> create(SocketPath const& socket_path) {
        detail::CString path_text{socket_path};
        detail::clear_last_error();
        SysaUnitStateAdmin* handle = sysa_unitstate_admin_new(path_text.get());
        if (handle == nullptr) {
            return std::unexpected(detail::take_last_error(Status::invalid_argument));
        }
        return UnitStateAdmin{handle};
    }

    /// Stream every cached unit to `handler`; returning an error aborts the
    /// iteration.
    [[nodiscard]] Expected<UnitStateResult> list(EntryHandler handler) const {
        if (!handler) {
            return std::unexpected(Error{Status::invalid_argument, ErrorCode::none, "the unit-state handler is empty"});
        }

        detail::clear_last_error();
        std::uint32_t total = 0;
        const auto raw_status = sysa_unitstate_admin_list(handle_, &entry_thunk, &handler, &total);
        if (raw_status != SYSA_STATUS_OK) {
            return std::unexpected(detail::take_error(detail::to_status(raw_status)));
        }
        return UnitStateResult{total};
    }

    [[nodiscard]] SysaUnitStateAdmin* native_handle() const noexcept { return handle_; }

private:
    explicit UnitStateAdmin(SysaUnitStateAdmin* handle) noexcept : handle_{handle} {}

    static SysaStatus entry_thunk(void* userdata, const char* name, const std::uint8_t* json,
                                  std::size_t json_len) noexcept {
        auto* handler = static_cast<EntryHandler*>(userdata);
        if (handler == nullptr || !*handler) {
            return SYSA_STATUS_INVALID_ARGUMENT;
        }
        try {
            auto result = (*handler)(UnitStateEntry{detail::string_view_of(name), detail::byte_span(json, json_len)});
            return result ? SYSA_STATUS_OK : detail::error_to_c_status(result.error());
        } catch (...) {
            return SYSA_STATUS_INTERNAL;
        }
    }

    void reset() noexcept {
        if (handle_ != nullptr) {
            sysa_unitstate_admin_free(handle_);
            handle_ = nullptr;
        }
    }

    SysaUnitStateAdmin* handle_ = nullptr;
};

} // namespace sysa
