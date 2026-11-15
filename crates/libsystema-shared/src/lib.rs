//! The shared IPC layer: wire contract, model and worker-side runtime.
//!
//! This is the crate every System Worker (and every admin client) depends
//! on.  It deliberately has **no dependency on the System A server crate**:
//! the protobuf contract, envelope framing, the unit IR, the structured
//! error codes and the `UnitStatus` model all live here, together with the
//! worker runtime (`WorkerIpc`, the `UnitController` trait) and the finder /
//! staging / unit-state clients.
//!
//! `libsysa` re-exports this crate, so the historical `sysa::proto` /
//! `sysa::ipc` / `sysa::worker_ipc` paths keep working unchanged.

pub mod controller;
pub mod error_code;
pub mod finder;
pub mod ipc;
pub mod ir;
pub mod proto {
    //! Generated protobuf types for the IPC protocol.
    //!
    //! prost emits one file per proto package, so the three sources under
    //! `proto/` (`common`, `workload`, `control`) land in a single flat
    //! module here.  The split is enforced at `protoc` level: each plane
    //! only imports `common.proto` and cannot name the other's types.
    include!(concat!(env!("OUT_DIR"), "/ipc.rs"));
}
pub mod staging_admin;
pub mod unitstate_admin;
pub mod worker_ipc;
