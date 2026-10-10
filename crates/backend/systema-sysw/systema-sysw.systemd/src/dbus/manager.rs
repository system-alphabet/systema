//! Bridge implementation of `org.freedesktop.systemd1.Manager`.
//!
//! Reads come from the control-port [`UnitMirror`]; every mutation is
//! translated to a `manager.*` control RPC.  Job methods reply as soon as the
//! job is queued — the returned job path is a reference value and completion
//! arrives as the `JobRemoved` signal, exactly as systemd does.  Holding the
//! reply until the job finished would pin a caller's connection for the whole
//! job: logind sits inside `CreateSession` waiting for its `StartUnit`, so
//! every other request made against logind queues up behind that one reply.

use std::os::fd::AsRawFd;
use std::sync::Arc;
use tracing::{debug, info};
use zbus::interface;
use zvariant::{OwnedObjectPath, OwnedValue};

use super::BridgeContext;

use sysa::proto::{
    EnqueueJobRequest, EnqueueJobResult, LoadUnitRequest, LoadUnitResult, RefUnitRequest,
    RefUnitResult, ResetFailedRequest, ResetFailedUnitRequest, SimpleManagerResult,
    TransientProperty, TransientUnitRequest, UnitSnapshot,
};

// --------------------------------------------------------------------------
// Helper: D-Bus path encoding
// --------------------------------------------------------------------------

/// Encode a unit name as a D-Bus object path segment.
/// e.g. "nginx.service" → "nginx_2eservice"
pub fn encode_unit_path(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
        } else {
            out.push_str(&format!("_{:02x}", ch as u32));
        }
    }
    out
}

pub fn unit_object_path(name: &str) -> OwnedObjectPath {
    let encoded = encode_unit_path(name);
    OwnedObjectPath::try_from(format!("/org/freedesktop/systemd1/unit/{}", encoded))
        .expect("valid unit path")
}

pub fn job_object_path(job_id: u64) -> OwnedObjectPath {
    OwnedObjectPath::try_from(format!("/org/freedesktop/systemd1/job/{}", job_id))
        .expect("valid job path")
}

/// Validate a job-mode string locally (the allocator enforces it too, but
/// rejecting here keeps the D-Bus error type faithful).
fn parse_job_mode(mode: &str) -> zbus::fdo::Result<()> {
    let valid = [
        "fail", "replace", "replace-irreversibly", "isolate", "flush",
        "ignore-dependencies", "ignore-requirements", "triggering",
        "restart-dependencies", "lenient", "queue",
    ];
    if valid.contains(&mode) {
        Ok(())
    } else {
        Err(zbus::fdo::Error::InvalidArgs(sysa::l10n::fmt(
            sysa::l10n::t_("Job mode {mode} invalid"),
            &[("mode", &mode.to_string())],
        )))
    }
}

/// Validate a job-type string (`bus_unit_parse_job_type()`).
fn parse_job_type(s: &str) -> zbus::fdo::Result<(&'static str, bool)> {
    match s {
        "start" => Ok(("start", false)),
        "verify-active" => Ok(("verify-active", false)),
        "stop" => Ok(("stop", false)),
        "reload" => Ok(("reload", false)),
        "restart" => Ok(("restart", false)),
        "try-restart" => Ok(("try-restart", false)),
        "try-reload" => Ok(("try-reload", false)),
        "reload-or-start" => Ok(("reload-or-start", false)),
        "nop" => Ok(("nop", false)),
        "reload-or-restart" => Ok(("restart", true)),
        "reload-or-try-restart" => Ok(("try-restart", true)),
        other => Err(zbus::fdo::Error::InvalidArgs(sysa::l10n::fmt(
            sysa::l10n::t_("Job type {other} invalid"),
            &[("other", &other.to_string())],
        ))),
    }
}

// --------------------------------------------------------------------------
// Unit info tuples (systemd D-Bus types)
// --------------------------------------------------------------------------

/// (name, description, load_state, active_state, sub_state, following,
///  object_path, job_id, job_type, job_object_path)
type UnitInfo = (
    String,
    String,
    String,
    String,
    String,
    String,
    OwnedObjectPath,
    u32,
    String,
    OwnedObjectPath,
);

/// (job_id, unit_name, job_type, job_state, job_object_path, unit_object_path)
pub type JobInfoTuple = (
    u32,
    String,
    String,
    String,
    OwnedObjectPath,
    OwnedObjectPath,
);

/// (path, state) with state "enabled"/"disabled"/"static" etc.
type UnitFileInfo = (String, String);

fn unit_info_entry(snap: &UnitSnapshot) -> UnitInfo {
    let job = (snap.running_job_id != 0).then_some(snap.running_job_id);
    let (job_id, job_type) = job
        .filter(|id| *id <= u32::MAX as u64)
        .map(|id| (id as u32, String::new()))
        .unwrap_or((0, String::new()));
    let job_path = if job_id > 0 {
        job_object_path(job_id as u64)
    } else {
        OwnedObjectPath::try_from("/").unwrap()
    };
    (
        snap.name.clone(),
        snap.description.clone(),
        snap.load_state.clone(),
        snap.active_state.clone(),
        if snap.sub_state.is_empty() {
            "dead".to_string()
        } else {
            snap.sub_state.clone()
        },
        String::new(),
        unit_object_path(&snap.name),
        job_id,
        job_type,
        job_path,
    )
}

// --------------------------------------------------------------------------
// Value translation for property bags (a(sv) → proto PropertyBag)
// --------------------------------------------------------------------------

/// Read the PID a pidfd refers to.
///
/// The kernel writes a `Pid:` line into a pidfd's `fdinfo` and nothing of the
/// sort for any other kind of descriptor, which is how the two are told apart;
/// `Ok(None)` therefore means "this is not a pidfd".
fn pid_from_pidfd(raw: std::os::fd::RawFd) -> std::io::Result<Option<u32>> {
    let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{raw}"))?;
    Ok(info
        .lines()
        .find_map(|line| line.strip_prefix("Pid:").map(str::trim))
        .and_then(|pid| pid.parse().ok()))
}

/// Push one proto property entry for `key`, expanding string/u32/u64/bool
/// arrays into multiple same-key `Value::S` / `Value::U` / `Value::B` entries
/// so the server's `prop_strings` / `prop_u32s` bag readers reconstruct the
/// list (systemd `a(sv)` list semantics).
fn push_prop_value(out: &mut Vec<TransientProperty>, key: &str, value: &OwnedValue) {
    let value: zvariant::Value = match value.try_clone() {
        Ok(v) => v.into(),
        Err(_) => return,
    };
    match value {
        zvariant::Value::Array(array) => {
            let sig = array.element_signature().as_str().to_string();
            for item in array.inner() {
                match sig.as_str() {
                    "s" => {
                        if let zvariant::Value::Str(s) = item {
                            push_prop_entry(
                                out,
                                key,
                                sysa::proto::manager_value::Value::S(s.to_string()),
                            );
                        }
                    }
                    "u" => {
                        if let zvariant::Value::U32(u) = item {
                            push_prop_entry(
                                out,
                                key,
                                sysa::proto::manager_value::Value::U(*u as u64),
                            );
                        }
                    }
                    "t" => {
                        if let zvariant::Value::U64(u) = item {
                            push_prop_entry(
                                out,
                                key,
                                sysa::proto::manager_value::Value::U(*u),
                            );
                        }
                    }
                    "b" => {
                        if let zvariant::Value::Bool(b) = item {
                            push_prop_entry(
                                out,
                                key,
                                sysa::proto::manager_value::Value::B(*b),
                            );
                        }
                    }
                    "h" => {
                        // `PIDFDs=` (ah) — logind names a session scope's
                        // processes by pidfd.  The control protocol has no
                        // file-descriptor type, so resolve each descriptor to
                        // the PID it refers to while it is still open here.
                        if let zvariant::Value::Fd(fd) = item {
                            match pid_from_pidfd(fd.as_raw_fd()) {
                                Ok(Some(pid)) => push_prop_entry(
                                    out,
                                    key,
                                    sysa::proto::manager_value::Value::U(pid as u64),
                                ),
                                Ok(None) => debug!(
                                    "dropping {key} entry: fd {} is not a pidfd",
                                    fd.as_raw_fd()
                                ),
                                Err(e) => debug!(
                                    "dropping {key} entry: cannot read fdinfo for fd {}: {e}",
                                    fd.as_raw_fd()
                                ),
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => push_scalar_entry(out, key, value),
    }
}

fn push_prop_entry(
    out: &mut Vec<TransientProperty>,
    key: &str,
    value: sysa::proto::manager_value::Value,
) {
    out.push(TransientProperty {
        key: key.to_string(),
        value: Some(sysa::proto::ManagerValue {
            value: Some(value),
        }),
    });
}

fn push_scalar_entry(out: &mut Vec<TransientProperty>, key: &str, value: zvariant::Value) {
    let v = match value {
        zvariant::Value::Str(s) => sysa::proto::manager_value::Value::S(s.to_string()),
        zvariant::Value::Bool(b) => sysa::proto::manager_value::Value::B(b),
        zvariant::Value::U64(u) => sysa::proto::manager_value::Value::U(u),
        zvariant::Value::U32(u) => sysa::proto::manager_value::Value::U(u as u64),
        zvariant::Value::I64(i) => sysa::proto::manager_value::Value::I(i),
        zvariant::Value::I32(i) => sysa::proto::manager_value::Value::I(i as i64),
        _ => return,
    };
    push_prop_entry(out, key, v);
}

/// Convert an `a(sv)` property array into proto `TransientProperty` entries.
/// Unknown value types are dropped (systemd ignores unrecognised properties).
fn properties_to_bag(properties: &[(String, OwnedValue)]) -> Option<sysa::proto::PropertyBag> {
    let mut out = Vec::new();
    for (key, value) in properties {
        push_prop_value(&mut out, key, value);
    }
    Some(sysa::proto::PropertyBag { properties: out })
}

// --------------------------------------------------------------------------
// Manager interface
// --------------------------------------------------------------------------

pub struct ManagerInterface {
    pub ctx: Arc<BridgeContext>,
}

impl ManagerInterface {
    pub fn new(ctx: Arc<BridgeContext>) -> Self {
        ManagerInterface { ctx }
    }

    /// Enqueue a job by type string and return its job path immediately.
    ///
    /// systemd's `bus_unit_queue_job()` sends the reply before the job has run
    /// (`sd_bus_message_send(reply)` sits ahead of the job's own state
    /// machine), so callers learn the outcome from `JobRemoved` rather than
    /// from this method's reply.  Waiting here would pin a caller's connection
    /// for the whole job, which is what froze logind: it blocks inside
    /// `CreateSession` on this reply, and `systemd-user-runtime-dir`'s property
    /// reads against logind then queue up until the reply finally lands.
    async fn enqueue_job(
        &self,
        name: &str,
        job_type: &str,
        mode: &str,
        reload_if_possible: bool,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        let req = EnqueueJobRequest {
            name: name.to_string(),
            job_type: job_type.to_string(),
            mode: mode.to_string(),
            reload_if_possible,
        };
        let reply: EnqueueJobResult = self
            .ctx
            .client
            .call("manager.enqueue", &req)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        if !reply.success {
            return Err(zbus::fdo::Error::Failed(reply.message));
        }
        super::ensure_unit_object(&self.ctx, &reply.unit_name).await;
        Ok(job_object_path(reply.job_id))
    }

    async fn emit_reloading(&self) {
        let Some(conn) = super::connection(&self.ctx) else {
            return;
        };
        if let Ok(signal_ctx) = zbus::SignalContext::new(conn, "/org/freedesktop/systemd1") {
            let _ = ManagerInterface::reloading(&signal_ctx).await;
        }
    }

    async fn emit_reloaded(&self) {
        self.emit_reloading().await;
    }

    /// Resolve the PID of the process owning the given unique bus name.
    async fn sender_pid(&self, header: zbus::MessageHeader<'_>) -> Option<u32> {
        let conn = match super::connection(&self.ctx) {
            Some(c) => c.clone(),
            None => return None,
        };
        let sender = header.sender()?.as_str();
        let proxy = zbus::fdo::DBusProxy::new(&conn).await.ok()?;
        let bus_name = zbus::names::BusName::try_from(sender).ok()?;
        proxy.get_connection_unix_process_id(bus_name).await.ok()
    }
}

#[interface(name = "org.freedesktop.systemd1.Manager")]
impl ManagerInterface {
    // ------------------------------------------------------------------
    // D-Bus signals
    // ------------------------------------------------------------------

    #[zbus(signal)]
    pub async fn job_new(
        ctxt: &zbus::SignalContext<'_>,
        id: u32,
        job: OwnedObjectPath,
        unit: String,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn job_removed(
        ctxt: &zbus::SignalContext<'_>,
        id: u32,
        job: OwnedObjectPath,
        unit: String,
        result: String,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn unit_removed(
        ctxt: &zbus::SignalContext<'_>,
        unit: String,
        job: OwnedObjectPath,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn reloading(ctxt: &zbus::SignalContext<'_>) -> zbus::Result<()>;

    // ------------------------------------------------------------------
    // Unit lookup methods
    // ------------------------------------------------------------------

    async fn get_unit(&self, name: &str) -> zbus::fdo::Result<OwnedObjectPath> {
        debug!("D-Bus GetUnit: {}", name);
        let name = super::resolve_canonical(&self.ctx, name).await;
        if self.ctx.mirror.read().get(&name).is_some() {
            Ok(unit_object_path(&name))
        } else {
            Err(zbus::fdo::Error::UnknownObject(sysa::l10n::fmt(
                sysa::l10n::t_("Unit {name} is not loaded."),
                &[("name", &name.to_string())],
            )))
        }
    }

    async fn get_unit_by_pid(&self, pid: u32) -> zbus::fdo::Result<OwnedObjectPath> {
        debug!("D-Bus GetUnitByPID: pid={}", pid);
        match self.ctx.mirror.read().find_by_pid(pid) {
            Some(name) => Ok(unit_object_path(&name)),
            None => Err(zbus::fdo::Error::UnknownObject(sysa::l10n::fmt(
                sysa::l10n::t_("No unit found for PID {pid}."),
                &[("pid", &pid.to_string())],
            ))),
        }
    }

    async fn get_unit_by_pidfd(&self, fd: zvariant::OwnedFd) -> zbus::fdo::Result<OwnedObjectPath> {
        debug!("D-Bus GetUnitByPIDFD");
        let raw = fd.as_raw_fd();
        let pid = match pid_from_pidfd(raw) {
            Ok(Some(pid)) => pid,
            Ok(None) => {
                return Err(zbus::fdo::Error::InvalidArgs(sysa::l10n::fmt(
                    sysa::l10n::t_("fd {raw} is not a pidfd (no Pid: entry in fdinfo)"),
                    &[("raw", &raw.to_string())],
                )));
            }
            Err(e) => {
                return Err(zbus::fdo::Error::InvalidArgs(sysa::l10n::fmt(
                    sysa::l10n::t_("cannot read fdinfo for fd {raw}: {e}"),
                    &[("raw", &raw.to_string()), ("e", &e.to_string())],
                )));
            }
        };
        self.get_unit_by_pid(pid).await
    }

    async fn load_unit(&self, name: &str) -> zbus::fdo::Result<OwnedObjectPath> {
        debug!("D-Bus LoadUnit: {}", name);
        let req = LoadUnitRequest {
            name: name.to_string(),
        };
        let reply: LoadUnitResult = self
            .ctx
            .client
            .call("manager.load_unit", &req)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        if !reply.success {
            return Err(zbus::fdo::Error::Failed(reply.message));
        }
        super::ensure_unit_object(&self.ctx, &reply.name).await;
        Ok(unit_object_path(&reply.name))
    }

    // ------------------------------------------------------------------
    // Job enqueueing methods
    // ------------------------------------------------------------------

    async fn start_unit(&self, name: &str, mode: &str) -> zbus::fdo::Result<OwnedObjectPath> {
        info!("D-Bus StartUnit: {} (mode={})", name, mode);
        parse_job_mode(mode)?;
        self.enqueue_job(name, "start", mode, false).await
    }

    /// Queue a start job on behalf of an
    /// `org.freedesktop.systemd1.Activator.ActivationRequest`.
    ///
    /// systemd's `signal_activation_request()` only enqueues: a signal has no
    /// reply, so dbus-daemon learns the activation succeeded by watching the
    /// unit claim its bus name, not from us.
    pub(crate) async fn enqueue_for_activation(&self, unit: &str) -> zbus::fdo::Result<()> {
        self.enqueue_job(unit, "start", "replace", false).await?;
        Ok(())
    }

    async fn stop_unit(&self, name: &str, mode: &str) -> zbus::fdo::Result<OwnedObjectPath> {
        info!("D-Bus StopUnit: {} (mode={})", name, mode);
        parse_job_mode(mode)?;
        self.enqueue_job(name, "stop", mode, false).await
    }

    async fn restart_unit(&self, name: &str, mode: &str) -> zbus::fdo::Result<OwnedObjectPath> {
        info!("D-Bus RestartUnit: {} (mode={})", name, mode);
        parse_job_mode(mode)?;
        self.enqueue_job(name, "restart", mode, false).await
    }

    async fn reload_unit(&self, name: &str, mode: &str) -> zbus::fdo::Result<OwnedObjectPath> {
        info!("D-Bus ReloadUnit: {} (mode={})", name, mode);
        parse_job_mode(mode)?;
        self.enqueue_job(name, "reload", mode, false).await
    }

    async fn try_restart_unit(&self, name: &str, mode: &str) -> zbus::fdo::Result<OwnedObjectPath> {
        info!("D-Bus TryRestartUnit: {} (mode={})", name, mode);
        parse_job_mode(mode)?;
        self.enqueue_job(name, "try-restart", mode, false).await
    }

    async fn try_reload_unit(&self, name: &str, mode: &str) -> zbus::fdo::Result<OwnedObjectPath> {
        info!("D-Bus TryReloadUnit: {} (mode={})", name, mode);
        parse_job_mode(mode)?;
        self.enqueue_job(name, "try-reload", mode, false).await
    }

    async fn reload_or_restart_unit(
        &self,
        name: &str,
        mode: &str,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        info!("D-Bus ReloadOrRestartUnit: {} (mode={})", name, mode);
        parse_job_mode(mode)?;
        self.enqueue_job(name, "reload-or-restart", mode, true)
            .await
    }

    async fn reload_or_try_restart_unit(
        &self,
        name: &str,
        mode: &str,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        info!("D-Bus ReloadOrTryRestartUnit: {} (mode={})", name, mode);
        parse_job_mode(mode)?;
        self.enqueue_job(name, "reload-or-try-restart", mode, true)
            .await
    }

    async fn enqueue_unit_job(
        &self,
        name: &str,
        job_type: &str,
        job_mode: &str,
    ) -> zbus::fdo::Result<(
        u32,
        OwnedObjectPath,
        String,
        OwnedObjectPath,
        String,
        Vec<(u32, OwnedObjectPath, String, OwnedObjectPath, String)>,
    )> {
        info!(
            "D-Bus EnqueueUnitJob: unit={} job_type={} job_mode={}",
            name, job_type, job_mode
        );
        let (kind, reload_if_possible) = parse_job_type(job_type)?;
        parse_job_mode(job_mode)?;
        let req = EnqueueJobRequest {
            name: name.to_string(),
            job_type: kind.to_string(),
            mode: job_mode.to_string(),
            reload_if_possible,
        };
        let reply: EnqueueJobResult = self
            .ctx
            .client
            .call("manager.enqueue", &req)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        if !reply.success {
            return Err(zbus::fdo::Error::Failed(reply.message));
        }
        super::ensure_unit_object(&self.ctx, &reply.unit_name).await;
        Ok((
            reply.job_id as u32,
            job_object_path(reply.job_id),
            reply.unit_name.clone(),
            unit_object_path(&reply.unit_name),
            kind.to_string(),
            Vec::new(),
        ))
    }

    async fn enqueue_unit_job_many(
        &self,
        units: Vec<String>,
        job_type: &str,
        job_mode: &str,
        flags: u64,
    ) -> zbus::fdo::Result<Vec<(u32, OwnedObjectPath, String, OwnedObjectPath, String)>> {
        info!(
            "D-Bus EnqueueUnitJobMany: {} unit(s), job_type={}, job_mode={}",
            units.len(),
            job_type,
            job_mode
        );
        if units.is_empty() {
            return Err(zbus::fdo::Error::InvalidArgs(
                sysa::l10n::t_("At least one unit name is required.").to_string(),
            ));
        }
        if flags != 0 {
            return Err(zbus::fdo::Error::InvalidArgs(
                sysa::l10n::t_("Flags are not supported yet and must be 0.").to_string(),
            ));
        }
        let (kind, reload_if_possible) = parse_job_type(job_type)?;
        parse_job_mode(job_mode)?;
        let mut jobs = Vec::with_capacity(units.len());
        for name in units {
            let req = EnqueueJobRequest {
                name: name.clone(),
                job_type: kind.to_string(),
                mode: job_mode.to_string(),
                reload_if_possible,
            };
            let reply: EnqueueJobResult = self
                .ctx
                .client
                .call("manager.enqueue", &req)
                .await
                .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
            if !reply.success {
                return Err(zbus::fdo::Error::Failed(reply.message));
            }
            super::ensure_unit_object(&self.ctx, &reply.unit_name).await;
            jobs.push((
                reply.job_id as u32,
                job_object_path(reply.job_id),
                reply.unit_name.clone(),
                unit_object_path(&reply.unit_name),
                kind.to_string(),
            ));
        }
        Ok(jobs)
    }

    async fn start_transient_unit(
        &self,
        #[zbus(header)] header: zbus::MessageHeader<'_>,
        name: &str,
        mode: &str,
        properties: Vec<(String, OwnedValue)>,
        _aux_units: Vec<(String, Vec<(String, OwnedValue)>)>,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        info!("D-Bus StartTransientUnit: {} (mode={})", name, mode);
        parse_job_mode(mode)?;
        let sender_pid = self.sender_pid(header).await;
        let req = TransientUnitRequest {
            name: name.to_string(),
            kind: String::new(),
            properties: properties_to_bag(&properties),
            sender_pid: sender_pid.unwrap_or(0),
            mode: mode.to_string(),
        };
        let reply: EnqueueJobResult = self
            .ctx
            .client
            .call("manager.start_transient", &req)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        if !reply.success {
            return Err(zbus::fdo::Error::Failed(reply.message));
        }
        super::ensure_unit_object(&self.ctx, &reply.unit_name).await;
        Ok(job_object_path(reply.job_id))
    }

    async fn start_transient_unit_many(
        &self,
        #[zbus(header)] header: zbus::MessageHeader<'_>,
        units: Vec<(String, Vec<(String, OwnedValue)>)>,
        mode: &str,
        _aux_units: Vec<(String, Vec<(String, OwnedValue)>)>,
    ) -> zbus::fdo::Result<Vec<OwnedObjectPath>> {
        info!(
            "D-Bus StartTransientUnitMany: {} unit(s), mode={}",
            units.len(),
            mode
        );
        parse_job_mode(mode)?;
        let sender_pid = self.sender_pid(header).await;
        let mut jobs = Vec::with_capacity(units.len());
        for (name, properties) in units {
            let req = TransientUnitRequest {
                name: name.clone(),
                kind: String::new(),
                properties: properties_to_bag(&properties),
                sender_pid: sender_pid.unwrap_or(0),
                mode: mode.to_string(),
            };
            let reply: EnqueueJobResult = self
                .ctx
                .client
                .call("manager.start_transient", &req)
                .await
                .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
            if !reply.success {
                return Err(zbus::fdo::Error::Failed(reply.message));
            }
            super::ensure_unit_object(&self.ctx, &reply.unit_name).await;
            jobs.push(job_object_path(reply.job_id));
        }
        Ok(jobs)
    }

    async fn set_unit_properties(
        &self,
        name: &str,
        mode: &str,
        properties: Vec<(String, OwnedValue)>,
    ) -> zbus::fdo::Result<()> {
        info!("D-Bus SetUnitProperties: {} (mode={})", name, mode);
        if mode != "replace" {
            return Err(zbus::fdo::Error::InvalidArgs(sysa::l10n::fmt(
                sysa::l10n::t_("SetUnitProperties only supports job mode 'replace' (got {mode})"),
                &[("mode", &mode.to_string())],
            )));
        }
        let req = sysa::proto::SetUnitPropertiesRequest {
            name: name.to_string(),
            mode: mode.to_string(),
            properties: properties_to_bag(&properties),
        };
        let reply: SimpleManagerResult = self
            .ctx
            .client
            .call("manager.set_unit_properties", &req)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        if reply.success {
            Ok(())
        } else {
            Err(zbus::fdo::Error::Failed(reply.message))
        }
    }

    // ------------------------------------------------------------------
    // Listing methods
    // ------------------------------------------------------------------

    async fn list_units(&self) -> zbus::fdo::Result<Vec<UnitInfo>> {
        debug!("D-Bus ListUnits");
        let mirror = self.ctx.mirror.read();
        let result: Vec<UnitInfo> = mirror.names().iter().filter_map(|n| {
            mirror.get(n).map(unit_info_entry)
        }).collect();
        Ok(result)
    }

    async fn list_units_filtered(&self, states: Vec<String>) -> zbus::fdo::Result<Vec<UnitInfo>> {
        debug!("D-Bus ListUnitsFiltered: states={:?}", states);
        let mirror = self.ctx.mirror.read();
        let result: Vec<UnitInfo> = mirror
            .names()
            .into_iter()
            .filter_map(|n| mirror.get(&n).map(unit_info_entry))
            .filter(|info| {
                states.is_empty() || states.iter().any(|s| s == &info.2)
            })
            .collect();
        Ok(result)
    }

    async fn list_units_by_patterns(
        &self,
        states: Vec<String>,
        patterns: Vec<String>,
    ) -> zbus::fdo::Result<Vec<UnitInfo>> {
        debug!(
            "D-Bus ListUnitsByPatterns: states={:?} patterns={:?}",
            states, patterns
        );
        let mirror = self.ctx.mirror.read();
        let result: Vec<UnitInfo> = mirror
            .names()
            .into_iter()
            .filter_map(|n| mirror.get(&n).map(unit_info_entry))
            .filter(|info| {
                if !patterns.is_empty() && !patterns.iter().any(|p| matches_glob(p, &info.0)) {
                    return false;
                }
                if !states.is_empty() && !states.iter().any(|s| s == &info.2) {
                    return false;
                }
                true
            })
            .collect();
        Ok(result)
    }

    async fn list_units_by_names(&self, names: Vec<String>) -> zbus::fdo::Result<Vec<UnitInfo>> {
        debug!("D-Bus ListUnitsByNames: names={:?}", names);
        let mut result = Vec::new();
        for name in names {
            let canonical = super::resolve_canonical(&self.ctx, &name).await;
            if let Some(snap) = super::snapshot_of(&self.ctx, &canonical) {
                result.push(unit_info_entry(&snap));
            }
        }
        Ok(result)
    }

    async fn list_jobs(&self) -> zbus::fdo::Result<Vec<JobInfoTuple>> {
        debug!("D-Bus ListJobs");
        let mirror = self.ctx.mirror.read();
        let result = mirror
            .running_jobs()
            .iter()
            .map(|j| {
                (
                    j.job_id as u32,
                    j.unit_name.clone(),
                    j.job_type.clone(),
                    j.status.clone(),
                    job_object_path(j.job_id),
                    unit_object_path(&j.unit_name),
                )
            })
            .collect();
        Ok(result)
    }

    async fn list_unit_files(&self) -> zbus::fdo::Result<Vec<UnitFileInfo>> {
        debug!("D-Bus ListUnitFiles");
        let mirror = self.ctx.mirror.read();
        let result: Vec<UnitFileInfo> = mirror
            .names()
            .iter()
            .filter_map(|n| mirror.get(n))
            .map(|snap| {
                (
                    format!(
                        "{}/{}",
                        sysa::paths::instance().systemd_lib_unit_dir,
                        snap.name
                    ),
                    snap.unit_file_state.clone(),
                )
            })
            .collect();
        Ok(result)
    }

    async fn list_unit_files_by_patterns(
        &self,
        states: Vec<String>,
        patterns: Vec<String>,
    ) -> zbus::fdo::Result<Vec<UnitFileInfo>> {
        debug!(
            "D-Bus ListUnitFilesByPatterns: states={:?} patterns={:?}",
            states, patterns
        );
        let mirror = self.ctx.mirror.read();
        let result: Vec<UnitFileInfo> = mirror
            .names()
            .iter()
            .filter_map(|n| mirror.get(n))
            .filter(|snap| {
                if !states.is_empty() && !states.iter().any(|s| s == &snap.unit_file_state) {
                    return false;
                }
                if !patterns.is_empty() && !patterns.iter().any(|p| matches_glob(p, &snap.name)) {
                    return false;
                }
                true
            })
            .map(|snap| {
                (
                    format!(
                        "{}/{}",
                        sysa::paths::instance().systemd_lib_unit_dir,
                        snap.name
                    ),
                    snap.unit_file_state.clone(),
                )
            })
            .collect();
        Ok(result)
    }

    async fn get_unit_file_state(&self, file: &str) -> zbus::fdo::Result<String> {
        debug!("D-Bus GetUnitFileState: file={}", file);
        let base = std::path::Path::new(file)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(file);
        let mirror = self.ctx.mirror.read();
        if let Some(snap) = mirror.get(base) {
            return Ok(snap.unit_file_state.clone());
        }
        // Not in memory — try to find it on disk without loading.
        for dir in sysa::paths::instance().unit_search_paths.iter() {
            if std::path::Path::new(dir).join(base).exists() {
                return Ok("static".to_string());
            }
        }
        Err(zbus::fdo::Error::Failed(sysa::l10n::fmt(
            sysa::l10n::t_("Unit file {base} not found."),
            &[("base", &base.to_string())],
        )))
    }

    async fn get_unit_processes(
        &self,
        unit_name: &str,
    ) -> zbus::fdo::Result<Vec<(String, u32, String)>> {
        debug!("D-Bus GetUnitProcesses: unit={}", unit_name);
        let mirror = self.ctx.mirror.read();
        let Some(snap) = mirror.get(unit_name) else {
            return Ok(Vec::new());
        };
        let Some(metrics) = &snap.metrics else {
            return Ok(Vec::new());
        };
        let cgroup_path = metrics.control_group.trim_end_matches('/');
        Ok(metrics
            .processes
            .iter()
            .map(|p| {
                let full = if p.subpath.is_empty() {
                    cgroup_path.to_string()
                } else {
                    format!("{}/{}", cgroup_path, p.subpath)
                };
                (full, p.pid, p.name.clone())
            })
            .collect())
    }

    // ------------------------------------------------------------------
    // Subscription management
    // ------------------------------------------------------------------

    async fn subscribe(&self) -> zbus::fdo::Result<()> {
        debug!("D-Bus Subscribe (no-op)");
        Ok(())
    }

    async fn unsubscribe(&self) -> zbus::fdo::Result<()> {
        debug!("D-Bus Unsubscribe (no-op)");
        Ok(())
    }

    // ------------------------------------------------------------------
    // Reload / daemon management
    // ------------------------------------------------------------------

    async fn reload(&self) -> zbus::fdo::Result<()> {
        info!("D-Bus Reload: rescan via System A's control plane");
        self.emit_reloading().await;
        let reply: SimpleManagerResult = self
            .ctx
            .client
            .call("manager.reload", &sysa::proto::ManagerReloadRequest {})
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        self.emit_reloaded().await;
        if reply.success {
            Ok(())
        } else {
            Err(zbus::fdo::Error::Failed(reply.message))
        }
    }

    async fn reset_failed_unit(&self, name: &str) -> zbus::fdo::Result<()> {
        debug!("D-Bus ResetFailedUnit: name={}", name);
        let req = ResetFailedUnitRequest {
            name: name.to_string(),
        };
        let reply: SimpleManagerResult = self
            .ctx
            .client
            .call("manager.reset_failed_unit", &req)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        if reply.success {
            Ok(())
        } else {
            Err(zbus::fdo::Error::Failed(reply.message))
        }
    }

    async fn reset_failed(&self) -> zbus::fdo::Result<()> {
        debug!("D-Bus ResetFailed");
        let reply: SimpleManagerResult = self
            .ctx
            .client
            .call("manager.reset_failed", &ResetFailedRequest::default())
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        if reply.success {
            Ok(())
        } else {
            Err(zbus::fdo::Error::Failed(reply.message))
        }
    }

    // ------------------------------------------------------------------
    // Scope / reference methods
    // ------------------------------------------------------------------

    async fn abandon_scope(&self, name: &str) -> zbus::fdo::Result<()> {
        debug!("D-Bus AbandonScope: name={}", name);
        let req = sysa::proto::AbandonScopeRequest {
            name: name.to_string(),
        };
        let reply: SimpleManagerResult = self
            .ctx
            .client
            .call("manager.abandon_scope", &req)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        if reply.success {
            Ok(())
        } else {
            Err(zbus::fdo::Error::Failed(reply.message))
        }
    }

    async fn ref_unit(&self, name: &str) -> zbus::fdo::Result<u32> {
        debug!("D-Bus RefUnit: name={}", name);
        let req = RefUnitRequest {
            name: name.to_string(),
        };
        let reply: RefUnitResult = self
            .ctx
            .client
            .call("manager.ref_unit", &req)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        if reply.success {
            Ok(reply.n_refs as u32)
        } else {
            Err(zbus::fdo::Error::Failed(reply.message))
        }
    }

    async fn unref_unit(&self, name: &str) -> zbus::fdo::Result<u32> {
        debug!("D-Bus UnrefUnit: name={}", name);
        let req = RefUnitRequest {
            name: name.to_string(),
        };
        let reply: RefUnitResult = self
            .ctx
            .client
            .call("manager.unref_unit", &req)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        if reply.success {
            Ok(reply.n_refs as u32)
        } else {
            Err(zbus::fdo::Error::Failed(reply.message))
        }
    }

    async fn get_unit_by_invocation_id(
        &self,
        invocation_id: &str,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        debug!("D-Bus GetUnitByInvocationID: id={}", invocation_id);
        match self.ctx.mirror.read().find_by_invocation(invocation_id) {
            Some(name) => Ok(unit_object_path(&name)),
            None => Err(zbus::fdo::Error::UnknownObject(sysa::l10n::fmt(
                sysa::l10n::t_("No unit with invocation ID {invocation_id}."),
                &[("invocation_id", &invocation_id.to_string())],
            ))),
        }
    }

    // ------------------------------------------------------------------
    // Manager properties (mirror reads + constants like System A served)
    // ------------------------------------------------------------------

    #[zbus(property)]
    fn version(&self) -> &str {
        "255"
    }

    #[zbus(property)]
    fn features(&self) -> &str {
        ""
    }

    #[zbus(property)]
    fn virtualization(&self) -> &str {
        ""
    }

    #[zbus(property)]
    fn architecture(&self) -> &str {
        std::env::consts::ARCH
    }

    #[zbus(property)]
    fn tainted(&self) -> &str {
        ""
    }

    #[zbus(property)]
    fn firmware_timestamp(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn loader_timestamp(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn kernel_timestamp(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn init_r_d_timestamp(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn userspace_timestamp(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn finish_timestamp(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn security_start_timestamp(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn security_finish_timestamp(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn generators_start_timestamp(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn generators_finish_timestamp(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn units_load_start_timestamp(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn units_load_finish_timestamp(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn log_level(&self) -> &str {
        "info"
    }

    #[zbus(property)]
    fn log_target(&self) -> &str {
        "journal"
    }

    #[zbus(property)]
    fn n_names(&self) -> u32 {
        self.ctx.mirror.read().len() as u32
    }

    #[zbus(property)]
    fn n_failed_units(&self) -> u32 {
        self.ctx.mirror.read().failed_count()
    }

    #[zbus(property)]
    fn n_jobs(&self) -> u32 {
        self.ctx.mirror.read().running_jobs().len() as u32
    }

    #[zbus(property)]
    fn n_installed_jobs(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn n_failed_jobs(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn progress(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn environment(&self) -> Vec<String> {
        std::env::vars()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect()
    }

    #[zbus(property)]
    fn confirm_spawn(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn show_status(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn unit_path(&self) -> Vec<String> {
        sysa::paths::instance()
            .unit_search_paths
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[zbus(property)]
    fn default_standard_output(&self) -> &str {
        "journal"
    }

    #[zbus(property)]
    fn default_standard_error(&self) -> &str {
        "journal"
    }

    #[zbus(property)]
    fn runtime_watchdog_u_sec(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn reboot_watchdog_u_sec(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn k_exec_watchdog_u_sec(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn service_watchdogs(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn control_group(&self) -> &str {
        "/"
    }

    #[zbus(property)]
    fn system_state(&self) -> &str {
        "running"
    }

    #[zbus(property)]
    fn exit_code(&self) -> u8 {
        0
    }

    #[zbus(property)]
    fn default_timer_accuracy_u_sec(&self) -> u64 {
        60_000_000
    }

    #[zbus(property)]
    fn default_timeout_start_u_sec(&self) -> u64 {
        90_000_000
    }

    #[zbus(property)]
    fn default_timeout_stop_u_sec(&self) -> u64 {
        90_000_000
    }

    #[zbus(property)]
    fn default_timeout_abort_u_sec(&self) -> u64 {
        90_000_000
    }

    #[zbus(property)]
    fn default_restart_u_sec(&self) -> u64 {
        100_000
    }

    #[zbus(property)]
    fn default_start_limit_interval_u_sec(&self) -> u64 {
        10_000_000
    }

    #[zbus(property)]
    fn default_start_limit_burst(&self) -> u32 {
        5
    }

    #[zbus(property)]
    fn default_c_p_u_accounting(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn default_block_i_o_accounting(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn default_memory_accounting(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn default_tasks_accounting(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn default_limit_c_p_u(&self) -> u64 {
        u64::MAX
    }

    #[zbus(property)]
    fn default_tasks_max(&self) -> u64 {
        u64::MAX
    }

    #[zbus(property)]
    fn timer_slack_n_sec(&self) -> u64 {
        50_000
    }

    #[zbus(property)]
    fn shutdown_finish_timestamp(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn previous_shutdown_start_timestamp(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn previous_shutdown_finish_timestamp(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn previous_shutdown_late_start_timestamp(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn previous_shutdown_late_finish_timestamp(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn k_execs_count(&self) -> u32 {
        0
    }

    #[zbus(property)]
    fn reload_count(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn event_loop_rate_limit_interval_u_sec(&self) -> u64 {
        1_000_000
    }

    #[zbus(property)]
    fn event_loop_rate_limit_burst(&self) -> u32 {
        50_000
    }

    #[zbus(property)]
    fn c_p_u_set_partition(&self) -> &str {
        "member"
    }

    #[zbus(property)]
    fn o_o_m_rules(&self) -> Vec<String> {
        Vec::new()
    }
}

// --------------------------------------------------------------------------
// Helpers
// --------------------------------------------------------------------------

fn matches_glob(pattern: &str, name: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let nm: Vec<char> = name.chars().collect();
    glob_match(&pat, &nm)
}

fn glob_match(pattern: &[char], name: &[char]) -> bool {
    match (pattern.first(), name.first()) {
        (None, None) => true,
        (Some(&'*'), _) => {
            for i in 0..=name.len() {
                if glob_match(&pattern[1..], &name[i..]) {
                    return true;
                }
            }
            false
        }
        (Some(&'?'), Some(_)) => glob_match(&pattern[1..], &name[1..]),
        (Some(p), Some(n)) if p == n => glob_match(&pattern[1..], &name[1..]),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_unit_path_matches_systema_encoding() {
        assert_eq!(encode_unit_path("nginx.service"), "nginx_2eservice");
        assert_eq!(encode_unit_path("-.slice"), "_2d_2eslice");
        assert_eq!(encode_unit_path("dev-sda1.mount"), "dev_2dsda1_2emount");
        assert_eq!(encode_unit_path("plain"), "plain");
    }

    #[test]
    fn parse_job_type_accepts_all_systemd_types() {
        assert_eq!(parse_job_type("start").unwrap(), ("start", false));
        assert_eq!(parse_job_type("stop").unwrap(), ("stop", false));
        assert_eq!(parse_job_type("restart").unwrap(), ("restart", false));
        assert_eq!(parse_job_type("reload").unwrap(), ("reload", false));
        assert_eq!(parse_job_type("try-restart").unwrap(), ("try-restart", false));
        assert_eq!(parse_job_type("try-reload").unwrap(), ("try-reload", false));
        assert_eq!(parse_job_type("reload-or-start").unwrap(), ("reload-or-start", false));
        assert_eq!(parse_job_type("verify-active").unwrap(), ("verify-active", false));
        assert_eq!(parse_job_type("nop").unwrap(), ("nop", false));
        assert_eq!(parse_job_type("reload-or-restart").unwrap(), ("restart", true));
        assert_eq!(
            parse_job_type("reload-or-try-restart").unwrap(),
            ("try-restart", true)
        );
        assert!(parse_job_type("bogus").is_err());
    }

    #[test]
    fn parse_job_mode_accepts_all_systemd_modes() {
        for mode in [
            "fail",
            "lenient",
            "replace",
            "replace-irreversibly",
            "isolate",
            "flush",
            "ignore-dependencies",
            "ignore-requirements",
            "triggering",
            "restart-dependencies",
            "queue",
        ] {
            assert!(parse_job_mode(mode).is_ok(), "mode {mode} accepted");
        }
        assert!(parse_job_mode("bogus").is_err());
    }

    #[test]
    fn glob_matching() {
        assert!(matches_glob("sshd.service", "sshd.service"));
        assert!(matches_glob("ssh*.service", "sshd.service"));
        assert!(matches_glob("s?hd.service", "sshd.service"));
        assert!(!matches_glob("s?hd.service", "sshd2.service"));
        assert!(!matches_glob("*.socket", "sshd.service"));
        assert!(matches_glob("*", "anything"));
    }

    #[test]
    fn unit_info_from_snapshot_defaults() {
        let snap = UnitSnapshot {
            name: "foo.service".to_string(),
            description: "Foo".to_string(),
            load_state: "loaded".to_string(),
            active_state: "active".to_string(),
            sub_state: "running".to_string(),
            running_job_id: 42,
            ..Default::default()
        };
        let info = unit_info_entry(&snap);
        assert_eq!(info.0, "foo.service");
        assert_eq!(info.1, "Foo");
        assert_eq!(info.2, "loaded");
        assert_eq!(info.3, "active");
        assert_eq!(info.4, "running");
        assert_eq!(info.7, 42);
        assert_eq!(info.9.as_str(), "/org/freedesktop/systemd1/job/42");
    }

    #[test]
    fn properties_to_bag_flattens_string_arrays() {
        use zvariant::Value;
        let s = |v: &str| OwnedValue::try_from(Value::new(v)).unwrap();
        let arr = OwnedValue::try_from(Value::new(vec!["a", "b", "c"])).unwrap();
        let props = vec![
            ("After".to_string(), arr),
            ("Description".to_string(), s("Hi")),
            ("DefaultDependencies".to_string(), OwnedValue::from(true)),
        ];
        let bag = properties_to_bag(&props).expect("bag");
        assert_eq!(bag.properties.len(), 5);
        assert!(bag
            .properties
            .iter()
            .any(|p| p.key == "After" && p.value.as_ref().is_some()));
        assert!(bag
            .properties
            .iter()
            .any(|p| p.key == "Description"
                && matches!(p.value.as_ref().unwrap().value, Some(sysa::proto::manager_value::Value::S(_)))));
        assert!(bag.properties.iter().any(|p| p.key == "DefaultDependencies"));
    }

    /// Open a pidfd naming `pid`.  Needs Linux 5.3+.
    fn pidfd_for(pid: u32) -> std::os::fd::OwnedFd {
        use std::os::fd::FromRawFd;
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
        assert!(
            raw >= 0,
            "pidfd_open({pid}): {}",
            std::io::Error::last_os_error()
        );
        unsafe { std::os::fd::OwnedFd::from_raw_fd(raw as std::os::fd::RawFd) }
    }

    #[test]
    fn pid_from_pidfd_reads_the_pid_it_names() {
        let fd = pidfd_for(std::process::id());
        assert_eq!(
            pid_from_pidfd(fd.as_raw_fd()).expect("fdinfo readable"),
            Some(std::process::id())
        );
    }

    #[test]
    fn pid_from_pidfd_rejects_ordinary_descriptors() {
        // An ordinary descriptor's fdinfo carries no `Pid:` line at all, and
        // that absence is the only thing separating a pidfd from the rest.
        let file = std::fs::File::open("/proc/self").expect("open /proc/self");
        assert_eq!(
            pid_from_pidfd(file.as_raw_fd()).expect("fdinfo readable"),
            None
        );
    }

    /// `PIDFDs=` (ah) is how logind names a session scope's processes.  The
    /// control protocol carries no file descriptors, so the bridge has to
    /// resolve each one to the PID it names before the request goes over IPC.
    #[test]
    fn properties_to_bag_resolves_pidfds_into_pids() {
        let own = pidfd_for(std::process::id());
        let mut arr = zvariant::Array::new(zvariant::Signature::from_str_unchecked("h"));
        arr.append(zvariant::Value::Fd(zvariant::Fd::Owned(own)))
            .expect("h element inside ah array");
        let pidfd_list = OwnedValue::try_from(zvariant::Value::Array(arr)).expect("owned array");

        let bag = properties_to_bag(&[("PIDFDs".to_string(), pidfd_list)]).expect("bag");
        let resolved: Vec<_> = bag
            .properties
            .iter()
            .filter(|p| p.key == "PIDFDs")
            .collect();

        assert_eq!(resolved.len(), 1);
        assert!(matches!(
            resolved[0].value.as_ref().and_then(|v| v.value.as_ref()),
            Some(sysa::proto::manager_value::Value::U(pid)) if *pid == std::process::id() as u64
        ));
    }

    /// A descriptor that is not a pidfd must be dropped, not turned into a
    /// plausible-looking PID.
    #[test]
    fn properties_to_bag_drops_non_pidfd_handles() {
        let file = std::fs::File::open("/proc/self").expect("open /proc/self");
        let mut arr = zvariant::Array::new(zvariant::Signature::from_str_unchecked("h"));
        arr.append(zvariant::Value::Fd(zvariant::Fd::Owned(file.into())))
            .expect("h element inside ah array");
        let value = OwnedValue::try_from(zvariant::Value::Array(arr)).expect("owned array");

        let bag = properties_to_bag(&[("PIDFDs".to_string(), value)]).expect("bag");
        assert!(bag.properties.iter().all(|p| p.key != "PIDFDs"));
    }
}
