#pragma once

#include "sysa/common.hpp"
#include "sysa/concepts.hpp"
#include "sysa/controller.hpp"
#include "sysa/detail.hpp"
#include "sysa/error.hpp"
#include "sysa/publisher.hpp"
#include "sysa/shutdown_handle.hpp"
#include "sysa/unit_status.hpp"

#include <cstddef>
#include <cstdint>
#include <functional>
#include <memory>
#include <ranges>
#include <span>
#include <string_view>
#include <systema_shared.h>
#include <utility>
#include <vector>

namespace sysa {

/// Whether a failed method result is the unit's own failure or a task fault.
///
/// Mirrors `libsystema_shared::worker_ipc::UnitFailure`: `unit` means the unit
/// itself failed (for example a oneshot that exited non-zero), while `task`
/// means the operation malfunctioned.
enum class FailureKind : std::uint8_t {
    task,
    unit
};

/// A deferred method result observed by `Worker::on_method_result`.
struct MethodResultInfo {
    std::string_view method;
    std::string_view unit_name;
    bool success = false;
    bool unit_failure = false;
    std::string_view error;
    bool queued = false;

    [[nodiscard]] FailureKind failure_kind() const noexcept {
        return unit_failure ? FailureKind::unit : FailureKind::task;
    }
};

/// A custom envelope observed by `Worker::on_envelope`.
///
/// The string views and the payload span are borrowed for the duration of the
/// callback.
struct EnvelopeInfo {
    std::uint64_t request_id = 0;
    std::string_view source;
    std::string_view method;
    std::span<const std::byte> payload;
};

/// A worker handle plus the controller and hooks it dispatches to.
///
/// The C API owns a dedicated worker thread.  `Worker` owns the C handle and
/// keeps the C++ callback storage at a stable address for as long as the
/// worker can call into it.
class Worker {
public:
    using Hook = std::function<void()>;
    using MethodResultHandler = std::function<void(MethodResultInfo const& result)>;
    using EnvelopeHandler = std::function<Expected<bool>(EnvelopeInfo const& envelope)>;

    Worker(Worker&& other) noexcept
        : handle_{std::exchange(other.handle_, nullptr)}, runtime_{std::move(other.runtime_)} {}

    Worker& operator=(Worker&& other) noexcept {
        if (this != &other) {
            reset();
            handle_ = std::exchange(other.handle_, nullptr);
            runtime_ = std::move(other.runtime_);
        }
        return *this;
    }

    Worker(Worker const&) = delete;
    Worker& operator=(Worker const&) = delete;

    ~Worker() { reset(); }

    /// Create a worker handle for `worker_id`, managing `unit_types`.
    template <StringLike WorkerId, StringSequence UnitTypes>
    [[nodiscard]] static Expected<Worker> create(WorkerId const& worker_id, UnitTypes const& unit_types) {
        detail::CString id_text{worker_id};
        detail::CStringArray types{unit_types};
        auto runtime = std::make_unique<Runtime>();

        detail::clear_last_error();
        SysaWorker* handle = sysa_worker_new(id_text.get(), types.data(), types.size());
        if (handle == nullptr) {
            return std::unexpected(detail::take_last_error(Status::invalid_argument));
        }
        return Worker{handle, std::move(runtime)};
    }

    /// Set the System A socket path (required before `start`).
    template <StringLike SocketPath> Result set_socket_path(SocketPath const& socket_path) {
        detail::CString path_text{socket_path};
        detail::clear_last_error();
        const auto raw_status = sysa_worker_set_socket_path(handle_, path_text.get());
        return detail::status_to_result(raw_status);
    }

    /// Copy the controller table into the worker (required before `start`).
    Result set_controller(Controller controller) {
        if (runtime_->controller_configured) {
            return std::unexpected(detail::internal_error("the controller is already set"));
        }

        runtime_->controller = std::move(controller);
        runtime_->controller_configured = true;
        auto table = runtime_->controller.make_table();

        detail::clear_last_error();
        const auto raw_status = sysa_worker_set_controller(handle_, &table);
        if (raw_status != SYSA_STATUS_OK) {
            runtime_->controller_configured = false;
            runtime_->controller = Controller{};
            return std::unexpected(detail::take_error(detail::to_status(raw_status)));
        }
        return {};
    }

    /// Declare support for the `unit.define` protocol.
    Result enable_unit_define() {
        detail::clear_last_error();
        return detail::status_to_result(sysa_worker_enable_unit_define(handle_));
    }

    /// Declare support for the `resource.prepare` protocol.
    Result enable_resource_prepare() {
        detail::clear_last_error();
        return detail::status_to_result(sysa_worker_enable_resource_prepare(handle_));
    }

    /// Run `hook` on the still-open connection before `worker.exit`.
    Result on_shutdown(Hook hook) {
        runtime_->shutdown_hook = std::move(hook);
        detail::clear_last_error();
        const auto raw_status = sysa_worker_set_on_shutdown(
            handle_, runtime_->shutdown_hook ? &Worker::shutdown_thunk : nullptr, runtime_.get());
        return detail::status_to_result(raw_status);
    }

    /// Run `hook` after every successful (re)connection.
    Result on_reconnect(Hook hook) {
        runtime_->reconnect_hook = std::move(hook);
        detail::clear_last_error();
        const auto raw_status = sysa_worker_set_on_reconnect(
            handle_, runtime_->reconnect_hook ? &Worker::reconnect_thunk : nullptr, runtime_.get());
        return detail::status_to_result(raw_status);
    }

    /// Observe every deferred method result.
    Result on_method_result(MethodResultHandler handler) {
        runtime_->method_result_hook = std::move(handler);
        detail::clear_last_error();
        const auto raw_status = sysa_worker_set_on_method_result(
            handle_, runtime_->method_result_hook ? &Worker::method_result_thunk : nullptr, runtime_.get());
        return detail::status_to_result(raw_status);
    }

    /// Handle envelopes that are neither method calls nor state-sync requests.
    Result on_envelope(EnvelopeHandler handler) {
        runtime_->envelope_hook = std::move(handler);
        detail::clear_last_error();
        const auto raw_status = sysa_worker_set_on_envelope(
            handle_, runtime_->envelope_hook ? &Worker::envelope_thunk : nullptr, runtime_.get());
        return detail::status_to_result(raw_status);
    }

    /// Start the worker thread.  Requires a socket path and a controller.
    Result start() {
        if (!runtime_->controller_configured) {
            return std::unexpected(detail::internal_error("the controller is not set"));
        }
        detail::clear_last_error();
        return detail::status_to_result(sysa_worker_start(handle_));
    }

    /// Ask the worker to leave gracefully.
    Result stop() { return stop(std::string_view{"stop"}); }

    template <StringLike Reason> Result stop(Reason const& reason) { return shutdown_handle().shutdown(reason); }

    /// A copyable handle that requests this worker's exit from other tasks.
    [[nodiscard]] ShutdownHandle shutdown_handle() const noexcept { return ShutdownHandle{handle_}; }

    /// A copyable publisher that controller handlers can keep.
    [[nodiscard]] EventPublisher publisher() const noexcept { return EventPublisher{handle_}; }

    /// Join the worker thread once and report its run result.
    [[nodiscard]] Expected<Status> wait() {
        SysaStatus run_status = SYSA_STATUS_OK;
        detail::clear_last_error();
        const auto raw_status = sysa_worker_wait(handle_, &run_status);
        if (raw_status != SYSA_STATUS_OK) {
            return std::unexpected(detail::take_error(detail::to_status(raw_status)));
        }
        return detail::to_status(run_status);
    }

    /// Publish a `unit.state_update` snapshot.
    template <SequenceOf<UnitStatus> Statuses>
    Result publish_unit_states(Statuses const& statuses, bool full_snapshot) const {
        return publisher().publish_unit_states(statuses, full_snapshot);
    }

    /// Subscribe to `event.publish` notifications for the given units.
    template <StringSequence Names> Result subscribe_units(Names const& names) const {
        return publisher().subscribe_units(names);
    }

    /// Cancel `event.publish` subscriptions for the given units.
    template <StringSequence Names> Result unsubscribe_units(Names const& names) const {
        return publisher().unsubscribe_units(names);
    }

    /// Send a fire-and-forget envelope with a raw payload.
    template <StringLike Method, ByteSequence Payload>
    Result send_envelope_bytes(Method const& method, Payload const& payload) const {
        return publisher().send_envelope_bytes(method, payload);
    }

    /// Send a reply envelope with a raw payload.
    template <StringLike Method, ByteSequence Payload>
    Result send_reply(std::uint64_t request_id, Method const& method, Payload const& payload) const {
        return publisher().send_reply(request_id, method, payload);
    }

    /// Ask System A where a unit's cgroup lives (protobuf `UnitCgroupResult`).
    template <StringLike UnitName>
    [[nodiscard]] Expected<Bytes> query_unit_cgroup(UnitName const& unit_name, std::uint32_t timeout_ms = 0) const {
        return publisher().query_unit_cgroup(unit_name, timeout_ms);
    }

    [[nodiscard]] SysaWorker* native_handle() const noexcept { return handle_; }

private:
    struct Runtime {
        Controller controller;
        bool controller_configured = false;
        std::function<void()> shutdown_hook;
        std::function<void()> reconnect_hook;
        std::function<void(MethodResultInfo const&)> method_result_hook;
        std::function<Expected<bool>(EnvelopeInfo const&)> envelope_hook;
    };

    Worker(SysaWorker* handle, std::unique_ptr<Runtime> runtime) noexcept
        : handle_{handle}, runtime_{std::move(runtime)} {}

    static void shutdown_thunk(void* userdata) noexcept {
        auto* runtime = static_cast<Runtime*>(userdata);
        if (runtime == nullptr || !runtime->shutdown_hook) {
            return;
        }
        try {
            runtime->shutdown_hook();
        } catch (...) {
        }
    }

    static void reconnect_thunk(void* userdata) noexcept {
        auto* runtime = static_cast<Runtime*>(userdata);
        if (runtime == nullptr || !runtime->reconnect_hook) {
            return;
        }
        try {
            runtime->reconnect_hook();
        } catch (...) {
        }
    }

    static void method_result_thunk(void* userdata, const char* method, const char* unit_name, std::uint8_t success,
                                    std::uint8_t unit_failure, const char* error, std::uint8_t queued) noexcept {
        auto* runtime = static_cast<Runtime*>(userdata);
        if (runtime == nullptr || !runtime->method_result_hook) {
            return;
        }
        try {
            runtime->method_result_hook(
                MethodResultInfo{detail::string_view_of(method), detail::string_view_of(unit_name), success != 0,
                                 unit_failure != 0, detail::string_view_of(error), queued != 0});
        } catch (...) {
        }
    }

    static SysaStatus envelope_thunk(void* userdata, std::uint64_t request_id, const char* source, const char* method,
                                     const std::uint8_t* payload, std::size_t payload_len,
                                     std::uint8_t* handled) noexcept {
        auto* runtime = static_cast<Runtime*>(userdata);
        if (runtime == nullptr || !runtime->envelope_hook) {
            return SYSA_STATUS_INVALID_ARGUMENT;
        }
        try {
            auto result = runtime->envelope_hook(EnvelopeInfo{request_id, detail::string_view_of(source),
                                                              detail::string_view_of(method),
                                                              detail::byte_span(payload, payload_len)});
            if (!result) {
                return detail::error_to_c_status(result.error());
            }
            if (handled != nullptr) {
                *handled = *result ? 1 : 0;
            }
            return SYSA_STATUS_OK;
        } catch (...) {
            return SYSA_STATUS_INTERNAL;
        }
    }

    void reset() noexcept {
        if (handle_ != nullptr) {
            const auto raw_status = sysa_worker_free(handle_);
            handle_ = nullptr;
            if (raw_status != SYSA_STATUS_OK) {
                // The C API refuses to free a worker from inside its own callback.
                // Keep the callback storage alive so the still-running worker never
                // calls into freed memory.
                (void)runtime_.release();
                sysa_clear_error();
                return;
            }
            sysa_clear_error();
        }
        runtime_.reset();
    }

    SysaWorker* handle_ = nullptr;
    std::unique_ptr<Runtime> runtime_;
};

} // namespace sysa
