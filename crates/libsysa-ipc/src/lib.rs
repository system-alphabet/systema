//! IPC kernel shared by System A and all System Workers: the protobuf
//! contract, the envelope framing over Unix sockets, and the worker /
//! admin / finder IPC machinery on both ends.
//!
//! Split out of `libsysa` (re-exported there for tree-wide path
//! stability); binary infrastructure (logging bootstrap, gettext wiring,
//! CLI output) and server-side orchestration (event bus, notify
//! broadcast, mount policy) remain in `libsysa`.
//!
//! Provides:
//! - Protobuf-generated IPC message types
//! - IPC framing utilities (length-delimited codec over Unix sockets)
//! - Worker-IPC loop and the unit controller trait
//! - Finder / admin client sessions
//! - Shared error types

pub mod controller;
pub mod finder;
pub mod ipc;
pub mod l10n;
pub mod paths;
pub mod proto {
    //! Generated protobuf types for the IPC protocol.
    //!
    //! prost emits one file per proto package, so the three sources under
    //! `proto/` (`common`, `workload`, `control`) land in a single flat
    //! module here.  The split is enforced at `protoc` level: each plane
    //! only imports `common.proto` and cannot name the other's types.
    include!(concat!(env!("OUT_DIR"), "/ipc.rs"));
}
pub mod signals;
pub mod staging_admin;
pub mod unitstate_admin;
pub mod worker_ipc;
