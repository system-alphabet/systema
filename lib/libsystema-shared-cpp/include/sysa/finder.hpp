#pragma once

#include "sysa/common.hpp"
#include "sysa/concepts.hpp"
#include "sysa/detail.hpp"
#include "sysa/error.hpp"

#include <cstddef>
#include <cstdint>
#include <systema_shared.h>
#include <utility>

namespace sysa
{

/// Finder-side staging client (`finder.register_units`,
/// `finder.commit_units`, `staging.query`).
///
/// The returned byte buffers are protobuf messages (`UnitRegistrationAck`,
/// `StagingQueryResult`); decode them with the definitions in `proto/`.
class Finder
{
public:
    Finder(Finder&& other) noexcept : handle_{std::exchange(other.handle_, nullptr)} {}

    Finder& operator=(Finder&& other) noexcept
    {
        if (this != &other)
        {
            reset();
            handle_ = std::exchange(other.handle_, nullptr);
        }
        return *this;
    }

    Finder(Finder const&) = delete;
    Finder& operator=(Finder const&) = delete;

    ~Finder() { reset(); }

    /// Create a finder client for the given System A socket path.
    template <StringLike SocketPath> [[nodiscard]] static Expected<Finder> create(SocketPath const& socket_path)
    {
        detail::CString path_text{socket_path};
        detail::clear_last_error();
        SysaFinder* handle = sysa_finder_new(path_text.get());
        if (handle == nullptr)
        {
            return std::unexpected(detail::take_last_error(Status::invalid_argument));
        }
        return Finder{handle};
    }

    /// Register a batch of units (JSON-serialized `unit_id -> UnitIR` map).
    template <StringLike Name, TextLike UnitsJson>
    [[nodiscard]] Expected<Bytes> register_units(Name const& name, UnitsJson const& units_json) const
    {
        detail::CString name_text{name};
        detail::clear_last_error();
        std::uint8_t* data = nullptr;
        std::size_t size = 0;
        const auto raw_status = sysa_finder_register_units(handle_, name_text.get(), detail::text_data(units_json),
                                                           detail::text_size(units_json), &data, &size);
        if (raw_status != SYSA_STATUS_OK)
        {
            return std::unexpected(detail::take_error(detail::to_status(raw_status)));
        }
        return Bytes{data, size};
    }

    /// Commit a staging area.
    template <StringLike Name> [[nodiscard]] Expected<Bytes> commit_units(Name const& name) const
    {
        detail::CString name_text{name};
        detail::clear_last_error();
        std::uint8_t* data = nullptr;
        std::size_t size = 0;
        const auto raw_status = sysa_finder_commit_units(handle_, name_text.get(), &data, &size);
        if (raw_status != SYSA_STATUS_OK)
        {
            return std::unexpected(detail::take_error(detail::to_status(raw_status)));
        }
        return Bytes{data, size};
    }

    /// Query a staging area.
    template <StringLike Name> [[nodiscard]] Expected<Bytes> query_staging(Name const& name) const
    {
        detail::CString name_text{name};
        detail::clear_last_error();
        std::uint8_t* data = nullptr;
        std::size_t size = 0;
        const auto raw_status = sysa_finder_query_staging(handle_, name_text.get(), &data, &size);
        if (raw_status != SYSA_STATUS_OK)
        {
            return std::unexpected(detail::take_error(detail::to_status(raw_status)));
        }
        return Bytes{data, size};
    }

    [[nodiscard]] SysaFinder* native_handle() const noexcept { return handle_; }

private:
    explicit Finder(SysaFinder* handle) noexcept : handle_{handle} {}

    void reset() noexcept
    {
        if (handle_ != nullptr)
        {
            sysa_finder_free(handle_);
            handle_ = nullptr;
        }
    }

    SysaFinder* handle_ = nullptr;
};

} // namespace sysa
