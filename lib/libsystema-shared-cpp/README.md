# libsystema-shared-cpp

Header-only C++26 wrapper around the `libsystema-shared-c` C ABI.  It covers
the IPC-facing data model (`ConfigMap`, `UnitIR`, `UnitStatus`), the clients
(`Finder`, `StagingAdmin`, `UnitStateAdmin`, `ControlClient`) and the worker
side (`Worker`, `Controller` and its hooks).

Everything lives in namespace `sysa`; include the umbrella header:

```cpp
#include <sysa/sysa.hpp>
```

## Requirements

- xmake 3.x and a C++26 compiler (the project is tested with g++ 16).
- Nightly cargo: the workspace uses `cargo-features = ["different-binary-name"]`.
- `libsystema-shared-c` is built by xmake through cargo; no separate step is
  needed.  The crate is declared as the project-local xmake package
  `libsystema-shared-c`, so consumers pull it in with `add_packages` like any
  other dependency.

## Build and test

```sh
xmake f -m debug -y
xmake build -y sysa-smoke
xmake run sysa-smoke
```

The C ABI is installed as the xmake package
`build/.packages/l/libsystema-shared-c/...`, which holds the generated C
header, the shared library and its rpath; cargo's own target directory is
`build/cargo/{debug,release}`.  Neither touches the repository's top-level
`target/`, and `xmake` defaults to the release profile — the matching cargo
profile is selected automatically.

Packages are only resolved while configuring, so after editing the Rust crates
run `xmake f --require=y` to rebuild and reinstall the C ABI; a plain
`xmake build` links the library installed by the last configure.  `rm -rf build`
drops the cargo target directory and the installed package for a full rebuild.

To use the library from another xmake target in this repository:

```lua
target("my-tool")
    set_kind("binary")
    add_files("src/main.cpp")
    add_deps("libsystema-shared-cpp")
```

The dependency propagates the C++ and C include directories, the link directory
and the shared-library rpath.  `libsystema-shared-cpp` is an `INTERFACE`
(`headeronly`) target; there is nothing to compile or link for the wrapper
itself.

## API notes

- Fallible calls return `sysa::Expected<T>` (`std::expected<T, sysa::Error>`);
  the wrapper never throws and exceptions cannot cross the C ABI.
- Owned C allocations are adopted without copying: `sysa::OwnedString`,
  `sysa::Bytes`, `sysa::StringList` and `sysa::StagingList` are move-only RAII
  types.  Borrowed views into `UnitStatus` stay valid until the status is
  modified or freed.
- String arguments are constrained by `sysa::StringLike` (any type with an
  STL-compatible `data()` / `size()`, or a NUL-terminated `char` pointer).
  Byte payloads use `sysa::ByteSequence`, string collections use
  `sysa::StringSequence`, and JSON entry points accept `sysa::TextLike`.
  Passing a string literal never forwards its trailing NUL.
- `sysa::Controller` handlers run on the C API's blocking callback threads and
  are serialized per worker.  They may block, but must not call
  `Worker::wait` or destroy the worker.
- `sysa::UnitType` mirrors `UnitIR`'s type string: a closed `sysa::UnitKind`
  for the standard kinds plus the verbatim name for producer-specific types.
  `UnitIR::unit_kind()` returns the parsed value, while `unit_type()` keeps the
  raw string.
- `sysa::EventPublisher` and `sysa::ShutdownHandle` are copyable, non-owning
  handles obtained from `Worker::publisher()` / `Worker::shutdown_handle()`.
  They stay valid until the `Worker` is destroyed, so a controller handler can
  capture a copy; dropping one does nothing, and only `shutdown()` requests an
  exit (repeated calls are harmless).
- `Worker::wait()` returns `sysa::Expected<sysa::Status>`: the value is the
  worker's run result, and a join or runtime failure comes back as the error.
  `sysa::MethodResultInfo::failure_kind()` distinguishes a unit's own failure
  (`sysa::FailureKind::unit`) from a task fault (`sysa::FailureKind::task`).
- `sysa::ControlClient` performs the version + `manager.hello` handshake in
  `create`.  Requests block the calling thread; server-pushed events are pulled
  with `next_event(timeout_ms)` (zero waits indefinitely) and returned as
  `sysa::ControlEvent`, so no callback runs on a library thread.
- Wire payloads returned by `Finder`, `StagingAdmin`, `ControlClient` and
  `Worker::query_unit_cgroup` are protobuf messages defined in `proto/`; the
  wrapper exposes them as `sysa::Bytes`.
