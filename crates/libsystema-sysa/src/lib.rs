//! Server-side IPC helpers used by System A.
//!
//! The wire contract (protobuf types, framing, IR, error codes), the unit
//! status model and the worker-side runtime all live in `libsystema-shared-rs`;
//! this crate depends on it and adds only what System A's accept paths need:
//! the version verdict and the pre-hello session gate.
//!
//! Dependency direction is strictly `libsystema-sysa` → `libsystema-shared-rs`.
//! The shared crate never depends on this one, so an external worker project
//! can take `libsystema-shared-rs` alone.

pub mod ipc;
