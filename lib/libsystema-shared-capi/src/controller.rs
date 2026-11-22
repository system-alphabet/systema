//! The C controller table: function pointers the worker runtime dispatches to.
//!
//! Every callback runs on a blocking pool thread (never on the worker's async
//! runtime), serialized by one per-worker mutex, so the C `userdata` does not
//! have to be thread-safe.

use std::ffi::{c_char, c_void};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail};
use systema_shared::controller::{UnitController, UnitStatus};

use crate::status::SysaStatus;
use crate::unit_status::SysaUnitStatus;

/// `status(userdata, unit_name, out)` — fill `*out` with a status created by
/// `sysa_unit_status_new`; the library takes ownership.
pub type SysaStatusCallback = Option<
    unsafe extern "C" fn(
        userdata: *mut c_void,
        unit_name: *const c_char,
        out: *mut *mut SysaUnitStatus,
    ) -> SysaStatus,
>;

/// `start(userdata, unit_name, config, config_len, invocation_id)`.
pub type SysaStartCallback = Option<
    unsafe extern "C" fn(
        userdata: *mut c_void,
        unit_name: *const c_char,
        config: *const u8,
        config_len: usize,
        invocation_id: *const c_char,
    ) -> SysaStatus,
>;

/// `stop(userdata, unit_name)`.
pub type SysaStopCallback =
    Option<unsafe extern "C" fn(userdata: *mut c_void, unit_name: *const c_char) -> SysaStatus>;

/// `restart(userdata, unit_name, config, config_len, invocation_id)`.
pub type SysaRestartCallback = SysaStartCallback;

/// `reload(userdata, unit_name, config, config_len)`.
pub type SysaReloadCallback = Option<
    unsafe extern "C" fn(
        userdata: *mut c_void,
        unit_name: *const c_char,
        config: *const u8,
        config_len: usize,
    ) -> SysaStatus,
>;

/// `sync_state(userdata, out, count)` — fill `out` with an array allocated by
/// `sysa_unit_status_array_alloc`; the library takes ownership.
pub type SysaSyncStateCallback = Option<
    unsafe extern "C" fn(
        userdata: *mut c_void,
        out: *mut *mut *mut SysaUnitStatus,
        count: *mut usize,
    ) -> SysaStatus,
>;

/// The controller table a worker dispatches method calls to.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SysaController {
    pub userdata: *mut c_void,
    pub status: SysaStatusCallback,
    pub start: SysaStartCallback,
    pub stop: SysaStopCallback,
    pub restart: SysaRestartCallback,
    pub reload: SysaReloadCallback,
    pub sync_state: SysaSyncStateCallback,
}

// The table is only ever handed out behind the per-worker serialization gate,
// so the C-provided `userdata` is never touched concurrently.
unsafe impl Send for SysaController {}
unsafe impl Sync for SysaController {}

/// A `UnitController` whose operations call back into C.
pub(crate) struct CController {
    vtable: SysaController,
    gate: Arc<Mutex<()>>,
}

impl CController {
    pub(crate) fn new(vtable: SysaController, gate: Arc<Mutex<()>>) -> Self {
        CController { vtable, gate }
    }

    async fn invoke<T, F>(&self, what: &'static str, call: F) -> anyhow::Result<T>
    where
        F: FnOnce() -> (SysaStatus, T) + Send + 'static,
        T: Send + 'static,
    {
        let gate = self.gate.clone();
        match tokio::task::spawn_blocking(move || {
            let _guard = gate.lock().unwrap_or_else(|poison| poison.into_inner());
            let _callback = crate::runtime::CallbackGuard::enter();
            call()
        })
        .await
        {
            Ok((SysaStatus::Ok, value)) => Ok(value),
            Ok((status, _)) => Err(anyhow!("{what}: the C controller returned {status:?}")),
            Err(error) => Err(anyhow!(
                "{what}: the C controller callback panicked: {error}"
            )),
        }
    }
}

#[async_trait::async_trait]
impl UnitController for CController {
    async fn status(&self, unit_name: &str) -> anyhow::Result<UnitStatus> {
        let Some(callback) = self.vtable.status else {
            bail!("status: the C controller does not implement status");
        };
        let userdata = self.vtable.userdata as usize;
        let name = crate::ffi::sanitize(unit_name);
        let value = self
            .invoke("status", move || {
                let mut out: *mut SysaUnitStatus = std::ptr::null_mut();
                let status = unsafe { callback(userdata as *mut c_void, name.as_ptr(), &mut out) };
                let value = if status == SysaStatus::Ok {
                    unsafe { crate::unit_status::take_unit_status(out) }
                } else {
                    if !out.is_null() {
                        drop(unsafe { Box::from_raw(out) });
                    }
                    None
                };
                (status, value)
            })
            .await?;
        value.ok_or_else(|| anyhow!("status: the C controller returned no unit status"))
    }

    async fn start(
        &self,
        unit_name: &str,
        config: &[u8],
        invocation_id: &str,
    ) -> anyhow::Result<()> {
        let Some(callback) = self.vtable.start else {
            bail!("start: the C controller does not implement start");
        };
        let userdata = self.vtable.userdata as usize;
        let name = crate::ffi::sanitize(unit_name);
        let config = config.to_vec();
        let invocation_id = crate::ffi::sanitize(invocation_id);
        self.invoke("start", move || {
            let status = unsafe {
                callback(
                    userdata as *mut c_void,
                    name.as_ptr(),
                    config.as_ptr(),
                    config.len(),
                    invocation_id.as_ptr(),
                )
            };
            (status, ())
        })
        .await
    }

    async fn stop(&self, unit_name: &str) -> anyhow::Result<()> {
        let Some(callback) = self.vtable.stop else {
            bail!("stop: the C controller does not implement stop");
        };
        let userdata = self.vtable.userdata as usize;
        let name = crate::ffi::sanitize(unit_name);
        self.invoke("stop", move || {
            let status = unsafe { callback(userdata as *mut c_void, name.as_ptr()) };
            (status, ())
        })
        .await
    }

    async fn restart(
        &self,
        unit_name: &str,
        config: &[u8],
        invocation_id: &str,
    ) -> anyhow::Result<()> {
        let Some(callback) = self.vtable.restart else {
            bail!("restart: the C controller does not implement restart");
        };
        let userdata = self.vtable.userdata as usize;
        let name = crate::ffi::sanitize(unit_name);
        let config = config.to_vec();
        let invocation_id = crate::ffi::sanitize(invocation_id);
        self.invoke("restart", move || {
            let status = unsafe {
                callback(
                    userdata as *mut c_void,
                    name.as_ptr(),
                    config.as_ptr(),
                    config.len(),
                    invocation_id.as_ptr(),
                )
            };
            (status, ())
        })
        .await
    }

    async fn reload(&self, unit_name: &str, config: &[u8]) -> anyhow::Result<()> {
        let Some(callback) = self.vtable.reload else {
            bail!("reload: the C controller does not implement reload");
        };
        let userdata = self.vtable.userdata as usize;
        let name = crate::ffi::sanitize(unit_name);
        let config = config.to_vec();
        self.invoke("reload", move || {
            let status = unsafe {
                callback(
                    userdata as *mut c_void,
                    name.as_ptr(),
                    config.as_ptr(),
                    config.len(),
                )
            };
            (status, ())
        })
        .await
    }

    async fn sync_state(&self) -> Vec<UnitStatus> {
        let Some(callback) = self.vtable.sync_state else {
            return Vec::new();
        };
        let userdata = self.vtable.userdata as usize;
        match self
            .invoke("sync_state", move || {
                let mut out: *mut *mut SysaUnitStatus = std::ptr::null_mut();
                let mut count = 0usize;
                let status = unsafe { callback(userdata as *mut c_void, &mut out, &mut count) };
                let mut values = Vec::new();
                if !out.is_null() {
                    for index in 0..count {
                        let entry = unsafe { *out.add(index) };
                        if let Some(value) = unsafe { crate::unit_status::take_unit_status(entry) }
                        {
                            values.push(value);
                        }
                    }
                    unsafe { crate::unit_status::free_status_array(out, count) };
                }
                (status, values)
            })
            .await
        {
            Ok(values) => values,
            Err(_) => Vec::new(),
        }
    }
}
