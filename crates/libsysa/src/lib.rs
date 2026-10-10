//! Common library shared by System A and all System Workers.
//!
//! Provides:
//! - Protobuf-generated IPC message types
//! - IPC framing utilities (length-delimited codec over Unix sockets)
//! - Shared error types

pub mod controller;
pub mod event_bus;
pub mod finder;
pub mod ipc;
pub mod l10n;
pub mod logging;
pub mod mounts;
pub mod notify;
pub mod paths;
pub mod signals;
pub mod staging_admin;
pub mod unit_name;
pub mod unitstate_admin;
pub mod version;
pub mod worker_ipc;
pub mod proto {
    //! Generated protobuf types for the IPC protocol.
    //!
    //! prost emits one file per proto package, so the three sources under
    //! `proto/` (`common`, `workload`, `control`) land in a single flat
    //! module here.  The split is enforced at `protoc` level: each plane
    //! only imports `common.proto` and cannot name the other's types.
    include!(concat!(env!("OUT_DIR"), "/ipc.rs"));
}
