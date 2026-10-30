use anyhow::{bail, Result};
use sysa::controller::{UnitController, UnitStatus};
use sysa::worker_ipc::WorkerIpc;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::warn;

use crate::power::PowerAction;

pub const WORKER_ID: &str = "system-i-1";

struct PowerController {
    requests: mpsc::UnboundedSender<PowerAction>,
    pending: Arc<Mutex<HashMap<String, PowerAction>>>,
}

impl PowerController {
    fn validate(&self, unit_name: &str) -> Result<PowerAction> {
        let action = unit_name
            .strip_suffix(".power")
            .and_then(|stem| {
                PowerAction::from_unit_name(stem).filter(|action| action.to_string() == stem)
            })
            .ok_or_else(|| {
                anyhow::anyhow!(sysa::l10n::fmt(
                    sysa::l10n::t_("unknown power action: {s}"),
                    &[("s", unit_name)]
                ))
            })?;
        if self.requests.is_closed() {
            bail!(
                "{}",
                sysa::l10n::t_("System Init is not accepting power transitions.")
            );
        }
        Ok(action)
    }
}

#[async_trait::async_trait]
impl UnitController for PowerController {
    async fn status(&self, unit_name: &str) -> Result<UnitStatus> {
        self.validate(unit_name)?;
        Ok(UnitStatus {
            unit_name: unit_name.to_string(),
            active_state: "inactive".to_string(),
            sub_state: "dead".to_string(),
            ..Default::default()
        })
    }

    async fn start(&self, unit_name: &str, _: &[u8], invocation_id: &str) -> Result<()> {
        let action = self.validate(unit_name)?;
        let mut pending = self.pending.lock().expect("power requests mutex poisoned");
        if pending.contains_key(invocation_id) {
            bail!(
                "{}",
                sysa::l10n::fmt(
                    sysa::l10n::t_("Power transition already pending for invocation {id}."),
                    &[("id", invocation_id)]
                )
            );
        }
        pending.insert(invocation_id.to_string(), action);
        Ok(())
    }

    async fn stop(&self, unit_name: &str) -> Result<()> {
        self.validate(unit_name)?;
        Ok(())
    }

    async fn restart(&self, unit_name: &str, _: &[u8], invocation_id: &str) -> Result<()> {
        self.start(unit_name, &[], invocation_id).await
    }

    async fn reload(&self, unit_name: &str, _: &[u8]) -> Result<()> {
        self.validate(unit_name)?;
        Ok(())
    }
}

pub struct PowerWorker {
    shutdown: oneshot::Sender<()>,
    task: JoinHandle<Result<()>>,
}

impl PowerWorker {
    pub fn spawn() -> (Self, mpsc::UnboundedReceiver<PowerAction>) {
        let (requests, receiver) = mpsc::unbounded_channel();
        let (shutdown, shutdown_rx) = oneshot::channel();
        let callback_requests = requests.clone();
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let callback_pending = pending.clone();
        let reconnect_pending = pending.clone();
        let ipc = WorkerIpc::new(WORKER_ID, &["power"])
            .on_reconnect(move || {
                let pending = reconnect_pending.clone();
                async move {
                    pending
                        .lock()
                        .expect("power requests mutex poisoned")
                        .clear();
                }
            })
            .on_method_result(move |call, result, queued| {
                if result.success && matches!(call.method.as_str(), "start" | "restart") {
                    let action = callback_pending
                        .lock()
                        .expect("power requests mutex poisoned")
                        .remove(&call.invocation_id);
                    if let Some(action) = action.filter(|_| queued) {
                        if callback_requests.send(action).is_err() {
                            warn!(
                                "Power transition receiver closed after method result was queued"
                            );
                        }
                    }
                }
            });
        let task = tokio::spawn(async move {
            ipc.run_until_shutdown(
                async move {
                    let _ = shutdown_rx.await;
                    "System Init shutdown"
                },
                move |_| PowerController {
                    requests: requests.clone(),
                    pending: pending.clone(),
                },
                |_, _| Ok(false),
            )
            .await
        });
        (Self { shutdown, task }, receiver)
    }

    pub async fn disconnect(self) {
        let _ = self.shutdown.send(());
        match self.task.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!("Power worker failed during shutdown: {e:#}"),
            Err(e) => warn!("Power worker task failed: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn controller_validates_and_reports_inactive() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let controller = PowerController {
            requests: tx,
            pending: Arc::new(Mutex::new(HashMap::new())),
        };
        let status = controller.status("halt.power").await.unwrap();
        assert_eq!(
            (status.active_state.as_str(), status.sub_state.as_str()),
            ("inactive", "dead")
        );
        controller.start("halt.power", &[], "1").await.unwrap();
        controller.restart("reboot.power", &[], "2").await.unwrap();
        assert_eq!(
            controller.pending.lock().unwrap().get("1"),
            Some(&PowerAction::Halt)
        );
        assert_eq!(
            controller.pending.lock().unwrap().get("2"),
            Some(&PowerAction::Reboot)
        );
        assert!(controller.start("halt.power", &[], "1").await.is_err());
        controller.stop("halt.power").await.unwrap();
        controller.reload("halt.power", &[]).await.unwrap();
        assert!(rx.try_recv().is_err());
        assert!(controller.start("unknown.power", &[], "").await.is_err());
        assert!(controller.start("halt.power.extra", &[], "").await.is_err());
        assert!(controller.start("halt.power.power", &[], "").await.is_err());
        drop(rx);
        assert!(controller.start("halt.power", &[], "").await.is_err());
    }
}
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
