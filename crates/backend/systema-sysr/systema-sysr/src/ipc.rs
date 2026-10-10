//! IPC entry point for the System R worker.
//!
//! Mirrors the pattern of System M: connect to System A, register as the
//! `slice` resource worker, and dispatch method calls to the
//! [`ResourceWorker`].  Resource control is event-driven: on every
//! connection the worker subscribes to unit events for **all** units (so
//! service cgroups are managed too, even though System R does not own
//! `service` jobs), and `event.publish` envelopes carrying a
//! [`UnitResourceEvent`] are applied through the worker.
//!
//! User slices are driven by `user.sessions` envelopes carrying a
//! [`UserSessionEvent`] (the absolute login-session count per UID, sent by
//! the session manager): the worker creates the user's `user-<UID>.slice`
//! inside the always-present `user.slice` root on login and releases it on
//! logout.
//!
//! The cgroup backend is (re)built on every connection attempt so a cgroup
//! filesystem that appears later is picked up on reconnect.
//!
//! The hierarchy itself does not depend on that rebuild: [`run`] snapshots
//! the cgroup tree before the first connection can create anything in it, and
//! puts it back once the IPC loop returns.  System Init stops every worker
//! before it stops System A, so the restore has run by the time System A
//! exits and the tree is again the one the boot started from.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use prost::Message as ProstMessage;
use sysa::proto::{Envelope, UnitDefineRequest, UnitDefineResult, UnitResourceEvent, UserSessionEvent};
use sysa::worker_ipc::{EventPublisher, WorkerIpc};
use tracing::{debug, warn};

use crate::worker::{ResourceWorker, new_registry, new_user_sessions};

const WORKER_ID: &str = "system-r-1";
/// System R owns slice jobs; service cgroups are managed purely through the
/// event subscription (System S owns `service` and spawns the processes).
const WORKER_UNIT_TYPES: &[&str] = &["slice"];

/// Run the System R worker IPC loop (reconnecting on failure) until the
/// process is stopped.
pub async fn run() -> Result<()> {
    let registry = new_registry();
    // User-session counts survive IPC reconnects (like the registry), so a
    // logout delivered after a reconnect can still release the user slice.
    let sessions = new_user_sessions();

    // The controller factory creates a fresh worker per connection (with a
    // fresh EventPublisher); the same worker is stashed here so the
    // event.publish handler below can reach it.
    let current: Arc<Mutex<Option<ResourceWorker>>> = Arc::new(Mutex::new(None));
    let current_for_handler = current.clone();

    // The hierarchy as System R found it, taken before the first connection
    // gets the chance to `ensure` anything: this is the baseline that
    // `restore` puts back on the way out.  `None` means there is no cgroup
    // filesystem here to begin with, so there is nothing to restore either.
    let controller = systema_sysr_linux::linux_controller();
    let baseline = controller.snapshot();

    let result = WorkerIpc::new(WORKER_ID, WORKER_UNIT_TYPES)
        .supports_unit_define()
        .run(
            move |event_pub| {
                let worker =
                    ResourceWorker::with_defaults(registry.clone(), sessions.clone(), event_pub.clone());
                // Subscribe to events for every unit (empty list = all).
                // System A replays the currently active units on subscribe.
                event_pub.subscribe_units(&[]);
                debug!(
                    "System R subscribed to unit events (all); resource control available={}",
                    worker.available()
                );
                // `user.slice` is a static container: ensure it exists and
                // is known to System A from the moment System R is up (like
                // systemd's boot-time unit).  The finder registration runs
                // on a fresh connection, so it is spawned; re-committing
                // user slices of still-logged-in users covers registrations
                // that failed while System A was unreachable.
                worker.ensure_user_slice_root();
                spawn_user_slice_registration(&worker);
                if worker.available() {
                    // Push cgroup runtime metrics to System A every second.
                    worker.start_metrics_sampler(std::time::Duration::from_secs(1));
                }
                *current.lock().unwrap() = Some(worker.clone());
                worker
            },
            move |env, event_pub| {
                if env.method == "event.publish" {
                    handle_resource_envelope(env, &current_for_handler);
                    return Ok(true);
                }
                if env.method == "user.sessions" {
                    handle_user_sessions_envelope(env, &current_for_handler);
                    return Ok(true);
                }
                if env.method == "unit.define" {
                    handle_unit_define_envelope(env, event_pub);
                    return Ok(true);
                }
                Ok(false)
            },
        )
        .await;

    // The last thing System R does before exiting.  Every way the loop can
    // end — the `worker.exit` handshake, a System A that went away, a fatal
    // error — returns through here, and System Init does not stop System A
    // until every worker has been reaped, so by the time System A exits the
    // cgroups are back in the state the baseline recorded at boot.
    if let Some(baseline) = &baseline {
        if let Err(e) = controller.restore(baseline) {
            warn!("Cannot restore the cgroup hierarchy: {e}");
        }
    }

    result
}

/// Spawn the registration of `user.slice` plus every currently active user
/// slice with System A (fire-and-forget; finder calls are best-effort).
fn spawn_user_slice_registration(worker: &ResourceWorker) {
    let worker = worker.clone();
    tokio::spawn(async move {
        if let Err(e) = crate::register::commit_slice(
            "user.slice",
            &crate::register::static_user_slice_description(),
        )
        .await
        {
            warn!("Cannot register user.slice with System A: {e}");
        }
        let uids: Vec<u32> = worker.active_user_slice_uids();
        for uid in uids {
            let slice_name = systema_sysr_common::user_slice_name(uid);
            if let Err(e) = crate::register::commit_slice(
                &slice_name,
                &crate::register::user_slice_description(uid),
            )
            .await
            {
                warn!("Cannot register {slice_name} with System A: {e}");
            }
        }
    });
}

/// Decode an `event.publish` envelope and apply the resource event through
/// the current worker.
fn handle_resource_envelope(
    env: &Envelope,
    current: &Arc<Mutex<Option<ResourceWorker>>>,
) {
    let event = match UnitResourceEvent::decode(env.payload.as_slice()) {
        Ok(e) => e,
        Err(e) => {
            warn!("Cannot decode UnitResourceEvent from System A: {e}");
            return;
        }
    };
    let worker = current.lock().unwrap();
    match worker.as_ref() {
        Some(worker) => worker.handle_resource_event(&event),
        None => warn!("event.publish for {} before worker initialised", event.unit_name),
    }
}

/// Decode a `user.sessions` envelope (login/logout accounting from the
/// session manager) and apply it through the current worker.  The slice
/// unit is committed to System A (finder API, needs its own connection)
/// before the lifecycle handling, so the worker runs the event on a
/// spawned task.
fn handle_user_sessions_envelope(
    env: &Envelope,
    current: &Arc<Mutex<Option<ResourceWorker>>>,
) {
    let event = match UserSessionEvent::decode(env.payload.as_slice()) {
        Ok(e) => e,
        Err(e) => {
            warn!("Cannot decode UserSessionEvent: {e}");
            return;
        }
    };
    let worker = current.lock().unwrap().clone();
    match worker {
        Some(worker) => {
            tokio::spawn(async move {
                worker.handle_user_session_event_registered(&event).await;
            });
        }
        None => warn!("user.sessions for uid {} before worker initialised", event.uid),
    }
}

/// Handle a `unit.define` request: synthesize the definition (and parent
/// chain) of every requested slice name and reply with the JSON-serialized
/// `unit_id → UnitIR` map, echoing the request_id.  Non-slice names are
/// refused — this protocol exists for dynamic units only.
fn handle_unit_define_envelope(env: &Envelope, event_pub: &EventPublisher) {
    let req = match UnitDefineRequest::decode(env.payload.as_slice()) {
        Ok(r) => r,
        Err(e) => {
            warn!("Cannot decode UnitDefineRequest: {e}");
            return;
        }
    };

    let mut units: HashMap<String, systema_sysf::ir::UnitIR> = HashMap::new();
    let mut invalid: Vec<String> = Vec::new();
    for name in &req.unit_names {
        match crate::register::synthesize_slice_chain(name) {
            Some(chain) => {
                for ir in chain {
                    units.insert(ir.id.clone(), ir);
                }
            }
            None => invalid.push(name.clone()),
        }
    }

    if !invalid.is_empty() {
        warn!("unit.define refused for non-slice units: {invalid:?}");
        event_pub.send_reply(
            env.request_id,
            "unit.define_result",
            UnitDefineResult {
                success: false,
                error: sysa::l10n::fmt(
                    sysa::l10n::t_("cannot synthesize definitions for non-slice units: {invalid}"),
                    &[("invalid", &format!("{:?}", invalid))],
                ),
                units_json: vec![],
            },
        );
        return;
    }

    let units_json = match serde_json::to_vec(&units) {
        Ok(json) => json,
        Err(e) => {
            warn!("Cannot serialise synthesized slice definitions: {e}");
            event_pub.send_reply(
                env.request_id,
                "unit.define_result",
                UnitDefineResult {
                    success: false,
                    error: sysa::l10n::fmt(
                        sysa::l10n::t_("cannot serialise synthesized definitions: {e}"),
                        &[("e", &e.to_string())],
                    ),
                    units_json: vec![],
                },
            );
            return;
        }
    };
    debug!(
        "unit.define answered: {} definition(s) for {:?}",
        units.len(),
        req.unit_names
    );
    event_pub.send_reply(
        env.request_id,
        "unit.define_result",
        UnitDefineResult {
            success: true,
            error: String::new(),
            units_json,
        },
    );
}
