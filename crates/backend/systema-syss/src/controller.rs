use crate::dbus::DbusWaiter;
use crate::notify::NotifyManager;
use crate::process::{start_service, stop_service};
use crate::state::{ServiceRegistry, ServiceState};
use anyhow::{anyhow, Context, Result};
use std::collections::HashMap;
use std::sync::Arc;
use sysa::controller::{decode_unit_config, UnitController, UnitStatus};
use sysa::proto::UnitConfig;
use sysa::worker_ipc::EventPublisher;
use tokio::sync::Mutex;
use tracing::{info, warn};

#[derive(Clone)]
pub struct ServiceController {
    registry: ServiceRegistry,
    event_pub: EventPublisher,
    fdpass: Arc<Mutex<Option<tokio::net::UnixStream>>>,
    notify: Option<NotifyManager>,
    dbus: DbusWaiter,
}

impl ServiceController {
    pub fn new(
        registry: ServiceRegistry,
        event_pub: EventPublisher,
        fdpass: Arc<Mutex<Option<tokio::net::UnixStream>>>,
        notify: Option<NotifyManager>,
        dbus: DbusWaiter,
    ) -> Self {
        ServiceController {
            registry,
            event_pub,
            fdpass,
            notify,
            dbus,
        }
    }

    /// Whether the unit is `Type=notify` / `Type=notify-reload` and thus
    /// must report `READY=1` (sd_notify) before its start job completes.
    fn is_notify_type(cfg: &UnitConfig) -> bool {
        cfg.service
            .as_ref()
            .map(|s| matches!(s.service_type.as_str(), "notify" | "notify-reload"))
            .unwrap_or(false)
    }

    /// Wait for the service's readiness notification (Type=notify(-reload)).
    /// On failure the service is stopped and the unit is marked failed.
    async fn await_notify_start(&self, unit_name: &str, pid: u32, cfg: &UnitConfig) -> Result<()> {
        let Some(notify) = self.notify.as_ref() else {
            return Ok(());
        };
        if !Self::is_notify_type(cfg) {
            return Ok(());
        }
        let timeout = cfg
            .service
            .as_ref()
            .map(|s| s.timeout_start_secs.max(1))
            .unwrap_or(90) as u64;
        notify.register_start(unit_name, pid);
        match notify.wait_ready(unit_name, pid, timeout).await {
            Ok(()) => {
                info!("{} (PID {}): reported READY=1", unit_name, pid);
                Ok(())
            }
            Err(e) => {
                warn!("{} (PID {}): notify start failed: {}", unit_name, pid, e);
                let _ = stop_service(self.registry.clone(), unit_name, 10).await;
                self.publish_state(unit_name);
                Err(anyhow!("{}", e))
            }
        }
    }

    /// Whether the unit has nothing to run: an ExecStart-less "action"
    /// service (no `[Service]` section or an empty `ExecStart=`).  In
    /// systemd such units are valid only when they carry a `SuccessAction=`/
    /// `FailureAction=` (e.g. `systemd-poweroff.service`, which exists purely
    /// to trigger `poweroff-force` on success); starting them completes
    /// immediately with no process being spawned.
    fn is_noop_service(cfg: &UnitConfig) -> bool {
        match cfg.service.as_ref() {
            None => true,
            Some(s) => s.exec_start.is_empty(),
        }
    }

    /// Mark an ExecStart-less "action" service as started (no-process
    /// equivalent of a oneshot that exits 0): the unit becomes active and
    /// stays so, like `RemainAfterExit=yes`.
    fn mark_noop_started(&self, unit_name: &str, invocation_id: Option<String>) {
        let mut reg = self.registry.lock();
        let inst = reg.entry(unit_name.to_string()).or_default();
        inst.state = ServiceState::Running;
        inst.main_pid = None;
        inst.invocation_id = invocation_id;
        drop(reg);
        self.publish_state(unit_name);
    }

    /// Whether the unit is `Type=oneshot` and thus the start job must wait
    /// for the ExecStart process to exit (like systemd).
    fn is_oneshot_type(cfg: &UnitConfig) -> bool {
        cfg.service
            .as_ref()
            .map(|s| s.service_type.as_str() == "oneshot")
            .unwrap_or(false)
    }

    /// Wait for the oneshot service's ExecStart process to exit.
    /// For `Type=oneshot` the start job completes only once the process
    /// exits successfully, mirroring systemd's
    /// `service_enter_start()` → `service_connect_watch_pid()` flow.
    /// On failure or timeout the unit is marked failed.
    async fn await_oneshot_exit(
        &self,
        unit_name: &str,
        pid: u32,
        mut child: tokio::process::Child,
        cfg: &UnitConfig,
    ) -> Result<()> {
        if !Self::is_oneshot_type(cfg) {
            return Ok(());
        }
        let timeout = cfg
            .service
            .as_ref()
            .map(|s| s.timeout_start_secs.max(1))
            .unwrap_or(90) as u64;
        match tokio::time::timeout(std::time::Duration::from_secs(timeout), child.wait()).await {
            Ok(Ok(status)) => {
                info!(
                    "Service {} (PID {}) exited: code={:?}, success={}",
                    unit_name,
                    pid,
                    status.code(),
                    status.success()
                );
                let stay_active = cfg
                    .service
                    .as_ref()
                    .map(|s| status.success() && s.remain_after_exit)
                    .unwrap_or(false);
                if stay_active {
                    info!(
                        "Service {}: RemainAfterExit=yes, keeping unit active after successful exit",
                        unit_name
                    );
                    {
                        let mut reg = self.registry.lock();
                        if let Some(inst) = reg.get_mut(unit_name) {
                            inst.state = ServiceState::Running;
                            inst.main_pid = None;
                            inst.last_exit_code = status.code();
                        }
                    }
                } else {
                    let state = if status.success() {
                        ServiceState::Dead
                    } else {
                        ServiceState::Failed
                    };
                    {
                        let mut reg = self.registry.lock();
                        if let Some(inst) = reg.get_mut(unit_name) {
                            inst.state = state;
                            inst.main_pid = None;
                            inst.last_exit_code = status.code();
                            inst.invocation_id = None;
                        }
                    }
                }
                self.publish_state(unit_name);
                if !status.success() {
                    return Err(anyhow!(sysa::l10n::fmt(
                        sysa::l10n::t_("oneshot service {unit_name} exited with code {code}"),
                        &[
                            ("unit_name", &unit_name.to_string()),
                            ("code", &format!("{:?}", status.code()))
                        ]
                    )));
                }
                Ok(())
            }
            Ok(Err(e)) => {
                warn!("Error waiting for oneshot {}: {}", unit_name, e);
                {
                    let mut reg = self.registry.lock();
                    if let Some(inst) = reg.get_mut(unit_name) {
                        inst.state = ServiceState::Failed;
                        inst.main_pid = None;
                        inst.invocation_id = None;
                    }
                }
                self.publish_state(unit_name);
                Err(anyhow!(sysa::l10n::fmt(
                    sysa::l10n::t_("oneshot service {unit_name} error: {e}"),
                    &[("unit_name", &unit_name.to_string()), ("e", &e.to_string())]
                )))
            }
            Err(_) => {
                warn!(
                    "Timeout waiting for oneshot {} ({}s), stopping",
                    unit_name, timeout
                );
                let _ = stop_service(self.registry.clone(), unit_name, 10).await;
                self.publish_state(unit_name);
                Err(anyhow!(sysa::l10n::fmt(
                    sysa::l10n::t_("oneshot service {unit_name} timed out after {timeout}s"),
                    &[
                        ("unit_name", &unit_name.to_string()),
                        ("timeout", &timeout.to_string())
                    ]
                )))
            }
        }
    }

    /// Whether the unit is `Type=dbus` and thus must own its `BusName=`
    /// before the start job completes.
    fn is_dbus_type(cfg: &UnitConfig) -> bool {
        cfg.service
            .as_ref()
            .map(|s| s.service_type.as_str() == "dbus")
            .unwrap_or(false)
    }

    /// Wait for the service to acquire its `BusName=` on the system bus
    /// (Type=dbus).  On failure the service is stopped and the unit is
    /// marked failed, like systemd's TimeoutStartSec kill.
    async fn await_dbus_start(&self, unit_name: &str, pid: u32, cfg: &UnitConfig) -> Result<()> {
        if !Self::is_dbus_type(cfg) {
            return Ok(());
        }
        let svc = cfg.service.as_ref().expect("dbus type implies service");
        let bus_name = svc.bus_name.as_str();
        // systemd refuses such a unit at load (service_verify()); the sysa
        // parser enforces the same.  Defend in depth: never wait on an
        // empty name.
        if bus_name.is_empty() {
            return Err(anyhow!(sysa::l10n::t_(
                "Service is of type D-Bus but no D-Bus service name has been specified. Refusing."
            )));
        }
        let timeout = svc.timeout_start_secs.max(1) as u64;
        match self.dbus.wait_name_owned(bus_name, pid, timeout).await {
            Ok(()) => {
                info!(
                    "{} (PID {}): acquired D-Bus name {}",
                    unit_name, pid, bus_name
                );
                Ok(())
            }
            Err(e) => {
                warn!("{} (PID {}): dbus start failed: {}", unit_name, pid, e);
                let _ = stop_service(self.registry.clone(), unit_name, 10).await;
                self.publish_state(unit_name);
                Err(anyhow!("{}", e))
            }
        }
    }

    /// Ask the socket worker (via the allocator) for the listener fds of the
    /// given socket units, in order.  Each request is a `socket.request_fd`
    /// envelope; the matching fd arrives back on our fdpass channel.
    #[cfg(unix)]
    async fn request_listener_fds(&self, socket_units: &[String]) -> Vec<std::os::unix::io::RawFd> {
        use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
        let stream = {
            let guard = self.fdpass.lock().await;
            match guard.as_ref() {
                Some(s) => {
                    let dup = nix::unistd::dup(s.as_raw_fd()).ok();
                    dup.and_then(|fd| {
                        let std_stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) };
                        tokio::net::UnixStream::from_std(std_stream).ok()
                    })
                }
                None => None,
            }
        };
        let Some(stream) = stream else {
            return Vec::new();
        };
        let mut fds: Vec<RawFd> = Vec::new();
        for unit in socket_units {
            if unit.is_empty() {
                continue;
            }
            self.event_pub
                .send_envelope_bytes("socket.request_fd", unit.as_bytes().to_vec());
            match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                sysa::ipc::recv_fd(&stream),
            )
            .await
            {
                Ok(Ok(fd)) => fds.push(fd),
                Ok(Err(e)) => {
                    warn!("recv_fd for '{}' failed: {}", unit, e);
                    break;
                }
                Err(_) => {
                    warn!("Timed out waiting for listener fd of '{}'", unit);
                    break;
                }
            }
        }
        fds
    }

    fn status_of(&self, unit_name: &str) -> UnitStatus {
        let reg = self.registry.lock();
        match reg.get(unit_name) {
            Some(inst) => {
                let mut extensions = HashMap::new();
                if let Some(code) = inst.last_exit_code {
                    extensions.insert("last_exit_code".to_string(), code.to_string());
                }
                UnitStatus {
                    unit_name: unit_name.to_string(),
                    active_state: match inst.state {
                        ServiceState::Dead => "inactive",
                        ServiceState::Failed => "failed",
                        ServiceState::Running => "active",
                        ServiceState::Starting => "activating",
                        ServiceState::Stopping => "deactivating",
                    }
                    .to_string(),
                    sub_state: inst.state.as_str().to_string(),
                    main_pid: inst.main_pid.unwrap_or(0),
                    invocation_id: inst.invocation_id.clone().unwrap_or_default(),
                    extensions,
                }
            }
            None => UnitStatus {
                unit_name: unit_name.to_string(),
                active_state: "inactive".to_string(),
                sub_state: "dead".to_string(),
                main_pid: 0,
                invocation_id: String::new(),
                extensions: HashMap::new(),
            },
        }
    }

    fn publish_state(&self, unit_name: &str) {
        let status = self.status_of(unit_name);
        self.event_pub
            .publish_unit_state_update(vec![status], false);
    }
}

#[async_trait::async_trait]
impl UnitController for ServiceController {
    async fn status(&self, unit_name: &str) -> Result<UnitStatus> {
        Ok(self.status_of(unit_name))
    }

    async fn sync_state(&self) -> Vec<UnitStatus> {
        let names: Vec<String> = {
            let guard = self.registry.lock();
            guard.keys().cloned().collect()
        };
        names.iter().map(|n| self.status_of(n)).collect()
    }

    async fn start(&self, unit_name: &str, config: &[u8], invocation_id: &str) -> Result<()> {
        let cfg = decode_unit_config(config)?;
        let inv_id = if invocation_id.is_empty() {
            None
        } else {
            Some(invocation_id.to_string())
        };
        if Self::is_noop_service(&cfg) {
            // ExecStart-less "action" service (e.g. systemd-poweroff.service,
            // which only carries SuccessAction=poweroff-force).  There is
            // nothing to spawn: the start job completes immediately, exactly
            // like systemd, so the unit's SuccessAction can fire.
            info!(
                "{}: no ExecStart, start completes without spawning a process",
                unit_name
            );
            self.mark_noop_started(unit_name, inv_id);
            return Ok(());
        }
        #[cfg(unix)]
        let listen_fds = self.request_listener_fds(&cfg.socket_units).await;
        #[cfg(not(unix))]
        let listen_fds = Vec::new();
        let (_pid, child) = start_service(self.registry.clone(), &cfg, inv_id, listen_fds).await?;
        {
            let mut reg = self.registry.lock();
            if let Some(inst) = reg.get_mut(unit_name) {
                inst.timeout_stop_secs = cfg.service.as_ref().map(|s| s.timeout_stop_secs);
                inst.remain_after_exit = cfg
                    .service
                    .as_ref()
                    .map(|s| s.remain_after_exit)
                    .unwrap_or(false);
            }
        }
        self.publish_state(unit_name);
        // The start job completion semantics depend on the service type:
        // - Type=oneshot: the start job completes only when ExecStart
        //   exits successfully (like systemd's service_enter_start()).
        // - Type=notify(-reload): the start job completes when READY=1
        //   is received (like systemd's service_enter_start_post()).
        // - Type=dbus: the start job completes when BusName= is owned.
        // - Others (simple/forking/etc.): the start job completes as
        //   soon as the process is spawned.
        if Self::is_oneshot_type(&cfg) {
            self.await_oneshot_exit(unit_name, _pid, child, &cfg)
                .await?;
        } else {
            tokio::spawn(crate::ipc::monitor_service(
                self.registry.clone(),
                unit_name.to_string(),
                self.event_pub.clone(),
                child,
            ));
            self.await_notify_start(unit_name, _pid, &cfg).await?;
            self.await_dbus_start(unit_name, _pid, &cfg).await?;
        }
        Ok(())
    }

    async fn stop(&self, unit_name: &str) -> Result<()> {
        let timeout = {
            let reg = self.registry.lock();
            reg.get(unit_name)
                .and_then(|inst| inst.timeout_stop_secs)
                .unwrap_or(30)
        };
        stop_service(self.registry.clone(), unit_name, timeout).await?;
        self.publish_state(unit_name);
        Ok(())
    }

    async fn restart(&self, unit_name: &str, config: &[u8], invocation_id: &str) -> Result<()> {
        let cfg = decode_unit_config(config)?;
        if Self::is_noop_service(&cfg) {
            // ExecStart-less "action" service: restart is a no-op success
            // (there was never a process to stop or start).
            info!("{}: no ExecStart, restart completes without spawning a process", unit_name);
            let inv_id = if invocation_id.is_empty() {
                None
            } else {
                Some(invocation_id.to_string())
            };
            self.mark_noop_started(unit_name, inv_id);
            return Ok(());
        }
        let timeout = cfg
            .service
            .as_ref()
            .map(|s| s.timeout_stop_secs)
            .unwrap_or(30);
        stop_service(self.registry.clone(), unit_name, timeout).await?;
        let inv_id = if invocation_id.is_empty() {
            None
        } else {
            Some(invocation_id.to_string())
        };
        #[cfg(unix)]
        let listen_fds = self.request_listener_fds(&cfg.socket_units).await;
        #[cfg(not(unix))]
        let listen_fds = Vec::new();
        let (_pid, child) = start_service(self.registry.clone(), &cfg, inv_id, listen_fds).await?;
        {
            let mut reg = self.registry.lock();
            if let Some(inst) = reg.get_mut(unit_name) {
                inst.timeout_stop_secs = cfg.service.as_ref().map(|s| s.timeout_stop_secs);
                inst.remain_after_exit = cfg
                    .service
                    .as_ref()
                    .map(|s| s.remain_after_exit)
                    .unwrap_or(false);
            }
        }
        self.publish_state(unit_name);
        if Self::is_oneshot_type(&cfg) {
            self.await_oneshot_exit(unit_name, _pid, child, &cfg)
                .await?;
        } else {
            tokio::spawn(crate::ipc::monitor_service(
                self.registry.clone(),
                unit_name.to_string(),
                self.event_pub.clone(),
                child,
            ));
            self.await_notify_start(unit_name, _pid, &cfg).await?;
            self.await_dbus_start(unit_name, _pid, &cfg).await?;
        }
        Ok(())
    }

    async fn reload(&self, unit_name: &str, config: &[u8]) -> Result<()> {
        let cfg = decode_unit_config(config)?;
        let pid = {
            let reg = self.registry.lock();
            reg.get(unit_name).and_then(|i| i.main_pid)
        };
        let Some(pid) = pid else {
            anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_("Reload of {unit_name} failed: service is not running."),
                &[("unit_name", unit_name)],
            ))
        };
        // Type=notify-reload: the reload job waits for RELOADING=1
        // (validated against MONOTONIC_USEC) followed by READY=1, like
        // systemd's service_notify_message_process_state().  The cycle must
        // be registered before the signal goes out so no notification can
        // slip in between.
        let wait_reload = {
            let notify_reload = cfg
                .service
                .as_ref()
                .map(|s| s.service_type == "notify-reload")
                .unwrap_or(false);
            notify_reload
                && self
                    .notify
                    .as_ref()
                    .map(|n| n.register_reload(unit_name, pid))
                    .unwrap_or(false)
        };
        #[cfg(unix)]
        {
            use nix::sys::signal;
            use nix::unistd::Pid;
            signal::kill(Pid::from_raw(pid as i32), signal::Signal::SIGHUP)
                .context(sysa::l10n::t_("Failed to send SIGHUP"))?;
        }
        if wait_reload {
            let timeout = cfg
                .service
                .as_ref()
                .map(|s| s.timeout_start_secs.max(1))
                .unwrap_or(90) as u64;
            let notify = self.notify.as_ref().unwrap();
            notify
                .wait_reload(unit_name, pid, timeout)
                .await
                .map_err(|e| {
                    warn!("{} (PID {}): notify reload failed: {}", unit_name, pid, e);
                    anyhow!("{}", e)
                })?;
            info!("{} (PID {}): reload completed (READY=1)", unit_name, pid);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sysa::proto::ServiceConfig;

    fn controller() -> ServiceController {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        ServiceController::new(
            crate::state::new_registry(),
            EventPublisher::new(tx, "test-worker", Arc::default()),
            Arc::new(Mutex::new(None)),
            None,
            DbusWaiter::default(),
        )
    }

    fn dbus_cfg(bus_name: &str) -> UnitConfig {
        UnitConfig {
            unit_name: "dbus-test.service".to_string(),
            service: Some(ServiceConfig {
                service_type: "dbus".to_string(),
                bus_name: bus_name.to_string(),
                timeout_start_secs: 30,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn dbus_type_is_detected() {
        assert!(ServiceController::is_dbus_type(&dbus_cfg(
            "org.example.Daemon"
        )));
        let mut cfg = dbus_cfg("org.example.Daemon");
        cfg.service.as_mut().unwrap().service_type = "simple".to_string();
        assert!(!ServiceController::is_dbus_type(&cfg));
    }

    #[tokio::test]
    async fn non_dbus_start_is_not_waited_on() {
        let mut cfg = dbus_cfg("org.example.Daemon");
        cfg.service.as_mut().unwrap().service_type = "simple".to_string();
        let result = controller()
            .await_dbus_start("dbus-test.service", 9999, &cfg)
            .await;
        assert!(result.is_ok(), "simple services are not gated");
    }

    #[tokio::test]
    async fn empty_bus_name_is_refused() {
        // The sysa parser refuses such units at load; the worker defends in
        // depth with the same message.
        let err = controller()
            .await_dbus_start("dbus-test.service", 9999, &dbus_cfg(""))
            .await
            .expect_err("Type=dbus without BusName= must be refused");
        assert!(
            err.to_string().contains("no D-Bus service name"),
            "unexpected: {err}"
        );
    }

    fn oneshot_cfg() -> UnitConfig {
        UnitConfig {
            unit_name: "oneshot-test.service".to_string(),
            service: Some(ServiceConfig {
                service_type: "oneshot".to_string(),
                timeout_start_secs: 30,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// Register a unit in the controller's registry so state updates land.
    fn register_unit(ctrl: &ServiceController, name: &str) {
        use crate::state::ServiceInstance;
        let mut reg = ctrl.registry.lock();
        reg.insert(
            name.to_string(),
            ServiceInstance {
                state: ServiceState::Starting,
                main_pid: Some(9999),
                ..ServiceInstance::new()
            },
        );
    }

    #[tokio::test]
    async fn oneshot_type_is_detected() {
        assert!(ServiceController::is_oneshot_type(&oneshot_cfg()));
        let mut cfg = oneshot_cfg();
        cfg.service.as_mut().unwrap().service_type = "simple".to_string();
        assert!(!ServiceController::is_oneshot_type(&cfg));
    }

    #[tokio::test]
    async fn non_oneshot_start_is_not_waited_on() {
        let mut cfg = oneshot_cfg();
        cfg.service.as_mut().unwrap().service_type = "simple".to_string();
        assert!(
            !ServiceController::is_oneshot_type(&cfg),
            "simple should not be detected as oneshot"
        );
    }

    #[tokio::test]
    async fn await_oneshot_exit_success() {
        let child = tokio::process::Command::new("true")
            .spawn()
            .expect("failed to spawn true");
        let ctrl = controller();
        register_unit(&ctrl, "oneshot-test.service");
        let cfg = oneshot_cfg();
        let result = ctrl
            .await_oneshot_exit("oneshot-test.service", child.id().unwrap_or(0), child, &cfg)
            .await;
        assert!(result.is_ok(), "oneshot exit 0 should succeed: {result:?}");
        let reg = ctrl.registry.lock();
        let inst = reg.get("oneshot-test.service").expect("unit should exist");
        assert_eq!(inst.state, ServiceState::Dead);
        assert_eq!(inst.last_exit_code, Some(0));
    }

    #[tokio::test]
    async fn await_oneshot_exit_failure() {
        let child = tokio::process::Command::new("false")
            .spawn()
            .expect("failed to spawn false");
        let ctrl = controller();
        register_unit(&ctrl, "oneshot-test.service");
        let cfg = oneshot_cfg();
        let result = ctrl
            .await_oneshot_exit("oneshot-test.service", child.id().unwrap_or(0), child, &cfg)
            .await;
        assert!(result.is_err(), "oneshot exit 1 should fail");
        let reg = ctrl.registry.lock();
        let inst = reg.get("oneshot-test.service").expect("unit should exist");
        assert_eq!(inst.state, ServiceState::Failed);
        assert_eq!(inst.last_exit_code, Some(1));
    }

    #[tokio::test]
    async fn await_oneshot_exit_remain_after_exit() {
        let mut cfg = oneshot_cfg();
        cfg.service.as_mut().unwrap().remain_after_exit = true;
        let child = tokio::process::Command::new("true")
            .spawn()
            .expect("failed to spawn true");
        let ctrl = controller();
        register_unit(&ctrl, "oneshot-test.service");
        let result = ctrl
            .await_oneshot_exit("oneshot-test.service", child.id().unwrap_or(0), child, &cfg)
            .await;
        assert!(result.is_ok(), "oneshot exit 0 should succeed: {result:?}");
        let reg = ctrl.registry.lock();
        let inst = reg.get("oneshot-test.service").expect("unit should exist");
        assert_eq!(inst.state, ServiceState::Running);
    }

    #[tokio::test]
    async fn noop_service_is_detected() {
        // systemd-poweroff.service carries practically no [Service] section.
        let mut cfg = oneshot_cfg();
        cfg.service.as_mut().unwrap().exec_start = "/usr/bin/true".to_string();
        assert!(
            !ServiceController::is_noop_service(&cfg),
            "oneshot with an ExecStart is not a no-op"
        );
        cfg.service = None;
        assert!(
            ServiceController::is_noop_service(&cfg),
            "a unit with no configured service is a no-op"
        );
        cfg.service = Some(ServiceConfig {
            service_type: "simple".to_string(),
            exec_start: "/usr/bin/true".to_string(),
            ..Default::default()
        });
        assert!(
            !ServiceController::is_noop_service(&cfg),
            "ExecStart= set means there is something to run"
        );
        cfg.service.as_mut().unwrap().exec_start.clear();
        assert!(
            ServiceController::is_noop_service(&cfg),
            "empty ExecStart= is a no-op"
        );
    }

    #[tokio::test]
    async fn noop_start_marks_running_without_spawning() {
        use prost::Message;
        let ctrl = controller();
        let config = UnitConfig {
            unit_name: "systemd-poweroff.service".to_string(),
            ..Default::default()
        };
        let result = ctrl
            .start(
                "systemd-poweroff.service",
                &config.encode_to_vec(),
                "inv-1",
            )
            .await;
        assert!(result.is_ok(), "no-op start must succeed: {result:?}");
        let reg = ctrl.registry.lock();
        let inst = reg.get("systemd-poweroff.service").expect("unit should exist");
        assert_eq!(inst.state, ServiceState::Running);
        assert_eq!(inst.main_pid, None);
    }
}
