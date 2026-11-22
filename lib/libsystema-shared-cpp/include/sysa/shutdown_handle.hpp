#pragma once

#include "sysa/concepts.hpp"
#include "sysa/detail.hpp"
#include "sysa/error.hpp"

#include <string_view>
#include <systema_shared.h>

namespace sysa {

/// A copyable request to stop a running worker.
///
/// Mirrors `libsystema_shared::worker_ipc::ShutdownHandle`: the handle is a
/// non-owning view of the worker, dropping it does not stop anything, and a
/// shutdown is requested only by calling `shutdown`.  Repeated calls are
/// harmless.
class ShutdownHandle {
public:
    ShutdownHandle() noexcept = default;
    explicit ShutdownHandle(SysaWorker* worker) noexcept : worker_{worker} {}

    [[nodiscard]] explicit operator bool() const noexcept { return worker_ != nullptr; }
    [[nodiscard]] SysaWorker* native_handle() const noexcept { return worker_; }

    /// Request a graceful exit with the default reason (`stop`).
    Result shutdown() const { return shutdown(std::string_view{"stop"}); }

    /// Request a graceful exit; `reason` names the exit in logs and `worker.exit`.
    template <StringLike Reason> Result shutdown(Reason const& reason) const {
        detail::CString reason_text{reason};
        detail::clear_last_error();
        return detail::status_to_result(sysa_worker_stop(worker_, reason_text.get()));
    }

private:
    SysaWorker* worker_ = nullptr;
};

} // namespace sysa
