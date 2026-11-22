//! C wrapper around the control-port (`manager.*`) client.
//!
//! One handle owns one established control session.  Requests block the
//! calling thread on the library runtime; events are pulled with
//! [`sysa_control_client_next_event`] instead of callbacks, so no C function
//! pointer is ever invoked from a library thread.

use std::ffi::{c_char, CString};
use std::sync::Arc;
use std::time::Duration;

use prost::Message;
use systema_shared::manager::{ControlClient, ControlEventKind, ControlEventStream};
use systema_shared::proto::{
    EnqueueJobRequest, GetUnitByInvocationRequest, GetUnitByPidRequest, ListJobsRequest,
    ListSnapshotsRequest, LoadUnitRequest, UnitSnapshotRequest,
};
use tokio::sync::Mutex;

use crate::status::SysaStatus;

/// A zero timeout waits for an event indefinitely, bounded only by this
/// (effectively infinite) outer budget.
const INDEFINITE_WAIT: Duration = Duration::from_secs(365 * 24 * 60 * 60);

/// The control events System A pushes to a bridge session.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SysaControlEventKind {
    /// Full `UnitSnapshot`; the unit's configuration or active state changed.
    UnitChanged = 0,
    /// Full `UnitSnapshot`; a unit was loaded into memory.
    UnitNew = 1,
    /// `UnitRemovedEvent`; a unit was unloaded / removed from memory.
    UnitRemoved = 2,
    /// Raw `UnitCgroupMetrics`.
    UnitMetrics = 3,
    /// `JobEvent`; a job was newly enqueued.
    JobNew = 4,
    /// `JobEvent`; a job reached a terminal state (`result` is set).
    JobCompleted = 5,
}

impl SysaControlEventKind {
    fn from_kind(kind: ControlEventKind) -> Self {
        match kind {
            ControlEventKind::UnitChanged => SysaControlEventKind::UnitChanged,
            ControlEventKind::UnitNew => SysaControlEventKind::UnitNew,
            ControlEventKind::UnitRemoved => SysaControlEventKind::UnitRemoved,
            ControlEventKind::UnitMetrics => SysaControlEventKind::UnitMetrics,
            ControlEventKind::JobNew => SysaControlEventKind::JobNew,
            ControlEventKind::JobCompleted => SysaControlEventKind::JobCompleted,
        }
    }
}

/// Opaque, library-owned control-port client.
pub struct SysaControlClient {
    client: Arc<ControlClient>,
    events: Arc<Mutex<ControlEventStream>>,
    flavor: CString,
}

/// What one `next_event` call observed.
enum NextEvent {
    Event(systema_shared::manager::ControlEvent),
    Closed,
    TimedOut,
}

/// Run one pre-encoded request on the library runtime.
fn raw_call(
    handle: &SysaControlClient,
    method: &str,
    payload: Vec<u8>,
) -> Result<Vec<u8>, SysaStatus> {
    let client = Arc::clone(&handle.client);
    let method = method.to_string();
    crate::runtime::block_on(
        async move { client.call_raw(&method, &payload).await },
        crate::runtime::DEFAULT_WAIT,
    )
}

/// Copy a reply payload into the caller's output pair.
fn write_reply(
    out: *mut *mut u8,
    out_len: *mut usize,
    reply: Result<Vec<u8>, SysaStatus>,
) -> SysaStatus {
    match reply {
        Ok(data) => crate::ffi::check(crate::ffi::write_out_bytes(out, out_len, data)),
        Err(status) => status,
    }
}

/// Connect to `socket_path`, perform the version + `manager.hello` handshake,
/// and return a control client.
///
/// `worker_id` may be null; a non-null value identifies a supervised bridge so
/// the session handler can broadcast `WORKER_READY`.
#[no_mangle]
pub extern "C" fn sysa_control_client_new(
    socket_path: *const c_char,
    flavor: *const c_char,
    worker_id: *const c_char,
    out: *mut *mut SysaControlClient,
) -> SysaStatus {
    ffi_try!(SysaStatus::Internal, {
        if out.is_null() {
            return crate::error::fail(SysaStatus::InvalidArgument, "out is null");
        }
        let path = match unsafe { crate::ffi::require_str(socket_path, "socket_path") } {
            Ok(value) => value.to_string(),
            Err(status) => return status,
        };
        let flavor = match unsafe { crate::ffi::require_str(flavor, "flavor") } {
            Ok(value) => value.to_string(),
            Err(status) => return status,
        };
        let worker_id = if worker_id.is_null() {
            None
        } else {
            match unsafe { crate::ffi::require_str(worker_id, "worker_id") } {
                Ok(value) => Some(value.to_string()),
                Err(status) => return status,
            }
        };

        let flavor_text = crate::ffi::sanitize(&flavor);
        let client = match crate::runtime::block_on(
            async move { ControlClient::connect(&path, &flavor, worker_id.as_deref()).await },
            crate::runtime::DEFAULT_WAIT,
        ) {
            Ok(client) => client,
            Err(status) => return status,
        };
        let events = client.events();
        let handle = SysaControlClient {
            client: Arc::new(client),
            events,
            flavor: flavor_text,
        };
        unsafe { *out = Box::into_raw(Box::new(handle)) };
        SysaStatus::Ok
    })
}

/// The bridge identity reported during the handshake.
///
/// The pointer is owned by `client` and stays valid until it is freed.
#[no_mangle]
pub extern "C" fn sysa_control_client_flavor(client: *const SysaControlClient) -> *const c_char {
    ffi_try!(std::ptr::null(), {
        if client.is_null() {
            crate::error::fail(SysaStatus::InvalidArgument, "client is null");
            return std::ptr::null();
        }
        unsafe { &*client }.flavor.as_ptr()
    })
}

/// Issue one `manager.*` request with a pre-encoded protobuf payload.
///
/// The reply payload (protobuf bytes) is written to `out` / `out_len` and
/// must be released with `sysa_bytes_free`.  A `manager.error` reply becomes a
/// failure status with its message in the thread-local error state.
#[no_mangle]
pub extern "C" fn sysa_control_client_call(
    client: *const SysaControlClient,
    method: *const c_char,
    request: *const u8,
    request_len: usize,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> SysaStatus {
    ffi_try!(SysaStatus::Internal, {
        if client.is_null() {
            return crate::error::fail(SysaStatus::InvalidArgument, "client is null");
        }
        let method = match unsafe { crate::ffi::require_str(method, "method") } {
            Ok(value) => value.to_string(),
            Err(status) => return status,
        };
        let payload = match unsafe { crate::ffi::require_bytes(request, request_len, "request") } {
            Ok(value) => value.to_vec(),
            Err(status) => return status,
        };
        let handle = unsafe { &*client };
        write_reply(out, out_len, raw_call(handle, &method, payload))
    })
}

/// Enqueue a start / stop / restart / reload job (protobuf
/// `EnqueueJobResult`).
///
/// `reload_if_possible` is a C boolean: zero is false, any other value true.
#[no_mangle]
pub extern "C" fn sysa_control_client_enqueue(
    client: *const SysaControlClient,
    name: *const c_char,
    job_type: *const c_char,
    mode: *const c_char,
    reload_if_possible: u8,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> SysaStatus {
    ffi_try!(SysaStatus::Internal, {
        if client.is_null() {
            return crate::error::fail(SysaStatus::InvalidArgument, "client is null");
        }
        let name = match unsafe { crate::ffi::require_str(name, "name") } {
            Ok(value) => value.to_string(),
            Err(status) => return status,
        };
        let job_type = match unsafe { crate::ffi::require_str(job_type, "job_type") } {
            Ok(value) => value.to_string(),
            Err(status) => return status,
        };
        let mode = match unsafe { crate::ffi::require_str(mode, "mode") } {
            Ok(value) => value.to_string(),
            Err(status) => return status,
        };
        let request = EnqueueJobRequest {
            name,
            job_type,
            mode,
            reload_if_possible: reload_if_possible != 0,
        };
        let handle = unsafe { &*client };
        write_reply(
            out,
            out_len,
            raw_call(handle, "manager.enqueue", request.encode_to_vec()),
        )
    })
}

/// Fetch the full current unit set (protobuf `ListSnapshotsResult`).
#[no_mangle]
pub extern "C" fn sysa_control_client_list_snapshots(
    client: *const SysaControlClient,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> SysaStatus {
    ffi_try!(SysaStatus::Internal, {
        if client.is_null() {
            return crate::error::fail(SysaStatus::InvalidArgument, "client is null");
        }
        let handle = unsafe { &*client };
        write_reply(
            out,
            out_len,
            raw_call(
                handle,
                "manager.list_snapshots",
                ListSnapshotsRequest::default().encode_to_vec(),
            ),
        )
    })
}

/// Fetch one unit's snapshot (protobuf `UnitSnapshotResult`).
#[no_mangle]
pub extern "C" fn sysa_control_client_unit_snapshot(
    client: *const SysaControlClient,
    name: *const c_char,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> SysaStatus {
    ffi_try!(SysaStatus::Internal, {
        if client.is_null() {
            return crate::error::fail(SysaStatus::InvalidArgument, "client is null");
        }
        let name = match unsafe { crate::ffi::require_str(name, "name") } {
            Ok(value) => value.to_string(),
            Err(status) => return status,
        };
        let request = UnitSnapshotRequest { name };
        let handle = unsafe { &*client };
        write_reply(
            out,
            out_len,
            raw_call(handle, "manager.unit_snapshot", request.encode_to_vec()),
        )
    })
}

/// List the current running jobs (protobuf `ListJobsResult`).
#[no_mangle]
pub extern "C" fn sysa_control_client_list_jobs(
    client: *const SysaControlClient,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> SysaStatus {
    ffi_try!(SysaStatus::Internal, {
        if client.is_null() {
            return crate::error::fail(SysaStatus::InvalidArgument, "client is null");
        }
        let handle = unsafe { &*client };
        write_reply(
            out,
            out_len,
            raw_call(
                handle,
                "manager.list_jobs",
                ListJobsRequest::default().encode_to_vec(),
            ),
        )
    })
}

/// Ask System A to load one unit (protobuf `LoadUnitResult`).
#[no_mangle]
pub extern "C" fn sysa_control_client_load_unit(
    client: *const SysaControlClient,
    name: *const c_char,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> SysaStatus {
    ffi_try!(SysaStatus::Internal, {
        if client.is_null() {
            return crate::error::fail(SysaStatus::InvalidArgument, "client is null");
        }
        let name = match unsafe { crate::ffi::require_str(name, "name") } {
            Ok(value) => value.to_string(),
            Err(status) => return status,
        };
        let request = LoadUnitRequest { name };
        let handle = unsafe { &*client };
        write_reply(
            out,
            out_len,
            raw_call(handle, "manager.load_unit", request.encode_to_vec()),
        )
    })
}

/// Resolve a PID to a canonical unit name (protobuf `GetUnitByPidResult`).
#[no_mangle]
pub extern "C" fn sysa_control_client_get_unit_by_pid(
    client: *const SysaControlClient,
    pid: u32,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> SysaStatus {
    ffi_try!(SysaStatus::Internal, {
        if client.is_null() {
            return crate::error::fail(SysaStatus::InvalidArgument, "client is null");
        }
        let request = GetUnitByPidRequest { pid };
        let handle = unsafe { &*client };
        write_reply(
            out,
            out_len,
            raw_call(handle, "manager.get_unit_by_pid", request.encode_to_vec()),
        )
    })
}

/// Resolve an invocation ID to a canonical unit name (protobuf
/// `GetUnitByPidResult`).
#[no_mangle]
pub extern "C" fn sysa_control_client_get_unit_by_invocation(
    client: *const SysaControlClient,
    invocation_id: *const c_char,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> SysaStatus {
    ffi_try!(SysaStatus::Internal, {
        if client.is_null() {
            return crate::error::fail(SysaStatus::InvalidArgument, "client is null");
        }
        let invocation_id = match unsafe { crate::ffi::require_str(invocation_id, "invocation_id") }
        {
            Ok(value) => value.to_string(),
            Err(status) => return status,
        };
        let request = GetUnitByInvocationRequest { invocation_id };
        let handle = unsafe { &*client };
        write_reply(
            out,
            out_len,
            raw_call(
                handle,
                "manager.get_unit_by_invocation",
                request.encode_to_vec(),
            ),
        )
    })
}

/// Wait for the next server-pushed event.
///
/// `timeout_ms` of zero waits indefinitely; a positive value bounds the wait
/// and yields [`SysaStatus::Timeout`] when no event arrives.  The payload is
/// protobuf bytes matching `kind` and must be released with
/// `sysa_bytes_free`.
#[no_mangle]
pub extern "C" fn sysa_control_client_next_event(
    client: *const SysaControlClient,
    timeout_ms: u64,
    out_kind: *mut SysaControlEventKind,
    out_payload: *mut *mut u8,
    out_len: *mut usize,
) -> SysaStatus {
    ffi_try!(SysaStatus::Internal, {
        if client.is_null() {
            return crate::error::fail(SysaStatus::InvalidArgument, "client is null");
        }
        if out_kind.is_null() {
            return crate::error::fail(SysaStatus::InvalidArgument, "out_kind is null");
        }
        let wait = if timeout_ms == 0 {
            INDEFINITE_WAIT
        } else {
            Duration::from_millis(timeout_ms)
        };
        let outer = wait.saturating_add(Duration::from_secs(5));
        let events = Arc::clone(&unsafe { &*client }.events);

        // The timeout covers both lock acquisition and the wait, and lives
        // inside the future, so an expired wait cancels `recv` instead of
        // leaving a task behind to swallow the next event.
        let outcome = match crate::runtime::block_on(
            async move {
                match tokio::time::timeout(wait, async {
                    let mut rx = events.lock().await;
                    rx.recv().await
                })
                .await
                {
                    Ok(Some(event)) => Ok(NextEvent::Event(event)),
                    Ok(None) => Ok(NextEvent::Closed),
                    Err(_) => Ok(NextEvent::TimedOut),
                }
            },
            outer,
        ) {
            Ok(outcome) => outcome,
            Err(status) => return status,
        };

        match outcome {
            NextEvent::Event(event) => {
                unsafe { *out_kind = SysaControlEventKind::from_kind(event.kind) };
                crate::ffi::check(crate::ffi::write_out_bytes(
                    out_payload,
                    out_len,
                    event.envelope.payload,
                ))
            }
            NextEvent::Closed => {
                crate::error::fail(SysaStatus::Disconnected, "the control event stream closed")
            }
            NextEvent::TimedOut => {
                crate::error::fail(SysaStatus::Timeout, "no control event arrived in time")
            }
        }
    })
}

/// Release a control client.
#[no_mangle]
pub extern "C" fn sysa_control_client_free(client: *mut SysaControlClient) {
    ffi_try!((), {
        if !client.is_null() {
            drop(unsafe { Box::from_raw(client) });
        }
    })
}
