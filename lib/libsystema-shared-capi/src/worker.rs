//! C-owned worker handle: lifecycle, controller wiring and event sending.
//!
//! The handle starts one dedicated thread that owns a `current_thread` tokio
//! runtime running [`WorkerIpc`].  Blocking entry points (publish, subscribe,
//! send) only enqueue work on that runtime; `_wait` and `_free` join it.

use std::ffi::{c_char, c_void};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use prost::Message;
use systema_shared::proto::{Envelope, MethodCall, MethodResult};
use systema_shared::worker_ipc::{EventPublisher, ShutdownHandle, WorkerIpc};
use tokio::runtime::{Builder, Handle};

use crate::controller::{CController, SystemaController};
use crate::status::SystemaStatus;
use crate::unit_status::{unit_status_from_ptr, SystemaUnitStatus};

/// `on_shutdown` / `on_reconnect` hook.
pub type SystemaSyncHook = Option<unsafe extern "C" fn(userdata: *mut c_void)>;

/// `on_method_result(userdata, method, unit_name, success, unit_failure,
/// error, queued)`.
pub type SystemaMethodResultHook = Option<
    unsafe extern "C" fn(
        userdata: *mut c_void,
        method: *const c_char,
        unit_name: *const c_char,
        success: u8,
        unit_failure: u8,
        error: *const c_char,
        queued: u8,
    ),
>;

/// `on_envelope(userdata, request_id, source, method, payload, payload_len,
/// handled)` — write non-zero to `handled` when the envelope was consumed.
pub type SystemaEnvelopeHook = Option<
    unsafe extern "C" fn(
        userdata: *mut c_void,
        request_id: u64,
        source: *const c_char,
        method: *const c_char,
        payload: *const u8,
        payload_len: usize,
        handled: *mut u8,
    ) -> SystemaStatus,
>;

#[derive(Clone, Copy)]
struct SyncHook {
    callback: unsafe extern "C" fn(*mut c_void),
    userdata: *mut c_void,
}

impl SyncHook {
    unsafe fn call(&self) {
        (self.callback)(self.userdata);
    }
}

#[derive(Clone, Copy)]
struct MethodResultHook {
    callback:
        unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char, u8, u8, *const c_char, u8),
    userdata: *mut c_void,
}

impl MethodResultHook {
    unsafe fn call(&self, call: &MethodCall, result: &MethodResult, queued: bool) {
        let method = crate::ffi::sanitize(&call.method);
        let unit_name = crate::ffi::sanitize(&call.unit_name);
        let error = crate::ffi::sanitize(&result.error);
        (self.callback)(
            self.userdata,
            method.as_ptr(),
            unit_name.as_ptr(),
            u8::from(result.success),
            u8::from(result.unit_failure),
            error.as_ptr(),
            u8::from(queued),
        );
    }
}

#[derive(Clone, Copy)]
struct EnvelopeHook {
    callback: unsafe extern "C" fn(
        *mut c_void,
        u64,
        *const c_char,
        *const c_char,
        *const u8,
        usize,
        *mut u8,
    ) -> SystemaStatus,
    userdata: *mut c_void,
}

// The hooks are plain function pointers plus an opaque pointer; the worker
// thread is the only thread that invokes them.
unsafe impl Send for SyncHook {}
unsafe impl Sync for SyncHook {}
unsafe impl Send for MethodResultHook {}
unsafe impl Sync for MethodResultHook {}
unsafe impl Send for EnvelopeHook {}
unsafe impl Sync for EnvelopeHook {}

#[derive(Default)]
struct WorkerShared {
    publisher: Mutex<Option<EventPublisher>>,
    shutdown: Mutex<Option<ShutdownHandle>>,
}

/// Opaque, library-owned worker handle.
pub struct SystemaWorker {
    worker_id: String,
    unit_types: Vec<String>,
    socket_path: Option<String>,
    controller: Option<SystemaController>,
    supports_unit_define: bool,
    supports_resource_prepare: bool,
    on_shutdown: Option<SyncHook>,
    on_reconnect: Option<SyncHook>,
    on_method_result: Option<MethodResultHook>,
    on_envelope: Option<EnvelopeHook>,
    shared: Arc<WorkerShared>,
    handle: Option<Handle>,
    thread: Option<JoinHandle<anyhow::Result<()>>>,
    run_result: Option<SystemaStatus>,
}

fn read_string_array(
    items: *const *const c_char,
    count: usize,
    what: &str,
) -> Result<Vec<String>, SystemaStatus> {
    if count == 0 {
        return Ok(Vec::new());
    }
    if items.is_null() {
        return Err(crate::error::fail(
            SystemaStatus::InvalidArgument,
            format!("{what} is null"),
        ));
    }
    let mut values = Vec::with_capacity(count);
    for index in 0..count {
        let entry = unsafe { *items.add(index) };
        values.push(unsafe { crate::ffi::require_str(entry, what) }?.to_string());
    }
    Ok(values)
}

fn publisher(worker: &SystemaWorker) -> Option<EventPublisher> {
    worker
        .shared
        .publisher
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone()
}

fn handle_envelope(hook: Option<EnvelopeHook>, envelope: &Envelope) -> anyhow::Result<bool> {
    let Some(hook) = hook else {
        return Ok(false);
    };
    let source = crate::ffi::sanitize(&envelope.source);
    let method = crate::ffi::sanitize(&envelope.method);
    let mut handled = 0u8;
    let status = unsafe {
        (hook.callback)(
            hook.userdata,
            envelope.request_id,
            source.as_ptr(),
            method.as_ptr(),
            envelope.payload.as_ptr(),
            envelope.payload.len(),
            &mut handled,
        )
    };
    if status != SystemaStatus::Ok {
        anyhow::bail!("the envelope handler returned {status:?}");
    }
    Ok(handled != 0)
}

fn build_ipc(worker: &SystemaWorker, socket_path: String) -> WorkerIpc {
    let unit_types: Vec<&str> = worker.unit_types.iter().map(String::as_str).collect();
    let mut ipc = WorkerIpc::new(&worker.worker_id, &unit_types).with_socket_path(socket_path);
    if worker.supports_unit_define {
        ipc = ipc.supports_unit_define();
    }
    if worker.supports_resource_prepare {
        ipc = ipc.supports_resource_prepare();
    }
    if let Some(hook) = worker.on_shutdown {
        ipc = ipc.on_shutdown(move || async move {
            let _ = tokio::task::spawn_blocking(move || unsafe { hook.call() }).await;
        });
    }
    if let Some(hook) = worker.on_reconnect {
        ipc = ipc.on_reconnect(move || async move {
            let _ = tokio::task::spawn_blocking(move || unsafe { hook.call() }).await;
        });
    }
    if let Some(hook) = worker.on_method_result {
        ipc = ipc.on_method_result(move |call, result, queued| {
            let _callback = crate::runtime::CallbackGuard::enter();
            unsafe { hook.call(call, result, queued) };
        });
    }
    ipc
}

/// Create a worker handle.
#[no_mangle]
pub extern "C" fn systema_shared_worker_new(
    worker_id: *const c_char,
    unit_types: *const *const c_char,
    unit_type_count: usize,
) -> *mut SystemaWorker {
    ffi_try!(std::ptr::null_mut(), {
        let worker_id = match unsafe { crate::ffi::require_str(worker_id, "worker_id") } {
            Ok(value) => value.to_string(),
            Err(_) => return std::ptr::null_mut(),
        };
        let unit_types = match read_string_array(unit_types, unit_type_count, "unit_types") {
            Ok(values) => values,
            Err(_) => return std::ptr::null_mut(),
        };
        Box::into_raw(Box::new(SystemaWorker {
            worker_id,
            unit_types,
            socket_path: None,
            controller: None,
            supports_unit_define: false,
            supports_resource_prepare: false,
            on_shutdown: None,
            on_reconnect: None,
            on_method_result: None,
            on_envelope: None,
            shared: Arc::new(WorkerShared::default()),
            handle: None,
            thread: None,
            run_result: None,
        }))
    })
}

/// Set the System A socket path (required before `_start`).
#[no_mangle]
pub extern "C" fn systema_shared_worker_set_socket_path(
    worker: *mut SystemaWorker,
    socket_path: *const c_char,
) -> SystemaStatus {
    ffi_try!(SystemaStatus::Internal, {
        if worker.is_null() {
            return crate::error::fail(SystemaStatus::InvalidArgument, "worker is null");
        }
        let path = match unsafe { crate::ffi::require_str(socket_path, "socket_path") } {
            Ok(value) => value.to_string(),
            Err(status) => return status,
        };
        unsafe { &mut *worker }.socket_path = Some(path);
        SystemaStatus::Ok
    })
}

/// Copy the controller table into the worker (required before `_start`).
#[no_mangle]
pub extern "C" fn systema_shared_worker_set_controller(
    worker: *mut SystemaWorker,
    controller: *const SystemaController,
) -> SystemaStatus {
    ffi_try!(SystemaStatus::Internal, {
        if worker.is_null() || controller.is_null() {
            return crate::error::fail(SystemaStatus::InvalidArgument, "argument is null");
        }
        unsafe { &mut *worker }.controller = Some(unsafe { *controller });
        SystemaStatus::Ok
    })
}

/// Register a hook that runs on the still-open connection before `worker.exit`.
#[no_mangle]
pub extern "C" fn systema_shared_worker_set_on_shutdown(
    worker: *mut SystemaWorker,
    callback: SystemaSyncHook,
    userdata: *mut c_void,
) -> SystemaStatus {
    ffi_try!(SystemaStatus::Internal, {
        if worker.is_null() {
            return crate::error::fail(SystemaStatus::InvalidArgument, "worker is null");
        }
        let hook = callback.map(|callback| SyncHook { callback, userdata });
        unsafe { &mut *worker }.on_shutdown = hook;
        SystemaStatus::Ok
    })
}

/// Register a hook that runs after every successful (re)connection.
#[no_mangle]
pub extern "C" fn systema_shared_worker_set_on_reconnect(
    worker: *mut SystemaWorker,
    callback: SystemaSyncHook,
    userdata: *mut c_void,
) -> SystemaStatus {
    ffi_try!(SystemaStatus::Internal, {
        if worker.is_null() {
            return crate::error::fail(SystemaStatus::InvalidArgument, "worker is null");
        }
        let hook = callback.map(|callback| SyncHook { callback, userdata });
        unsafe { &mut *worker }.on_reconnect = hook;
        SystemaStatus::Ok
    })
}

/// Register a hook that observes every deferred method result.
#[no_mangle]
pub extern "C" fn systema_shared_worker_set_on_method_result(
    worker: *mut SystemaWorker,
    callback: SystemaMethodResultHook,
    userdata: *mut c_void,
) -> SystemaStatus {
    ffi_try!(SystemaStatus::Internal, {
        if worker.is_null() {
            return crate::error::fail(SystemaStatus::InvalidArgument, "worker is null");
        }
        let hook = callback.map(|callback| MethodResultHook { callback, userdata });
        unsafe { &mut *worker }.on_method_result = hook;
        SystemaStatus::Ok
    })
}

/// Register a handler for envelopes that are neither method calls nor state
/// sync requests.
#[no_mangle]
pub extern "C" fn systema_shared_worker_set_on_envelope(
    worker: *mut SystemaWorker,
    callback: SystemaEnvelopeHook,
    userdata: *mut c_void,
) -> SystemaStatus {
    ffi_try!(SystemaStatus::Internal, {
        if worker.is_null() {
            return crate::error::fail(SystemaStatus::InvalidArgument, "worker is null");
        }
        let hook = callback.map(|callback| EnvelopeHook { callback, userdata });
        unsafe { &mut *worker }.on_envelope = hook;
        SystemaStatus::Ok
    })
}

/// Declare support for the `unit.define` protocol.
#[no_mangle]
pub extern "C" fn systema_shared_worker_enable_unit_define(
    worker: *mut SystemaWorker,
) -> SystemaStatus {
    ffi_try!(SystemaStatus::Internal, {
        if worker.is_null() {
            return crate::error::fail(SystemaStatus::InvalidArgument, "worker is null");
        }
        unsafe { &mut *worker }.supports_unit_define = true;
        SystemaStatus::Ok
    })
}

/// Declare support for the `resource.prepare` protocol.
#[no_mangle]
pub extern "C" fn systema_shared_worker_enable_resource_prepare(
    worker: *mut SystemaWorker,
) -> SystemaStatus {
    ffi_try!(SystemaStatus::Internal, {
        if worker.is_null() {
            return crate::error::fail(SystemaStatus::InvalidArgument, "worker is null");
        }
        unsafe { &mut *worker }.supports_resource_prepare = true;
        SystemaStatus::Ok
    })
}

/// Start the worker thread.  Requires a socket path and a controller.
#[no_mangle]
pub extern "C" fn systema_shared_worker_start(worker: *mut SystemaWorker) -> SystemaStatus {
    ffi_try!(SystemaStatus::Internal, {
        if worker.is_null() {
            return crate::error::fail(SystemaStatus::InvalidArgument, "worker is null");
        }
        let worker = unsafe { &mut *worker };
        if worker.thread.is_some() || worker.run_result.is_some() {
            return crate::error::fail(
                SystemaStatus::AlreadyRunning,
                "the worker is already running",
            );
        }
        let Some(socket_path) = worker.socket_path.clone() else {
            return crate::error::fail(SystemaStatus::InvalidArgument, "socket path is not set");
        };
        let Some(controller) = worker.controller else {
            return crate::error::fail(SystemaStatus::InvalidArgument, "controller is not set");
        };

        let runtime = match Builder::new_current_thread().enable_all().build() {
            Ok(runtime) => runtime,
            Err(error) => {
                return crate::error::fail(
                    SystemaStatus::Internal,
                    format!("cannot create the worker runtime: {error}"),
                )
            }
        };
        let handle = runtime.handle().clone();
        let ipc = build_ipc(worker, socket_path);
        *worker
            .shared
            .shutdown
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(ipc.shutdown_handle());

        let shared = worker.shared.clone();
        let gate = Arc::new(Mutex::new(()));
        let on_envelope = worker.on_envelope;
        let thread_name = format!("{}-ipc", worker.worker_id);
        let spawned = thread::Builder::new().name(thread_name).spawn(move || {
            crate::runtime::mark_runtime_thread();
            let factory_shared = shared.clone();
            let result = runtime.block_on(async move {
                let factory = move |publisher: EventPublisher| {
                    *factory_shared
                        .publisher
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner()) = Some(publisher.clone());
                    CController::new(controller, gate.clone())
                };
                ipc.run(factory, move |envelope, _publisher| {
                    handle_envelope(on_envelope, envelope)
                })
                .await
            });
            *shared
                .publisher
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()) = None;
            result
        });

        match spawned {
            Ok(thread) => {
                worker.handle = Some(handle);
                worker.thread = Some(thread);
                SystemaStatus::Ok
            }
            Err(error) => {
                *worker
                    .shared
                    .shutdown
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner()) = None;
                crate::error::fail(
                    SystemaStatus::Internal,
                    format!("cannot spawn the worker thread: {error}"),
                )
            }
        }
    })
}

/// Ask the worker to leave gracefully; safe to call more than once.
#[no_mangle]
pub extern "C" fn systema_shared_worker_stop(
    worker: *mut SystemaWorker,
    reason: *const c_char,
) -> SystemaStatus {
    ffi_try!(SystemaStatus::Internal, {
        if worker.is_null() {
            return crate::error::fail(SystemaStatus::InvalidArgument, "worker is null");
        }
        let reason = if reason.is_null() {
            "stop".to_string()
        } else {
            match unsafe { crate::ffi::require_str(reason, "reason") } {
                Ok(value) => value.to_string(),
                Err(status) => return status,
            }
        };
        let handle = unsafe { &*worker }
            .shared
            .shutdown
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        if let Some(handle) = handle {
            handle.shutdown(reason);
        }
        SystemaStatus::Ok
    })
}

/// Join the worker thread once and report its run result.
#[no_mangle]
pub extern "C" fn systema_shared_worker_wait(
    worker: *mut SystemaWorker,
    out: *mut SystemaStatus,
) -> SystemaStatus {
    ffi_try!(SystemaStatus::Internal, {
        if worker.is_null() {
            return crate::error::fail(SystemaStatus::InvalidArgument, "worker is null");
        }
        if crate::runtime::in_library_runtime() || crate::runtime::in_callback() {
            return crate::error::fail(
                SystemaStatus::Reentrant,
                "cannot wait for a worker from inside a callback",
            );
        }
        let worker = unsafe { &mut *worker };
        let status = match worker.thread.take() {
            Some(thread) => {
                let status = match thread.join() {
                    Ok(Ok(())) => SystemaStatus::Ok,
                    Ok(Err(error)) => crate::error::report(error),
                    Err(_) => {
                        crate::error::fail(SystemaStatus::Internal, "the worker thread panicked")
                    }
                };
                worker.run_result = Some(status);
                status
            }
            None => match worker.run_result {
                Some(status) => status,
                None => {
                    return crate::error::fail(
                        SystemaStatus::InvalidArgument,
                        "the worker has not been started",
                    )
                }
            },
        };
        if !out.is_null() {
            unsafe { *out = status };
        }
        status
    })
}

/// Release a worker handle, stopping and joining it first when it is running.
#[no_mangle]
pub extern "C" fn systema_shared_worker_free(worker: *mut SystemaWorker) -> SystemaStatus {
    ffi_try!(SystemaStatus::Internal, {
        if worker.is_null() {
            return SystemaStatus::Ok;
        }
        if crate::runtime::in_library_runtime() || crate::runtime::in_callback() {
            return crate::error::fail(
                SystemaStatus::Reentrant,
                "cannot free a worker from inside a callback",
            );
        }
        {
            let worker_ref = unsafe { &*worker };
            let handle = worker_ref
                .shared
                .shutdown
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .clone();
            if let Some(handle) = handle {
                handle.shutdown("free");
            }
        }
        let mut worker = unsafe { Box::from_raw(worker) };
        if let Some(thread) = worker.thread.take() {
            let _ = thread.join();
        }
        *worker
            .shared
            .publisher
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = None;
        drop(worker);
        SystemaStatus::Ok
    })
}

/// Publish a `unit.state_update` snapshot.
#[no_mangle]
pub extern "C" fn systema_shared_worker_publish_unit_states(
    worker: *mut SystemaWorker,
    units: *const *const SystemaUnitStatus,
    count: usize,
    full_snapshot: u8,
) -> SystemaStatus {
    ffi_try!(SystemaStatus::Internal, {
        if worker.is_null() {
            return crate::error::fail(SystemaStatus::InvalidArgument, "worker is null");
        }
        if count > 0 && units.is_null() {
            return crate::error::fail(SystemaStatus::InvalidArgument, "units is null");
        }
        let worker = unsafe { &*worker };
        let Some(handle) = worker.handle.clone() else {
            return crate::error::fail(SystemaStatus::NotRunning, "the worker is not running");
        };
        let Some(publisher) = publisher(worker) else {
            return crate::error::fail(SystemaStatus::NotRunning, "the worker is not connected");
        };
        let mut statuses = Vec::with_capacity(count);
        for index in 0..count {
            let entry = unsafe { *units.add(index) };
            match unsafe { unit_status_from_ptr(entry) } {
                Some(status) => statuses.push(status),
                None => {
                    return crate::error::fail(
                        SystemaStatus::InvalidArgument,
                        "units contains a null entry",
                    )
                }
            }
        }
        handle.spawn(async move {
            publisher.publish_unit_state_update(statuses, full_snapshot != 0);
        });
        SystemaStatus::Ok
    })
}

/// Subscribe to `event.publish` notifications for the given units.
#[no_mangle]
pub extern "C" fn systema_shared_worker_subscribe_units(
    worker: *mut SystemaWorker,
    unit_names: *const *const c_char,
    count: usize,
) -> SystemaStatus {
    change_subscription(worker, unit_names, count, true)
}

/// Cancel `event.publish` subscriptions for the given units.
#[no_mangle]
pub extern "C" fn systema_shared_worker_unsubscribe_units(
    worker: *mut SystemaWorker,
    unit_names: *const *const c_char,
    count: usize,
) -> SystemaStatus {
    change_subscription(worker, unit_names, count, false)
}

fn change_subscription(
    worker: *mut SystemaWorker,
    unit_names: *const *const c_char,
    count: usize,
    subscribe: bool,
) -> SystemaStatus {
    ffi_try!(SystemaStatus::Internal, {
        if worker.is_null() {
            return crate::error::fail(SystemaStatus::InvalidArgument, "worker is null");
        }
        let names = match read_string_array(unit_names, count, "unit_names") {
            Ok(values) => values,
            Err(status) => return status,
        };
        let worker = unsafe { &*worker };
        let Some(handle) = worker.handle.clone() else {
            return crate::error::fail(SystemaStatus::NotRunning, "the worker is not running");
        };
        let Some(publisher) = publisher(worker) else {
            return crate::error::fail(SystemaStatus::NotRunning, "the worker is not connected");
        };
        handle.spawn(async move {
            if subscribe {
                publisher.subscribe_units(&names);
            } else {
                publisher.unsubscribe_units(&names);
            }
        });
        SystemaStatus::Ok
    })
}

/// Send a fire-and-forget envelope with a raw payload.
#[no_mangle]
pub extern "C" fn systema_shared_worker_send_envelope_bytes(
    worker: *mut SystemaWorker,
    method: *const c_char,
    payload: *const u8,
    payload_len: usize,
) -> SystemaStatus {
    ffi_try!(SystemaStatus::Internal, {
        if worker.is_null() {
            return crate::error::fail(SystemaStatus::InvalidArgument, "worker is null");
        }
        let method = match unsafe { crate::ffi::require_str(method, "method") } {
            Ok(value) => value.to_string(),
            Err(status) => return status,
        };
        let payload = match unsafe { crate::ffi::require_bytes(payload, payload_len, "payload") } {
            Ok(value) => value.to_vec(),
            Err(status) => return status,
        };
        let worker = unsafe { &*worker };
        let Some(handle) = worker.handle.clone() else {
            return crate::error::fail(SystemaStatus::NotRunning, "the worker is not running");
        };
        let Some(publisher) = publisher(worker) else {
            return crate::error::fail(SystemaStatus::NotRunning, "the worker is not connected");
        };
        handle.spawn(async move {
            publisher.send_envelope_bytes(&method, payload);
        });
        SystemaStatus::Ok
    })
}

/// Send a reply envelope with a raw payload.
#[no_mangle]
pub extern "C" fn systema_shared_worker_send_reply(
    worker: *mut SystemaWorker,
    request_id: u64,
    method: *const c_char,
    payload: *const u8,
    payload_len: usize,
) -> SystemaStatus {
    ffi_try!(SystemaStatus::Internal, {
        if worker.is_null() {
            return crate::error::fail(SystemaStatus::InvalidArgument, "worker is null");
        }
        if request_id == 0 {
            return crate::error::fail(SystemaStatus::InvalidArgument, "request_id is zero");
        }
        let method = match unsafe { crate::ffi::require_str(method, "method") } {
            Ok(value) => value.to_string(),
            Err(status) => return status,
        };
        let payload = match unsafe { crate::ffi::require_bytes(payload, payload_len, "payload") } {
            Ok(value) => value.to_vec(),
            Err(status) => return status,
        };
        let worker = unsafe { &*worker };
        let Some(handle) = worker.handle.clone() else {
            return crate::error::fail(SystemaStatus::NotRunning, "the worker is not running");
        };
        let Some(publisher) = publisher(worker) else {
            return crate::error::fail(SystemaStatus::NotRunning, "the worker is not connected");
        };
        handle.spawn(async move {
            publisher.send_reply(request_id, &method, RawPayload(payload));
        });
        SystemaStatus::Ok
    })
}

/// Ask System A where a unit's cgroup lives; the answer is a protobuf-encoded
/// `UnitCgroupResult`.
#[no_mangle]
pub extern "C" fn systema_shared_worker_query_unit_cgroup(
    worker: *mut SystemaWorker,
    unit_name: *const c_char,
    timeout_ms: u32,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> SystemaStatus {
    ffi_try!(SystemaStatus::Internal, {
        if worker.is_null() {
            return crate::error::fail(SystemaStatus::InvalidArgument, "worker is null");
        }
        let unit_name = match unsafe { crate::ffi::require_str(unit_name, "unit_name") } {
            Ok(value) => value.to_string(),
            Err(status) => return status,
        };
        let Some(publisher) = publisher(unsafe { &*worker }) else {
            return crate::error::fail(SystemaStatus::NotRunning, "the worker is not running");
        };
        let timeout = if timeout_ms == 0 {
            crate::runtime::DEFAULT_WAIT
        } else {
            Duration::from_millis(u64::from(timeout_ms))
        };
        let result = match crate::runtime::block_on(
            async move {
                publisher
                    .query_unit_cgroup(&unit_name, timeout)
                    .await
                    .map_err(anyhow::Error::from)
            },
            timeout + Duration::from_secs(5),
        ) {
            Ok(result) => result,
            Err(status) => return status,
        };
        crate::ffi::check(crate::ffi::write_out_bytes(
            out,
            out_len,
            result.encode_to_vec(),
        ))
    })
}

/// An encode-only prost message carrying raw bytes unchanged.
#[derive(Debug, Default)]
struct RawPayload(Vec<u8>);

impl Message for RawPayload {
    fn encode_raw(&self, buf: &mut impl prost::bytes::BufMut) {
        prost::bytes::BufMut::put_slice(buf, &self.0);
    }

    fn merge_field(
        &mut self,
        _tag: u32,
        _wire_type: prost::encoding::WireType,
        _buf: &mut impl prost::bytes::Buf,
        _ctx: prost::encoding::DecodeContext,
    ) -> Result<(), prost::DecodeError> {
        Err(prost::DecodeError::new(
            "a raw payload cannot be decoded through prost",
        ))
    }

    fn encoded_len(&self) -> usize {
        self.0.len()
    }

    fn clear(&mut self) {
        self.0.clear();
    }
}
