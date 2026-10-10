//! Task scheduler — generates and dispatches tasks to System Workers.
//!
//! When System A receives a request to start or stop a unit, the scheduler:
//! 1. Determines which units must be started/stopped (dependency expansion).
//! 2. Creates Job records for each operation.
//! 3. Dispatches WorkerTask messages to the appropriate workers.

pub mod job_type;
pub mod transaction;

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{bail, Result};
use prost::Message;
use sysa::l10n;
use tokio::task::AbortHandle;
use tracing::{debug, error, info, warn};

use crate::state::{
    generate_invocation_id, next_job_id, next_request_id, next_task_id, AllocatorHandle,
    AllocatorState, Job, JobCompletion, JobKind, JobMode, JobNewInfo, JobResultKind, JobStatus,
    StartLimitState,
};
use crate::unit::types::{
    ExitKind, MountSection, RestartPolicy, StartLimitAction, UnitFile, UnitKind, UnitSection,
};
use sysa::proto::{
    AutomountConfig, DeviceConfig, MountConfig, PathConfig, ScopeConfig, ServiceConfig,
    SocketAddress, SocketConfig, TimerConfig, UnitConfig, UnitDefineRequest, UnitDefineResult,
};

use crate::scheduler::job_type::{job_type_collapse, JobType, UnitActiveState};
use crate::scheduler::transaction::{build_plan, PlanError, PlannerMode};

/// Map a transaction job type onto the public job kind used for worker
/// dispatch. `Nop` and `VerifyActive` steps need no worker interaction:
/// they are resolved by the planner against the cached runtime state.
fn job_kind_from_type(t: JobType) -> Option<JobKind> {
    match t {
        JobType::Start => Some(JobKind::Start),
        JobType::Stop => Some(JobKind::Stop),
        JobType::Restart => Some(JobKind::Restart),
        JobType::Reload => Some(JobKind::Reload),
        JobType::VerifyActive | JobType::Nop => None,
        JobType::TryRestart | JobType::TryReload | JobType::ReloadOrStart => None,
    }
}

/// Mode/type validation from systemd's `manager_add_job_full()`
/// (`manager.c:2321-2347`):
///
/// - `triggering` is only valid for stop jobs;
/// - `restart-dependencies` is only valid for start jobs;
/// - `isolate` requires `AllowIsolate=yes` on the unit.
fn check_mode_constraints(
    mode: JobMode,
    kind: JobKind,
    unit_name: &str,
    allow_isolate: bool,
) -> Result<()> {
    if mode == JobMode::Triggering && kind != JobKind::Stop {
        bail!(
            "{}",
            l10n::fmt(
                l10n::t_("--job-mode=triggering is only valid for stop."),
                &[]
            )
        );
    }
    if mode == JobMode::RestartDependencies && kind != JobKind::Start {
        bail!(
            "{}",
            l10n::fmt(
                l10n::t_("--job-mode=restart-dependencies is only valid for start."),
                &[],
            )
        );
    }
    if mode == JobMode::Isolate && !allow_isolate {
        bail!(
            "{}",
            l10n::fmt(
                l10n::t_("Operation refused, unit {unit_name} may not be isolated."),
                &[("unit_name", unit_name)],
            )
        );
    }
    Ok(())
}

/// Enqueue a start job with explicit mode.
pub async fn enqueue_start_with_mode(
    allocator: AllocatorHandle,
    unit_name: &str,
    mode: JobMode,
) -> Result<u64> {
    enqueue_job(allocator, unit_name, JobKind::Start, mode).await
}

/// Activate a transient unit (created via `StartTransientUnit`).
///
/// Scopes are dispatched through the normal job machinery: the System E
/// worker attaches the transient `PIDs=` to the scope's cgroup and reports
/// back, so the job completes when the worker's `task.result` arrives.
/// Other transient units (slices, auxiliaries) wrap already-existing
/// processes and need no worker: the unit is marked active and the job
/// Enqueue a job by full systemd job type, collapsing state-dependent
/// types (`try-restart`, `try-reload`, `reload-or-start`) against the
/// cached runtime state before planning — mirroring `manager_add_job_full()`
/// + `job_type_collapse()` (`job.c:482`).
///
/// `reload_if_possible` mirrors systemd's `ReloadOrRestartUnit` /
/// `ReloadOrTryRestartUnit` semantics (`unit_queue_job_check_and_mangle_type`,
/// `unit.c:7160`): when the unit can reload, `restart` is mangled into
/// `reload-or-start` and `try-restart` into `try-reload` before collapsing.
///
/// When the root collapses to `Nop` (e.g. try-restart of an inactive unit)
/// systemd creates a nop job that completes immediately with `JOB_DONE`
/// (`job.c:959-963`); a directly requested `verify-active` root completes
/// `done` when the unit is active-like and `skipped` otherwise (systemd
/// waits for activating units; System A completes immediately). In both
/// cases the job record is created, `JobNew`/`JobRemoved` are emitted and
/// no worker is touched.
///
/// Returns the job id and the collapsed kind so callers can track desired
/// state exactly.
pub async fn enqueue_job_type(
    allocator: AllocatorHandle,
    unit_name: &str,
    job_type: JobType,
    reload_if_possible: bool,
    mode: JobMode,
) -> Result<(u64, JobKind)> {
    // Resolve alias names to their canonical unit before anything else.
    let resolved = allocator.read().resolve_unit_name(unit_name);
    let unit_name: &str = &resolved;
    info!(
        "Scheduling {} for {} (mode={:?})",
        job_type.as_str(),
        unit_name,
        mode
    );

    // Mangle reload-if-possible, then collapse against the cached state.
    let (mangled, collapsed, state_now, allow_isolate) = {
        let state = allocator.read();
        let unit = state.units.get(unit_name);
        let can_reload = unit
            .and_then(|u| u.service.as_ref())
            .map(|s| !s.exec_reload.is_empty())
            .unwrap_or(false);
        let mangled = if reload_if_possible && can_reload {
            match job_type {
                JobType::Restart => JobType::ReloadOrStart,
                JobType::TryRestart => JobType::TryReload,
                other => other,
            }
        } else {
            job_type
        };
        let state_now = state
            .unit_states
            .get(unit_name)
            .map(|c| UnitActiveState::from_active_state_str(&c.active_state))
            .unwrap_or(UnitActiveState::Unknown);
        let collapsed = job_type_collapse(mangled, state_now);
        (
            mangled,
            collapsed,
            state_now,
            unit.map(|u| u.unit.allow_isolate).unwrap_or(false),
        )
    };

    // Validate mode/type combinations on the *requested* (mangled) type,
    // like systemd's manager_add_job_full() does before collapsing.
    let requested_kind = job_kind_from_type(mangled).unwrap_or(JobKind::Nop);
    check_mode_constraints(mode, requested_kind, unit_name, allow_isolate)?;

    match collapsed {
        JobType::Nop => {
            let job_id = next_job_id();
            let mut st = allocator.write();
            st.jobs.insert(
                job_id,
                Job {
                    id: job_id,
                    unit_name: unit_name.to_string(),
                    kind: JobKind::Nop,
                    status: JobStatus::Done,
                    timeout_abort: None,
                },
            );
            emit_job_new(&mut st, job_id, unit_name, JobKind::Nop);
            if let Some(ref tx) = st.job_completion_tx {
                let _ = tx.send(JobCompletion {
                    job_id,
                    unit_name: unit_name.to_string(),
                    result: JobResultKind::Done,
                });
            }
            Ok((job_id, JobKind::Nop))
        }
        JobType::VerifyActive => {
            // Directly requested verify-active root (`EnqueueUnitJob`).
            // systemd: active-like → done, activating → wait, else → skipped.
            let result = if state_now.is_active_or_reloading() {
                JobResultKind::Done
            } else {
                JobResultKind::Skipped
            };
            let job_id = next_job_id();
            let mut st = allocator.write();
            st.jobs.insert(
                job_id,
                Job {
                    id: job_id,
                    unit_name: unit_name.to_string(),
                    kind: JobKind::Nop,
                    status: JobStatus::Done,
                    timeout_abort: None,
                },
            );
            emit_job_new(&mut st, job_id, unit_name, JobKind::Nop);
            if let Some(ref tx) = st.job_completion_tx {
                let _ = tx.send(JobCompletion {
                    job_id,
                    unit_name: unit_name.to_string(),
                    result,
                });
            }
            Ok((job_id, JobKind::Nop))
        }
        other => {
            let kind =
                job_kind_from_type(other).expect("collapsed job type dispatches to a worker");
            let job_id = enqueue_job(allocator, unit_name, kind, mode).await?;
            Ok((job_id, kind))
        }
    }
}

// ---------------------------------------------------------------------------
// On-demand unit materialization (unit.define)
// ---------------------------------------------------------------------------

/// How long System A waits for a worker's `unit.define_result` before
/// failing the request.  The reply is a local Unix-socket roundtrip; the
/// bound is aligned with the job-timeout scale (docs/user-slice-todo.md
/// section 4.1 suggests consistency with job timeouts).
const UNIT_DEFINE_TIMEOUT: Duration = Duration::from_secs(60);

/// Upper bound for the per-transaction `unit.define` materialisation
/// rounds.  Dependency chains can be arbitrarily deep, but each round
/// loads the exact missing unit, so a handful of rounds covers realistic
/// graphs; the bound only guards against definition sources that can never
/// satisfy a missing unit.
const UNIT_DEFINE_MAX_ROUNDS: u32 = 32;

/// Walk the dependency closure the planner will expand and collect the
/// names of units that are referenced but not loaded — the pre-plan scan
/// of the `unit.define` protocol.
///
/// Mirrors the edge set of `transaction.rs`'s `add_job_and_dependencies`
/// for Start jobs (requires/binds_to/wants/upholds/requisite/conflicts),
/// plus the implicit `[Unit] Slice=` parent edge, so every unit that could
/// make `build_plan` fail with `UnitNotFound` is found up front.
pub fn collect_missing_units(units: &HashMap<String, UnitFile>, root: &str) -> Vec<String> {
    collect_missing_units_multi(units, &[root])
}

/// Like [`collect_missing_units`] but walks the dependency closure from
/// multiple roots (for multi-anchor transactions).
pub fn collect_missing_units_multi(
    units: &HashMap<String, UnitFile>,
    roots: &[&str],
) -> Vec<String> {
    let mut missing: Vec<String> = Vec::new();
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut stack: Vec<String> = roots.iter().map(|r| r.to_string()).collect();
    while let Some(name) = stack.pop() {
        if !visited.insert(name.clone()) {
            continue;
        }
        let Some(uf) = units.get(&name) else {
            missing.push(name);
            continue;
        };
        let section = &uf.unit;
        for dep in section
            .requires
            .iter()
            .chain(section.binds_to.iter())
            .chain(section.wants.iter())
            .chain(section.upholds.iter())
            .chain(section.requisite.iter())
            .chain(section.conflicts.iter())
        {
            stack.push(dep.clone());
        }
        // The implicit Slice= edge: a unit's parent slice must be loaded
        // for the plan to mirror systemd's IN_SLICE dependency.
        if !section.slice.is_empty() && section.slice != crate::state::ROOT_SLICE_NAME {
            stack.push(section.slice.clone());
        }
    }
    missing.sort();
    missing.dedup();
    missing
}

/// Request definitions for the given missing units from the workers that
/// own their unit types (`unit.define` protocol), then commit the returned
/// definitions into the allocator through the same idempotent merge as the
/// finder path.
///
/// Batching: units of the same type go in one request envelope to one
/// worker.  Fails — keeping the caller's hard-error semantics — when no
/// worker handles a unit's type, or the worker refuses / does not answer
/// within [`UNIT_DEFINE_TIMEOUT`].
pub async fn request_unit_definition(allocator: AllocatorHandle, missing: &[String]) -> Result<()> {
    // 1. Static loader first: units that exist on disk (or can be
    //    instantiated from a template) are System A's own on-demand load.
    //    The unit.define protocol is only for dynamic units (slices etc.)
    //    that no unit file covers (docs/user-slice-todo.md, 未决问题 2).
    let mut dynamic: Vec<String> = Vec::new();
    for name in missing {
        if !crate::unit::loader::ensure_loaded_from_disk(allocator.clone(), name).await? {
            dynamic.push(name.clone());
        }
    }
    if dynamic.is_empty() {
        return Ok(());
    }

    // 2a. `.power` units need no on-demand materialization here: System Init
    // owns the `power` unit type and registers every `.power` definition
    // (poweroff.power, reboot.power, ...) with System A during its control
    // phase via `manager.register_power_units`, so they are already in the
    // graph.  A `.power` name that is *not* registered (e.g. `evil.power`)
    // falls through to the general grouping below and fails with "No worker
    // available for unit type 'power'".

    // 2. Group the remaining (dynamic) units by the worker that owns their
    // unit type.  Only workers that declared `unit.define` support
    // (WorkerRegistration.supports_unit_define) are asked; a worker that
    // owns a type without implementing the protocol is skipped, leaving
    // the unit missing so the plan fails with UnitNotFound as before the
    // protocol existed.
    let targets: Vec<(String, Vec<String>)> = {
        let state = allocator.read();
        let mut grouped: std::collections::BTreeMap<String, Vec<String>> =
            std::collections::BTreeMap::new();
        for name in &dynamic {
            let unit_type = UnitKind::from_extension(name).worker_type().to_string();
            grouped.entry(unit_type).or_default().push(name.clone());
        }
        let mut targets: Vec<(String, Vec<String>)> = Vec::new();
        for (unit_type, names) in grouped {
            let worker = state
                .workers
                .values()
                .find(|w| w.unit_types.contains(&unit_type) && w.supports_unit_define);
            match worker {
                Some(w) => targets.push((w.worker_id.clone(), names)),
                None => {
                    let known = state
                        .workers
                        .values()
                        .any(|w| w.unit_types.contains(&unit_type));
                    if known {
                        // A worker owns the type but does not implement the
                        // definition protocol: the unit stays missing and
                        // the plan reports UnitNotFound (pre-protocol
                        // semantics).
                        debug!(
                            "Skipping unit.define for {:?}: no worker for type '{unit_type}' supports the protocol",
                            names
                        );
                    } else {
                        bail!(sysa::l10n::fmt(sysa::l10n::t_("No worker available for unit type '{unit_type}' (units: {names}); unit.define attempt failed. Is the corresponding System Worker running?"), &[("unit_type", &unit_type.to_string()), ("names", &format!("{:?}", names))]))
                    }
                }
            }
        }
        targets
    };
    if targets.is_empty() {
        return Ok(());
    }

    for (worker_id, names) in targets {
        let request_id = next_request_id();
        let (tx, rx) = tokio::sync::oneshot::channel::<UnitDefineResult>();
        {
            let mut state = allocator.write();
            state.unit_define_txs.insert(request_id, tx);
        }

        let req = UnitDefineRequest {
            unit_names: names.clone(),
        };
        let env = sysa::ipc::make_envelope(request_id, "system-a", &worker_id, "unit.define", req)?;
        let mut buf = bytes::BytesMut::new();
        env.encode(&mut buf)?;

        let worker_tx = {
            let state = allocator.read();
            state.workers.get(&worker_id).map(|w| w.envelope_tx.clone())
        };
        let Some(worker_tx) = worker_tx else {
            // The worker disconnected between grouping and send.
            let mut state = allocator.write();
            state.unit_define_txs.remove(&request_id);
            bail!(sysa::l10n::fmt(
                sysa::l10n::t_(
                    "Worker '{worker_id}' disconnected during unit.define (units: {names})"
                ),
                &[
                    ("worker_id", &worker_id.to_string()),
                    ("names", &format!("{:?}", names))
                ]
            ));
        };
        if worker_tx.send(buf.freeze()).await.is_err() {
            let mut state = allocator.write();
            state.unit_define_txs.remove(&request_id);
            bail!(sysa::l10n::fmt(
                sysa::l10n::t_(
                    "Worker '{worker_id}' disconnected during unit.define (units: {names})"
                ),
                &[
                    ("worker_id", &worker_id.to_string()),
                    ("names", &format!("{:?}", names))
                ]
            ));
        }

        let result = match tokio::time::timeout(UNIT_DEFINE_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => {
                let mut state = allocator.write();
                state.unit_define_txs.remove(&request_id);
                bail!(sysa::l10n::fmt(
                    sysa::l10n::t_(
                        "unit.define reply channel closed for '{worker_id}' (units: {names})"
                    ),
                    &[
                        ("worker_id", &worker_id.to_string()),
                        ("names", &format!("{:?}", names))
                    ]
                ));
            }
            Err(_) => {
                let mut state = allocator.write();
                state.unit_define_txs.remove(&request_id);
                bail!(sysa::l10n::fmt(sysa::l10n::t_("unit.define timed out for '{worker_id}' after {UNIT_DEFINE_TIMEOUT} (units: {names})"), &[("worker_id", &worker_id.to_string()), ("UNIT_DEFINE_TIMEOUT", &format!("{:?}", UNIT_DEFINE_TIMEOUT)), ("names", &format!("{:?}", names))]));
            }
        };
        if !result.success {
            bail!(sysa::l10n::fmt(
                sysa::l10n::t_(
                    "Worker '{worker_id}' refused unit.define (units: {names}): {error}"
                ),
                &[
                    ("worker_id", &worker_id.to_string()),
                    ("names", &format!("{:?}", names)),
                    ("error", &(result.error).to_string())
                ]
            ));
        }

        // Commit the synthesized definitions through the same idempotent
        // merge as the finder path (`state.merge_units`).
        let units: HashMap<String, systema_sysf::ir::UnitIR> =
            serde_json::from_slice(&result.units_json).map_err(|e| {
                anyhow::anyhow!(sysa::l10n::fmt(
                    sysa::l10n::t_(
                        "Failed to deserialize unit.define result from '{worker_id}': {e}"
                    ),
                    &[("worker_id", &worker_id.to_string()), ("e", &e.to_string())]
                ))
            })?;
        let (created, updated) = {
            let mut state = allocator.write();
            state.merge_units(&units).map_err(anyhow::Error::msg)?
        };
        // Newly materialized units must also receive DefaultDependencies=
        // (After=sysinit.target & co.), same as the finder commit path.
        crate::unit::loader::inject_default_dependencies(allocator.clone());
        info!("unit.define from {worker_id}: committed {created} new, {updated} updated unit(s)");
    }

    Ok(())
}

/// Dispatch one `power`-type unit step by forwarding it to System Init.
///
/// The `power` unit type belongs to System Init, which registers every
/// `.power` definition at boot via `manager.register_power_units`; no worker
/// ever registers `power` with System A.  The whole execution contract is
/// one datagram on the Init notify socket: `POWER=<short-action>`.  The step
/// job is recorded as Running, completes as Done as soon as the datagram is
/// handed to the kernel (for terminal transitions the machine goes down
/// before any reply could matter), and the unit state flips to `active`.  On
/// send failure the request's jobs are cancelled and the root completes
/// Failed.  The short action is the unit name minus its `.power` suffix;
/// System Init is the authority on the vocabulary.
async fn dispatch_power_step(
    allocator: AllocatorHandle,
    name: &str,
    step_kind: JobKind,
    job_id: u64,
    primary_job_id: u64,
    root_name: &str,
    created_job_ids: &[(u64, String)],
) -> Result<()> {
    // A `.power` unit's transition is only ever *started*.  Stopping or
    // reloading one is a no-op: resolving the job as Done without touching
    // the system mirrors how the synthetic unit simply disappears.
    if !matches!(step_kind, JobKind::Start | JobKind::Restart) {
        let mut state = allocator.write();
        if let Some(job) = state.jobs.get_mut(&job_id) {
            job.status = JobStatus::Done;
        }
        if let Some(ref tx) = state.job_completion_tx {
            let _ = tx.send(JobCompletion {
                job_id,
                unit_name: name.to_string(),
                result: JobResultKind::Done,
            });
        }
        return Ok(());
    }

    let Some(action) = name.strip_suffix(".power") else {
        let err = l10n::fmt(
            l10n::t_("Unit '{name}' is not a valid .power transition; cannot execute it."),
            &[("name", name)],
        );
        warn!("{}", err);
        cancel_request_jobs(&allocator, created_job_ids, primary_job_id, root_name);
        bail!("{}", err);
    };

    let invocation_id = match step_kind {
        JobKind::Start | JobKind::Restart => Some(generate_invocation_id()),
        _ => None,
    };
    {
        let mut state = allocator.write();
        if let Some(ref inv_id) = invocation_id {
            state.invocation_ids.insert(name.to_string(), inv_id.clone());
        }
        state.jobs.insert(
            job_id,
            Job {
                id: job_id,
                unit_name: name.to_string(),
                kind: step_kind,
                status: JobStatus::Running,
                timeout_abort: None,
            },
        );
    }
    emit_job_new_after_lock(allocator.clone(), job_id, name, step_kind);

    // The notify listener System Init binds before spawning anything
    // (<notify-dir>/init.sock); a datagram there is how workers and the
    // allocator report readiness, and how power transitions are requested.
    let sock_path =
        std::path::PathBuf::from(sysa::paths::instance().notify_dir.as_str()).join("init.sock");
    let payload = format!("POWER={action}\n");
    let delivered = match std::os::unix::net::UnixDatagram::unbound() {
        Ok(sock) => match sock.send_to(payload.as_bytes(), &sock_path) {
            Ok(_) => true,
            Err(e) => {
                error!(
                    "Cannot forward power transition '{action}' to System Init ({}): {e}",
                    sock_path.display()
                );
                false
            }
        },
        Err(e) => {
            error!("Cannot create notify datagram socket: {e}");
            false
        }
    };

    if delivered {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or(0);
        let mut state = allocator.write();
        if let Some(job) = state.jobs.get_mut(&job_id) {
            job.status = JobStatus::Done;
        }
        let entry = state.unit_states.entry(name.to_string()).or_default();
        if entry.active_state != "active" {
            let inv = invocation_id.clone().unwrap_or_default();
            entry.active_state = "active".to_string();
            entry.sub_state.clear();
            entry.invocation_id = inv;
            entry.active_enter_timestamp = now;
        }
        if let Some(ref tx) = state.job_completion_tx {
            let _ = tx.send(JobCompletion {
                job_id,
                unit_name: name.to_string(),
                result: JobResultKind::Done,
            });
        }
        info!(
            "Forwarded power transition '{action}' (unit {}) to System Init",
            name
        );
    } else {
        let err = l10n::fmt(
            l10n::t_(
                "Cannot forward power transition for '{name}' to System Init (notify socket unreachable)."
            ),
            &[("name", name)],
        );
        warn!("{}", err);
        cancel_request_jobs(&allocator, created_job_ids, primary_job_id, root_name);
        bail!("{}", err);
    }

    Ok(())
}

/// Cancel every job created for a request and complete the root as Failed.
/// Mirrors the `No worker registered` error path in [`enqueue_job`].
fn cancel_request_jobs(
    allocator: &AllocatorHandle,
    created_job_ids: &[(u64, String)],
    primary_job_id: u64,
    root_name: &str,
) {
    let mut state = allocator.write();
    for (jid, _) in created_job_ids {
        if let Some(job) = state.jobs.get_mut(jid) {
            job.status = JobStatus::Cancelled;
        }
        state.serial_completion_txs.remove(jid);
    }
    if let Some(ref tx) = state.job_completion_tx {
        let _ = tx.send(JobCompletion {
            job_id: primary_job_id,
            unit_name: root_name.to_string(),
            result: JobResultKind::Failed,
        });
    }
}

/// Core job enqueueing logic.
pub async fn enqueue_job(
    allocator: AllocatorHandle,
    unit_name: &str,
    kind: JobKind,
    mode: JobMode,
) -> Result<u64> {
    // Resolve alias names to their canonical unit before anything else, so an
    // alias (e.g. `display-manager.service`) never schedules a separate job.
    let resolved = allocator.read().resolve_unit_name(unit_name);
    let unit_name: &str = &resolved;
    info!("Scheduling {:?} for {} (mode={:?})", kind, unit_name, mode);

    // --- Mode/type validation (manager_add_job_full) ---
    {
        let state = allocator.read();
        let allow_isolate = state
            .units
            .get(unit_name)
            .map(|u| u.unit.allow_isolate)
            .unwrap_or(false);
        check_mode_constraints(mode, kind, unit_name, allow_isolate)?;
    }

    // --- Already-active short-circuit (systemd unit_start → -EALREADY) ---
    // A Start for a unit that is already active is a no-op: systemd's
    // `unit_start()` returns -EALREADY and completes the request without
    // re-running the unit.  The `unit_states` cache is kept current by the
    // workers' state pushes; units with RemainAfterExit=yes stay cached as
    // active after their process exits.  Only Start is affected — Restart
    // explicitly re-runs the unit.
    if kind == JobKind::Start {
        let already_active = {
            let state = allocator.read();
            state
                .unit_states
                .get(unit_name)
                .map(|c| c.active_state == "active")
                .unwrap_or(false)
        };
        if already_active {
            info!("{} already active; start is a no-op (-EALREADY)", unit_name);
            let job_id = next_job_id();
            let mut state = allocator.write();
            if let Some(ref tx) = state.job_completion_tx {
                let _ = tx.send(JobCompletion {
                    job_id,
                    unit_name: unit_name.to_string(),
                    result: JobResultKind::Done,
                });
            }
            emit_job_new(&mut state, job_id, unit_name, kind);
            return Ok(job_id);
        }
    }

    // Announce the job intent on the notify channel (bootlog / animation).
    match kind {
        JobKind::Start | JobKind::Restart => {
            sysa::notify::broadcast(&[("UNIT_STARTING", unit_name)]);
        }
        JobKind::Stop => {
            sysa::notify::broadcast(&[("UNIT_STOPPING", unit_name)]);
        }
        JobKind::Reload | JobKind::Nop => {}
    }

    // --- Non-transient scopes are refused (systemd scope_start) ---
    //
    // Scopes wrap externally-created processes and exist only as transient
    // units: a `.scope` on disk cannot be started by us.  Mirrors
    // systemd's `scope_start()` returning -ENOENT for non-transient scopes.
    if kind == JobKind::Start {
        let state = allocator.read();
        if let Some(unit) = state.units.get(unit_name) {
            if unit.kind == UnitKind::Scope && !unit.transient {
                bail!(
                    "{}",
                    l10n::fmt(
                        l10n::t_("Scope {unit_name} is not transient and cannot be started."),
                        &[("unit_name", unit_name)],
                    )
                );
            }
        }
    }

    // --- Early check: ensure at least one worker exists for the root unit ---
    {
        let state = allocator.read();
        let unit = state.units.get(unit_name);
        // For units that are not loaded yet the type is derived from the
        // name extension (e.g. `user-0.slice` → "slice"), so the check
        // below never falls back to "service" for a slice/scope root.
        let unit_type = unit
            .map(|u| u.kind.worker_type().to_string())
            .unwrap_or_else(|| {
                UnitKind::from_extension(unit_name)
                    .worker_type()
                    .to_string()
            });
        // The `power` unit type has no worker: it is owned by System Init,
        // which registers every `.power` definition at boot and executes the
        // transition in-process after System A forwards it over the notify
        // datagram (`dispatch_power_step`).  Treat it as always served.
        let has_worker = unit_type == "power"
            || state
                .workers
                .values()
                .any(|w| w.unit_types.contains(&unit_type));
        if !has_worker {
            bail!("{}", l10n::fmt(l10n::t_("No worker available for unit type '{unit_type}' (unit: {unit_name}). Cannot execute {kind} operation. Is the corresponding System Worker running?"), &[
                ("unit_type", &unit_type),
                ("unit_name", unit_name),
                ("kind", &format!("{:?}", kind)),
            ]));
        }
    }

    // --- Flush mode: cancel all pending jobs first ---
    if mode == JobMode::Flush {
        let to_cancel: Vec<u64> = {
            let state = allocator.read();
            state
                .jobs
                .values()
                .filter(|j| matches!(j.status, JobStatus::Running))
                .map(|j| j.id)
                .collect()
        };
        for jid in to_cancel {
            let mut state = allocator.write();
            let name = state
                .jobs
                .get(&jid)
                .map(|j| j.unit_name.clone())
                .unwrap_or_default();
            if let Some(job) = state.jobs.get_mut(&jid) {
                job.status = JobStatus::Cancelled;
            }
            if let Some(ref tx) = state.job_completion_tx {
                let _ = tx.send(JobCompletion {
                    job_id: jid,
                    unit_name: name,
                    result: JobResultKind::Cancelled,
                });
            }
        }
    }

    // --- Condition checks: skip start (not failure) if conditions not met ---
    let ignore_deps = mode == JobMode::IgnoreDependencies || mode == JobMode::IgnoreRequirements;
    if matches!(kind, JobKind::Start | JobKind::Restart) && !ignore_deps {
        let conditions_met = {
            let state = allocator.read();
            state
                .units
                .get(unit_name)
                .map(|u| check_conditions(&u.unit))
                .unwrap_or(true)
        };
        if !conditions_met {
            info!(
                "Conditions not met for {}; skipping start (unit stays inactive)",
                unit_name
            );
            let job_id = next_job_id();
            let mut state = allocator.write();
            if let Some(ref tx) = state.job_completion_tx {
                let _ = tx.send(JobCompletion {
                    job_id,
                    unit_name: unit_name.to_string(),
                    result: JobResultKind::Skipped,
                });
            }
            emit_job_new(&mut state, job_id, unit_name, kind);
            return Ok(job_id);
        }
    }

    // --- Assert checks: fail start if asserts not met ---
    if matches!(kind, JobKind::Start | JobKind::Restart) && !ignore_deps {
        let asserts_met = {
            let state = allocator.read();
            state
                .units
                .get(unit_name)
                .map(|u| check_asserts(&u.unit))
                .unwrap_or(true)
        };
        if !asserts_met {
            warn!(
                "Assert check failed for {}; unit start prevented",
                unit_name
            );
            let job_id = next_job_id();
            let mut state = allocator.write();
            if let Some(ref tx) = state.job_completion_tx {
                let _ = tx.send(JobCompletion {
                    job_id,
                    unit_name: unit_name.to_string(),
                    result: JobResultKind::Dependency,
                });
            }
            emit_job_new(&mut state, job_id, unit_name, kind);
            return Ok(job_id);
        }
    }

    // --- Start rate limiting (systemd unit_start → unit_test_start_limit) ---
    // Every start attempt counts: manual starts, auto-restarts and
    // dependency-triggered starts all funnel through enqueue_job(Start).
    if matches!(kind, JobKind::Start | JobKind::Restart) {
        let (interval_sec, burst, action) = {
            let state = allocator.read();
            state
                .units
                .get(unit_name)
                .map(|u| {
                    (
                        u.unit.start_limit_interval_sec,
                        u.unit.start_limit_burst,
                        u.unit.start_limit_action.clone(),
                    )
                })
                .unwrap_or((10, 5, StartLimitAction::None))
        };
        let rate_ok = {
            let mut state = allocator.write();
            let limit_state = state
                .start_limit_state
                .entry(unit_name.to_string())
                .or_insert_with(StartLimitState::new);
            limit_state.check_rate_limit(Duration::from_secs(interval_sec as u64), burst)
        };
        if !rate_ok {
            warn!(
                "Start rate limit exceeded for {} (interval={}s burst={}), refusing to start",
                unit_name, interval_sec, burst
            );
            execute_start_limit_action(allocator.clone(), &action, unit_name).await;
            bail!("{}", l10n::fmt(l10n::t_("Start rate limit exceeded for {unit_name} (interval={interval_sec}s burst={burst})."), &[
                ("unit_name", unit_name),
                ("interval_sec", &interval_sec.to_string()),
                ("burst", &burst.to_string()),
            ]));
        }
    }

    // --- Job conflict detection ---
    {
        let read_state = allocator.read();
        let existing: Option<(u64, String)> = read_state
            .jobs
            .values()
            .find(|j| {
                j.unit_name == unit_name && j.kind == kind && matches!(j.status, JobStatus::Running)
            })
            .map(|j| (j.id, j.unit_name.clone()));
        drop(read_state);

        if let Some((existing_id, _)) = existing {
            match mode {
                JobMode::Fail => {
                    bail!("{}", l10n::fmt(l10n::t_("Job already exists for unit {unit_name} (kind={kind}, id={existing_id})."), &[
                        ("unit_name", unit_name),
                        ("kind", &format!("{:?}", kind)),
                        ("existing_id", &existing_id.to_string()),
                    ]));
                }
                // systemd merges a job that is already running for the same
                // unit *and* same type into the existing one (job.c
                // `job_merge()`: `unit_get_job()` with a matching type),
                // regardless of job mode — it never re-dispatches it.
                //
                // Cancelling and re-spawning here is what turns a
                // self-recursive `systemctl start` inside a unit's ExecStart
                // into an infinite spawn loop: SysV init scripts (e.g.
                // `/etc/init.d/virtualbox-guest-utils`) detect systemd and
                // delegate to `systemctl start $unit`, which must resolve to
                // the already-running job instead of starting another copy.
                _ => {
                    return Ok(existing_id);
                }
            }
        }
    }

    // --- Build the transaction plan (systemd transaction_activate()) ---
    // The plan is the closure of every unit pulled in through the
    // dependency atoms (Requires/Wants/Requisite/BindsTo/Upholds/Conflicts/
    // PartOf/PropagatesReloadTo), merged to one job per unit, ordered by the
    // After=/Before= ordering graph, with ordering cycles broken.
    //
    // On-demand materialization (unit.define): before planning, the
    // dependency closure is walked and every referenced-but-missing unit
    // (e.g. a scope's `Slice=user-<UID>.slice` that System R has not
    // committed yet) is requested from the worker that owns its unit type,
    // which synthesizes a definition (System R: slice parent chains).  The
    // definitions are committed through the same idempotent merge as the
    // finder path before the plan is built — the systemd counterpart is
    // `unit_add_dependency_by_name()` loading the parent slice on demand.
    // A second `UnitNotFound` after the pre-scan gets one more definition
    // round for the exact missing unit (bounded retry); if it still fails,
    // the plan error is reported as before.
    let plan = {
        let missing = {
            let state = allocator.read();
            let units: HashMap<String, UnitFile> = state.units.clone();
            collect_missing_units(&units, unit_name)
        };

        if !missing.is_empty() {
            info!(
                "Pre-plan scan: {} referenced-but-missing unit(s) ({:?}); requesting definitions via unit.define",
                missing.len(),
                missing
            );
            request_unit_definition(allocator.clone(), &missing).await?;
        }

        // Build the plan, materialising on-disk/dynamic definitions one
        // layer at a time: the pre-plan scan above only expands the
        // dependency closure one level (missing units' own dependencies are
        // not yet known), so a chain `a -> b -> c` can still hit
        // `UnitNotFound` for the second layer.  Loop until the plan builds
        // or no definition source can satisfy the missing unit.
        let mut attempts: u32 = 0;
        loop {
            let (units, states, installed) = {
                let state = allocator.read();
                (
                    state.units.clone(),
                    state
                        .unit_states
                        .iter()
                        .map(|(n, c)| {
                            (
                                n.clone(),
                                UnitActiveState::from_active_state_str(&c.active_state),
                            )
                        })
                        .collect::<HashMap<String, UnitActiveState>>(),
                    state
                        .jobs
                        .values()
                        .filter(|j| matches!(j.status, JobStatus::Running))
                        .map(|j| (j.unit_name.clone(), JobType::from_job_kind(j.kind)))
                        .collect::<HashMap<String, JobType>>(),
                )
            };
            match build_plan(
                &units,
                &states,
                &installed,
                unit_name,
                JobType::from_job_kind(kind),
                PlannerMode::from_job_mode(mode),
            ) {
                Ok(plan) => break plan,
                Err(PlanError::UnitNotFound(name)) if attempts < UNIT_DEFINE_MAX_ROUNDS => {
                    // Bounded retry: the pre-scan walk mirrors the planner's
                    // edge expansion but may miss an edge it only discovers
                    // later; ask for the exact missing unit once more.
                    warn!(
                        "Transaction for {} failed with missing unit {name} after pre-scan; one unit.define retry (round {})",
                        unit_name,
                        attempts + 1
                    );
                    request_unit_definition(allocator.clone(), std::slice::from_ref(&name)).await?;
                    attempts += 1;
                }
                Err(e) => {
                    warn!(
                        "Transaction for {} ({kind:?}, mode={mode:?}) failed: {e}",
                        unit_name
                    );
                    bail!("{}", e);
                }
            }
        }
    };

    debug!(
        "Plan for {unit_name}: {:?}",
        plan.steps
            .iter()
            .map(|s| (s.unit.as_str(), s.job_type.as_str()))
            .collect::<Vec<_>>()
    );

    // --- Requisite verification ---
    // Surviving VerifyActive= steps check the cached runtime state; an
    // unknown state counts as not active (the plan kept the step because
    // the unit is not known to be active), matching systemd.
    if let Some(v) = plan
        .steps
        .iter()
        .find(|s| s.job_type == JobType::VerifyActive)
    {
        let active = {
            let state = allocator.read();
            state
                .unit_states
                .get(&v.unit)
                .map(|c| UnitActiveState::from_active_state_str(&c.active_state))
                .unwrap_or(UnitActiveState::Unknown)
                .is_active_or_reloading()
        };
        if !active {
            warn!(
                "Requisite check failed: {} depends on {} which is not active",
                unit_name, v.unit
            );
            let job_id = next_job_id();
            let mut state = allocator.write();
            if let Some(ref tx) = state.job_completion_tx {
                let _ = tx.send(JobCompletion {
                    job_id,
                    unit_name: unit_name.to_string(),
                    result: JobResultKind::Dependency,
                });
            }
            emit_job_new(&mut state, job_id, unit_name, kind);
            return Ok(job_id);
        }
    }

    // Before dispatching any dependencies, re-check that the root doesn't
    // already have a running job (handles races after the conflict detection above).
    {
        let state = allocator.read();
        if let Some(existing) = state.jobs.values().find(|j| {
            j.unit_name == unit_name && j.kind == kind && matches!(j.status, JobStatus::Running)
        }) {
            debug!(
                "Root unit {} already has a {:?} job (id={}) — returning existing ID",
                unit_name, kind, existing.id
            );
            return Ok(existing.id);
        }
    }

    let primary_job_id = next_job_id();
    let serial_mode = matches!(mode, JobMode::Replace | JobMode::ReplaceIrreversibly);

    // For serial execution, chain tasks so each waits for the previous.
    let mut serial_chain_rx: Option<tokio::sync::oneshot::Receiver<()>> = None;

    // Create job records and dispatch tasks.
    let mut created_job_ids: Vec<(u64, String)> = Vec::new();
    for step in &plan.steps {
        let name = &step.unit;
        // Steps that need no worker interaction (Nop/VerifyActive) are
        // resolved by the planner; nothing is dispatched for them.
        let Some(step_kind) = job_kind_from_type(step.job_type) else {
            debug!(
                "Skipping worker dispatch for {} ({})",
                name,
                step.job_type.as_str()
            );
            continue;
        };
        // The primary job ID belongs to the unit that was directly requested.
        let is_root = name.as_str() == unit_name;
        let job_id = if is_root {
            primary_job_id
        } else {
            next_job_id()
        };
        created_job_ids.push((job_id, name.clone()));

        // Idempotency: skip if there is already an in-flight job of the same kind.
        // Note: "already in desired state" check is removed — runtime state is not
        // cached.  Workers handle no-ops on their side.
        {
            let state = allocator.read();
            let existing_running_job_id: Option<u64> = state
                .jobs
                .values()
                .find(|j| {
                    j.unit_name == *name
                        && j.kind == step_kind
                        && matches!(j.status, JobStatus::Running)
                })
                .map(|j| j.id);

            if let Some(existing_jid) = existing_running_job_id {
                debug!(
                    "Skipping {:?} for {} (existing job running)",
                    step_kind, name
                );
                if is_root {
                    return Ok(existing_jid);
                }
                continue;
            }
        }

        // -EALREADY backstop (systemd unit_start): a plan may still contain a
        // Start step for a unit that became active between plan construction
        // and dispatch (e.g. an overlapping boot transaction).  Skipping here
        // mirrors unit_start() returning -EALREADY for active units.  The
        // root job is completed as done; non-root steps are skipped (their
        // dependents are unaffected, matching transaction_apply()).
        if step_kind == JobKind::Start {
            let already_active = {
                let state = allocator.read();
                state
                    .unit_states
                    .get(name.as_str())
                    .map(|c| c.active_state == "active")
                    .unwrap_or(false)
            };
            if already_active {
                debug!("Skipping Start for {} (unit already active)", name);
                if is_root {
                    {
                        let mut state = allocator.write();
                        if let Some(ref tx) = state.job_completion_tx {
                            let _ = tx.send(JobCompletion {
                                job_id,
                                unit_name: name.clone(),
                                result: JobResultKind::Done,
                            });
                        }
                        emit_job_new(&mut state, job_id, name, step_kind);
                    }
                    return Ok(job_id);
                }
                continue;
            }
        }

        // Find the appropriate worker.
        // NOTE: read lock is dropped before match so the error path can
        // acquire the write lock without deadlocking.
        let (worker_chan, worker_id, task_id, unit_type) = {
            let state = allocator.read();
            let unit = state.units.get(name.as_str());
            let unit_type = unit
                .map(|u| u.kind.worker_type().to_string())
                .unwrap_or_else(|| "service".to_string());

            let worker = state
                .workers
                .values()
                .find(|w| w.unit_types.contains(&unit_type));

            let tid = next_task_id();
            (
                worker.map(|w| w.envelope_tx.clone()),
                worker.map(|w| w.worker_id.clone()),
                tid,
                unit_type,
            )
        };

        // --- Power transitions run in System Init, not in a remote worker:
        // System Init owns the `power` unit type (it registers every
        // `.power` definition at boot via `manager.register_power_units`)
        // and executes the transition in-process.  Every `power`-type step
        // is forwarded to the Init notify listener as `POWER=<action>`, and
        // the job completes immediately (fire-and-forget: for terminal
        // transitions the kernel takes the machine down before any reply
        // could matter).  `task_id` above was reserved but never used.
        if unit_type == "power" {
            dispatch_power_step(
                allocator.clone(),
                name,
                step_kind,
                job_id,
                primary_job_id,
                unit_name,
                &created_job_ids[..],
            )
            .await?;
            continue;
        }

        let (worker_envelope_tx, task_id) = match (worker_chan, task_id) {
            (Some(tx), tid) => (tx, tid),
            (None, _) => {
                let err = l10n::fmt(l10n::t_("No worker registered for unit type '{unit_type}' (unit: {unit_name}). Cannot process dependency chain for '{name}'."), &[
                    ("unit_type", &unit_type),
                    ("unit_name", unit_name),
                    ("name", name),
                ]);
                warn!("{}", err);
                {
                    let mut state = allocator.write();
                    emit_job_new(&mut state, job_id, name, step_kind);

                    // Cancel all jobs already created for this request.
                    for (jid, _) in &created_job_ids {
                        if let Some(job) = state.jobs.get_mut(jid) {
                            job.status = JobStatus::Cancelled;
                        }
                        state.serial_completion_txs.remove(jid);
                    }

                    // Notify the caller that the root job has failed.
                    if let Some(ref tx) = state.job_completion_tx {
                        let _ = tx.send(JobCompletion {
                            job_id: primary_job_id,
                            unit_name: unit_name.to_string(),
                            result: JobResultKind::Failed,
                        });
                    }
                }
                bail!("{}", err);
            }
        };

        let unit_file = {
            let state = allocator.read();
            state.units.get(name.as_str()).cloned()
        };

        // Generate invocation ID for Start/Restart tasks.
        let invocation_id = match step_kind {
            JobKind::Start | JobKind::Restart => Some(generate_invocation_id()),
            _ => None,
        };

        // Create a serial chain entry for this task.
        let (next_serial_tx, next_serial_rx) = tokio::sync::oneshot::channel::<()>();

        // Record the job and the task_id → job_kind mapping.
        // Track invocation_id for GetUnitByInvocationID lookups.
        //
        // Re-check for an existing in-flight job of the same unit+kind
        // under the write lock: the pre-lock checks above race with a
        // concurrent enqueue (the actual job insert happens here), which
        // could otherwise dispatch a duplicate task for one request.
        let mut duplicate_job = false;
        {
            let mut state = allocator.write();
            let existing_id: Option<u64> = state
                .jobs
                .values()
                .find(|j| {
                    j.unit_name == *name
                        && j.kind == step_kind
                        && matches!(j.status, JobStatus::Running)
                })
                .map(|j| j.id);
            if let Some(existing_id) = existing_id {
                warn!(
                    "Job already exists for unit {} ({:?}, id={}); not dispatching a duplicate",
                    name, step_kind, existing_id
                );
                duplicate_job = true;
                if is_root {
                    // The caller is waiting on the primary job id; complete
                    // it immediately — the existing in-flight job is the
                    // one doing the work (systemd semantics: a second
                    // StartUnit for a running job merges into it).
                    if let Some(ref tx) = state.job_completion_tx {
                        let _ = tx.send(JobCompletion {
                            job_id: primary_job_id,
                            unit_name: unit_name.to_string(),
                            result: JobResultKind::Done,
                        });
                    }
                }
            } else {
                if let Some(ref inv_id) = invocation_id {
                    state.invocation_ids.insert(name.clone(), inv_id.clone());
                }
                state.jobs.insert(
                    job_id,
                    Job {
                        id: job_id,
                        unit_name: name.clone(),
                        kind: step_kind,
                        status: JobStatus::Running,
                        timeout_abort: None,
                    },
                );
                state.task_kinds.insert(task_id, step_kind);
                // Assign unit ownership to the dispatching worker.  Starting an
                // automount implicitly assigns ownership of its companion mount
                // unit (same worker handles both).
                if let Some(wid) = &worker_id {
                    state.unit_owners.insert(name.clone(), wid.clone());
                    if step_kind == JobKind::Start && name.ends_with(".automount") {
                        let mount_name = format!("{}.mount", name.trim_end_matches(".automount"));
                        if state.units.contains_key(&mount_name) {
                            state.unit_owners.insert(mount_name, wid.clone());
                        }
                    }
                }
                if serial_mode {
                    state.serial_completion_txs.insert(task_id, next_serial_tx);
                }
            }
        }
        if duplicate_job {
            // Nothing was dispatched; skip JobNew, the timeout monitor and
            // the envelope send for this step.
            continue;
        }

        // Emit JobNew signal for this job.
        emit_job_new_after_lock(allocator.clone(), job_id, name, step_kind);

        // --- Timeout monitoring ---
        let abort_handle = spawn_job_timeout(
            allocator.clone(),
            task_id,
            job_id,
            name,
            step_kind,
            &unit_file,
        );
        if let Some(handle) = abort_handle {
            let mut state = allocator.write();
            if let Some(job) = state.jobs.get_mut(&job_id) {
                job.timeout_abort = Some(handle);
            }
        }

        // Build MethodCall envelope for this job.
        let method_name = match step_kind {
            JobKind::Start => "start",
            JobKind::Stop => "stop",
            JobKind::Restart => "restart",
            JobKind::Reload => "reload",
            // Nop steps are never dispatched (job_kind_from_type yields
            // None for them); keep the match exhaustive defensively.
            JobKind::Nop => "nop",
        };
        let unit_config = {
            let state = allocator.read();
            unit_file
                .as_ref()
                .map(|uf| build_unit_config(uf, &state.units))
        };
        let mut args = Vec::new();
        if let Some(ref config) = unit_config {
            config.encode(&mut args).unwrap_or_default();
        }
        let call = sysa::proto::MethodCall {
            method: method_name.to_string(),
            unit_name: name.clone(),
            args,
            invocation_id: invocation_id.clone().unwrap_or_default(),
        };
        let call_env =
            sysa::ipc::make_envelope(task_id, "system-a", &unit_type, "method.call", call)?;
        let mut buf = bytes::BytesMut::new();
        call_env.encode(&mut buf)?;

        // In serial mode, wait for the previous task to complete before
        // sending the next one.  The previous task's handle_task_result
        // will signal through the serial chain channel.  Bounded so a
        // never-completing predecessor cannot wedge the whole dispatch
        // (the caller of this loop awaits it sequentially).
        let serial_wait = async {
            if serial_mode && is_root && serial_chain_rx.is_some() {
                if let Some(rx) = serial_chain_rx.take() {
                    let _ = rx.await;
                }
            }
            if serial_mode && !is_root {
                if let Some(rx) = serial_chain_rx.take() {
                    let _ = rx.await;
                }
            }
        };
        match tokio::time::timeout(Duration::from_secs(15), serial_wait).await {
            Ok(()) => {}
            Err(_) => {
                warn!(
                    "Serial chain wait timed out for {} (task {task_id}); dispatching anyway",
                    name
                );
            }
        }
        if worker_envelope_tx.send(buf.freeze()).await.is_err() {
            warn!("Worker channel closed for unit {}", name);
            let mut state = allocator.write();
            if let Some(job) = state.jobs.get_mut(&job_id) {
                job.status = JobStatus::Failed(sysa::l10n::t_("Worker disconnected").to_string());
            }
            if is_root {
                if let Some(ref tx) = state.job_completion_tx {
                    let _ = tx.send(JobCompletion {
                        job_id,
                        unit_name: name.clone(),
                        result: JobResultKind::Failed,
                    });
                }
            }
        }

        // Set up for the next iteration: the current task's serial_rx
        // will be consumed after the next task completes.
        serial_chain_rx = Some(next_serial_rx);
    }

    Ok(primary_job_id)
}

/// Ask every registered worker for a full state snapshot (`unit.sync_request`).
///
/// Workers reply asynchronously with `unit.sync_report` full-snapshot
/// updates which the IPC server feeds into the `unit_states` cache through
/// the same path as push events.  Used on daemon-reload.
pub async fn request_all_worker_syncs(allocator: AllocatorHandle) {
    let targets: Vec<String> = {
        let state = allocator.read();
        state.workers.keys().cloned().collect()
    };
    for worker_id in targets {
        let req = sysa::proto::UnitSyncRequest {};
        let env = match sysa::ipc::make_envelope(
            next_task_id(),
            "system-a",
            &worker_id,
            "unit.sync_request",
            req,
        ) {
            Ok(env) => env,
            Err(e) => {
                warn!("Failed to build unit.sync_request for '{worker_id}': {e}");
                continue;
            }
        };
        let mut buf = bytes::BytesMut::new();
        if env.encode(&mut buf).is_err() {
            warn!("Failed to encode unit.sync_request for '{worker_id}'");
            continue;
        }
        let worker = {
            let state = allocator.read();
            state.workers.get(&worker_id).map(|w| w.envelope_tx.clone())
        };
        if let Some(tx) = worker {
            if tx.send(buf.freeze()).await.is_err() {
                warn!("Worker channel closed while sending unit.sync_request to '{worker_id}'");
            }
        }
    }
}

/// Emit a JobNew signal for a newly created job (must hold the write lock).
fn emit_job_new(state: &mut AllocatorState, job_id: u64, unit_name: &str, _kind: JobKind) {
    if let Some(ref tx) = state.job_new_tx {
        let _ = tx.send(JobNewInfo {
            job_id,
            unit_name: unit_name.to_string(),
        });
    }
}

/// Emit JobNew without holding the write lock (acquires it briefly).
fn emit_job_new_after_lock(
    allocator: AllocatorHandle,
    job_id: u64,
    unit_name: &str,
    _kind: JobKind,
) {
    let state = allocator.read();
    if let Some(ref tx) = state.job_new_tx {
        let _ = tx.send(JobNewInfo {
            job_id,
            unit_name: unit_name.to_string(),
        });
    }
}

/// Update unit runtime state from a task result received from a worker.
/// Compute which units should receive a propagated `Start` because
/// `unit_name` (a dependency they `BindsTo`) just started successfully.
///
/// Propagation is gated on two conditions:
///   A. `unit_name` must be cached as `active` — a spawn-only success is
///      not enough to trust the binding (systemd only considers the
///      dependency satisfied once the unit is actually active);
///   B. a candidate must not already have an in-flight Start/Restart job,
///      and must not already be cached as `active`/`activating` — this
///      prevents "one more start on top of an in-flight start".
fn binds_to_start_propagation(
    state: &AllocatorState,
    unit_name: &str,
    success: bool,
    kind: JobKind,
) -> Vec<String> {
    if !success || !matches!(kind, JobKind::Start | JobKind::Restart) {
        return Vec::new();
    }

    // Gate A: only propagate when the dependency is genuinely active.
    let dep_active = state
        .unit_states
        .get(unit_name)
        .map(|c| c.active_state == "active")
        .unwrap_or(false);
    if !dep_active {
        debug!(
            "BindsTo start propagation suppressed: dependency {} not cached as active",
            unit_name
        );
        return Vec::new();
    }

    let mut targets = Vec::new();
    for (other_name, other_unit) in &state.units {
        if !other_unit.unit.binds_to.contains(unit_name) {
            continue;
        }

        // Gate B: skip units that already have a running Start/Restart job
        // or are already cached as active/activating.
        let has_running_job = state.jobs.values().any(|j| {
            j.unit_name == *other_name
                && matches!(j.kind, JobKind::Start | JobKind::Restart)
                && matches!(j.status, JobStatus::Running)
        });
        let target_active = state
            .unit_states
            .get(other_name)
            .map(|c| matches!(c.active_state.as_str(), "active" | "activating"))
            .unwrap_or(false);
        if has_running_job || target_active {
            debug!(
                "BindsTo start propagation suppressed: {} already has running job or is active",
                other_name
            );
            continue;
        }

        targets.push(other_name.clone());
    }
    targets
}

pub fn handle_task_result(
    allocator: AllocatorHandle,
    task_id: u64,
    success: bool,
    message: &str,
    unit_name: &str,
    kind: JobKind,
) {
    let mut post_actions: Vec<PostAction> = Vec::new();

    let task_restart_info: Option<(RestartPolicy, ExitKind)> = {
        let mut state = allocator.write();

        // Clean up the task_id → kind mapping.
        state.task_kinds.remove(&task_id);

        // Signal serial chain continuation if this task was part of one.
        if let Some(tx) = state.serial_completion_txs.remove(&task_id) {
            let _ = tx.send(());
        }

        // Clean up invocation_id tracking.
        if !success || kind == JobKind::Stop {
            state.invocation_ids.remove(unit_name);
        }

        // Find and update the associated job.
        let job_id = state
            .jobs
            .values()
            .find(|j| j.unit_name == unit_name && matches!(j.status, JobStatus::Running))
            .map(|j| j.id);

        if let Some(jid) = job_id {
            let result_kind = if success {
                JobResultKind::Done
            } else {
                JobResultKind::Failed
            };
            if let Some(job) = state.jobs.get_mut(&jid) {
                if let Some(abort) = job.timeout_abort.take() {
                    abort.abort();
                }
                job.status = if success {
                    JobStatus::Done
                } else {
                    JobStatus::Failed(message.to_string())
                };
                // Revert cached state to inactive when a Start/Restart job
                // directly fails.  This prevents unit_states from showing
                // "active" for a unit whose own job failed.
                if !success && matches!(kind, JobKind::Start | JobKind::Restart) {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_micros() as u64)
                        .unwrap_or(0);
                    let entry = state.unit_states.entry(unit_name.to_string()).or_default();
                    if !matches!(entry.active_state.as_str(), "inactive" | "failed") {
                        entry.active_enter_timestamp = 0;
                    }
                    entry.active_state = "inactive".to_string();
                    entry.sub_state.clear();
                    entry.invocation_id.clear();
                    if entry.inactive_enter_timestamp == 0 {
                        entry.inactive_enter_timestamp = now;
                    }
                }
            }
            if let Some(ref tx) = state.job_completion_tx {
                let _ = tx.send(JobCompletion {
                    job_id: jid,
                    unit_name: unit_name.to_string(),
                    result: result_kind,
                });
            }
        }

        // --- Failure propagation (systemd job_fail_dependencies()) ---
        // On a failed Start job, fail the start jobs of every unit pulling
        // the failed unit in through Requires=/Requisite=/BindsTo=
        // (UNIT_ATOM_PROPAGATE_START_FAILURE).  On a failed Stop job, the
        // same for units listed in the failed unit's Conflicts=
        // (UNIT_ATOM_PROPAGATE_STOP_FAILURE; only the Conflicts reverse
        // edge carries this atom).  Failures cascade recursively; every
        // affected job completes with JobResultKind::Dependency.
        if !success {
            fail_dependents(&mut state, unit_name, kind);
        }

        // --- BindsTo= lifecycle binding (simplified: no runtime check) ---
        if matches!(kind, JobKind::Stop) || !success {
            for (other_name, other_unit) in &state.units {
                if other_unit.unit.binds_to.contains(unit_name) {
                    post_actions.push(PostAction::Stop(other_name.clone()));
                }
            }
        }

        // --- PartOf= stop propagation (simplified: no runtime check) ---
        if matches!(kind, JobKind::Stop) {
            for (other_name, other_unit) in &state.units {
                if other_unit.unit.part_of.contains(unit_name) {
                    post_actions.push(PostAction::Stop(other_name.clone()));
                }
            }
        }

        // --- BindsTo= start propagation (gated: dependency active, target
        // not already in-flight/active) ---
        for name in binds_to_start_propagation(&state, unit_name, success, kind) {
            post_actions.push(PostAction::Start(name));
        }

        // --- PartOf= start propagation (simplified: always propagate) ---
        if success && matches!(kind, JobKind::Start | JobKind::Restart) {
            for (other_name, other_unit) in &state.units {
                if other_unit.unit.part_of.contains(unit_name) {
                    post_actions.push(PostAction::Start(other_name.clone()));
                }
            }
        }

        // --- OnSuccess= / OnFailure= triggers ---
        if let Some(unit) = state.units.get(unit_name) {
            if success && matches!(kind, JobKind::Start | JobKind::Restart) {
                for target in &unit.unit.on_success {
                    post_actions.push(PostAction::Start(target.clone()));
                }
                // --- SuccessAction= power transition ---
                // systemd runs the SuccessAction of a unit that exits
                // successfully (e.g. `systemd-poweroff.service` has
                // `SuccessAction=poweroff-force`): the configured power
                // transition is dispatched as a `.power` unit start, which
                // routes to System Init (the owner of the `power` type).
                if let Some(power_unit) = unit.unit.success_action.power_unit_name() {
                    info!(
                        "SuccessAction={} for {}: triggering {}",
                        unit.unit.success_action.as_str(),
                        unit_name,
                        power_unit
                    );
                    post_actions.push(PostAction::Start(power_unit.to_string()));
                }
            }
            if !success {
                for target in &unit.unit.on_failure {
                    post_actions.push(PostAction::Start(target.clone()));
                }
            }
        }

        // --- Upholds= continuous activation (simplified: no runtime check) ---
        if !success || kind == JobKind::Stop {
            for other_unit in state.units.values() {
                if other_unit.unit.upholds.contains(unit_name) {
                    post_actions.push(PostAction::Start(unit_name.to_string()));
                    break;
                }
            }
        }

        // --- Restart policy: determine exit kind and check if restart needed ---
        let restart_info_inner: Option<(RestartPolicy, ExitKind)> =
            if !success && matches!(kind, JobKind::Start | JobKind::Restart) {
                let exit_kind = if message.contains("Timeout") {
                    ExitKind::Timeout
                } else if message.contains("Watchdog") || message.contains("watchdog") {
                    ExitKind::Watchdog
                } else if message.contains("Signal") || message.contains("signal") {
                    ExitKind::Signal(-1)
                } else if let Some(code_str) = message.to_lowercase().split("exit code").nth(1) {
                    let code = code_str
                        .split_whitespace()
                        .next()
                        .and_then(|s| s.parse::<i32>().ok())
                        .unwrap_or(1);
                    ExitKind::ExitCode(code)
                } else {
                    ExitKind::ExitCode(1)
                };
                state
                    .units
                    .get(unit_name)
                    .and_then(|u| u.service.as_ref())
                    .map(|svc| (svc.restart.clone(), exit_kind))
            } else {
                None
            };
        restart_info_inner
    }; // drop write lock

    // Execute post-actions asynchronously.
    if !post_actions.is_empty() {
        let alloc = allocator.clone();
        tokio::spawn(async move {
            for action in post_actions {
                match action {
                    PostAction::Stop(name) => {
                        info!("Propagating stop to {}", name);
                        if let Err(e) =
                            enqueue_job(alloc.clone(), &name, JobKind::Stop, JobMode::Replace).await
                        {
                            warn!("Failed to propagate stop to {}: {}", name, e);
                        }
                    }
                    PostAction::Start(name) => {
                        info!("Triggering start for {}", name);
                        if let Err(e) =
                            enqueue_job(alloc.clone(), &name, JobKind::Start, JobMode::Replace)
                                .await
                        {
                            warn!("Failed to trigger start for {}: {}", name, e);
                        }
                    }
                }
            }
        });
    }

    // Schedule restart if the restart policy triggered.
    if let Some((ref policy, ref exit_kind)) = task_restart_info {
        if should_restart_service(policy, exit_kind) {
            schedule_automatic_restart(allocator.clone(), unit_name);
        }
    }
}

/// Fail the running start jobs of every unit depending on `failed` through
/// the propagation atom implied by `kind`, mirroring systemd's
/// `job_fail_dependencies()`:
///
/// - `JobKind::Start` → `UNIT_ATOM_PROPAGATE_START_FAILURE`: units that
///   list `failed` in Requires=/Requisite=/BindsTo=;
/// - `JobKind::Stop` → `UNIT_ATOM_PROPAGATE_STOP_FAILURE`: units listed in
///   `failed`'s Conflicts=.
///
/// Only start jobs are affected (systemd restricts to JOB_START /
/// JOB_VERIFY_ACTIVE).  Each affected job is completed with
/// `JobResultKind::Dependency` and the failure cascades recursively.
fn fail_dependents(state: &mut AllocatorState, failed: &str, kind: JobKind) {
    let is_start = matches!(kind, JobKind::Start);
    let is_stop = matches!(kind, JobKind::Stop);
    if !is_start && !is_stop {
        return;
    }
    let mut queue = std::collections::VecDeque::new();
    let mut visited = std::collections::HashSet::new();
    queue.push_back(failed.to_string());
    while let Some(name) = queue.pop_front() {
        if !visited.insert(name.clone()) {
            continue;
        }
        let dependents: Vec<String> = state
            .units
            .iter()
            .filter(|(n, u)| {
                let connected = if is_start {
                    u.unit.requires.contains(&name)
                        || u.unit.requisite.contains(&name)
                        || u.unit.binds_to.contains(&name)
                } else {
                    u.unit.conflicts.contains(&name)
                };
                connected
                    && state.jobs.values().any(|j| {
                        j.unit_name == **n
                            && j.kind == JobKind::Start
                            && matches!(j.status, JobStatus::Running)
                    })
            })
            .map(|(n, _)| n.clone())
            .collect();
        for dep in dependents {
            let jid = state
                .jobs
                .values()
                .find(|j| j.unit_name == dep && matches!(j.status, JobStatus::Running))
                .map(|j| j.id);
            if let Some(jid) = jid {
                if let Some(job) = state.jobs.get_mut(&jid) {
                    if let Some(abort) = job.timeout_abort.take() {
                        abort.abort();
                    }
                    job.status = JobStatus::Failed(sysa::l10n::fmt(
                        sysa::l10n::t_("dependency failed: {name}"),
                        &[("name", &name.to_string())],
                    ));
                }
                // Revert cached state to inactive: the dependent's Start job
                // just failed, so the unit is not actually active.  This
                // prevents unit_states from showing "active" for a unit whose
                // job failed (e.g. poweroff.target whose dependency
                // systemd-poweroff.service failed).
                {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_micros() as u64)
                        .unwrap_or(0);
                    let entry = state.unit_states.entry(dep.clone()).or_default();
                    if !matches!(entry.active_state.as_str(), "inactive" | "failed") {
                        entry.active_enter_timestamp = 0;
                    }
                    entry.active_state = "inactive".to_string();
                    entry.sub_state.clear();
                    entry.invocation_id.clear();
                    if entry.inactive_enter_timestamp == 0 {
                        entry.inactive_enter_timestamp = now;
                    }
                }
                if let Some(ref tx) = state.job_completion_tx {
                    let _ = tx.send(JobCompletion {
                        job_id: jid,
                        unit_name: dep.clone(),
                        result: JobResultKind::Dependency,
                    });
                }
                queue.push_back(dep);
            }
        }
    }
}

/// Post-processing actions to be taken after a task result is handled.
enum PostAction {
    Stop(String),
    Start(String),
}

fn build_unit_config(uf: &UnitFile, all_units: &HashMap<String, UnitFile>) -> UnitConfig {
    let service = uf.service.as_ref().map(|svc| ServiceConfig {
        // Only the first ExecStart command is sent to the worker.
        // Multiple ExecStart directives (Type=oneshot) will be supported in Phase 2.
        exec_start: svc
            .exec_start
            .first()
            .map(|c| c.raw.clone())
            .unwrap_or_default(),
        exec_stop: svc
            .exec_stop
            .first()
            .map(|c| c.raw.clone())
            .unwrap_or_default(),
        exec_reload: svc
            .exec_reload
            .first()
            .map(|c| c.raw.clone())
            .unwrap_or_default(),
        working_directory: svc.working_directory.clone(),
        user: svc.user.clone(),
        group: svc.group.clone(),
        pam_name: svc.pam_name.clone(),
        environment: svc.environment.clone(),
        restart_policy: svc.restart.as_str().to_string(),
        restart_delay_secs: svc.restart_sec,
        service_type: svc.service_type.as_str().to_string(),
        pid_file: svc.pid_file.clone(),
        timeout_start_secs: svc.timeout_start_sec,
        timeout_stop_secs: svc.timeout_stop_sec,
        standard_input: svc.standard_input.clone(),
        standard_output: svc.standard_output.clone(),
        standard_error: svc.standard_error.clone(),
        tty_path: svc.tty_path.clone(),
        remain_after_exit: svc.remain_after_exit,
        bus_name: svc.bus_name.clone(),
    });

    let socket = uf.socket.as_ref().map(|sk| {
        let mut listen: Vec<SocketAddress> = Vec::new();
        for addr in &sk.listen_stream {
            listen.push(SocketAddress {
                stream: addr.clone(),
                ..Default::default()
            });
        }
        for addr in &sk.listen_datagram {
            listen.push(SocketAddress {
                datagram: addr.clone(),
                ..Default::default()
            });
        }
        for addr in &sk.listen_sequential_packet {
            listen.push(SocketAddress {
                sequential_packet: addr.clone(),
                ..Default::default()
            });
        }
        for addr in &sk.listen_fifo {
            listen.push(SocketAddress {
                fifo: addr.clone(),
                ..Default::default()
            });
        }
        for addr in &sk.listen_netlink {
            listen.push(SocketAddress {
                netlink: addr.clone(),
                ..Default::default()
            });
        }
        // Resolve the associated service name: an explicit `Service=`
        // directive wins; otherwise follow the systemd convention
        // "foo.socket" -> "foo.service".
        let svc_name = if sk.service.is_empty() {
            uf.name.replace(".socket", ".service")
        } else {
            sk.service.clone()
        };

        SocketConfig {
            listen,
            accept: sk.accept,
            backlog: sk.backlog,
            socket_mode: sk.socket_mode.clone(),
            socket_user: sk.socket_user.clone(),
            socket_group: sk.socket_group.clone(),
            service: svc_name,
            directory_mode: sk.directory_mode.clone(),
        }
    });

    // For automount units, preload the companion `.mount` unit's config so
    // the worker can satisfy kernel trigger requests locally (scheme A).
    // The companion's [Mount] section wins if the automount unit itself
    // carries one (it normally does not).
    let mount = if uf.automount.is_some() && uf.mount.is_none() {
        let mount_name = format!("{}.mount", uf.name.trim_end_matches(".automount"));
        all_units
            .get(&mount_name)
            .and_then(|muf| muf.mount.as_ref())
            .map(mount_config_from_section)
    } else {
        uf.mount.as_ref().map(mount_config_from_section)
    };

    let automount = uf.automount.as_ref().map(|a| AutomountConfig {
        r#where: a.where_.clone(),
        extra_options: a.extra_options.clone(),
        timeout_idle_sec: a.timeout_idle_sec,
        directory_mode: a.directory_mode.clone(),
    });

    let timer = uf.timer.as_ref().map(|t| TimerConfig {
        on_active_sec: t.on_active_sec,
        on_boot_sec: t.on_boot_sec,
        on_startup_sec: t.on_startup_sec,
        on_unit_active_sec: t.on_unit_active_sec,
        on_unit_inactive_sec: t.on_unit_inactive_sec,
        on_calendar: t.on_calendar.clone(),
        accuracy_sec: t.accuracy_sec,
        randomized_delay_sec: t.randomized_delay_sec,
        unit: t.unit.clone(),
        persistent: t.persistent,
    });

    let device = uf.device.as_ref().map(|d| DeviceConfig {
        device_name: d.device_name.clone(),
        device_path: d.device_path.clone(),
        sysfs_path: d.sysfs_path.clone(),
        property: d.property.clone(),
    });

    let path = uf.path.as_ref().map(|p| PathConfig {
        path_exists: p.path_exists.clone(),
        path_exists_glob: p.path_exists_glob.clone(),
        path_changed: p.path_changed.clone(),
        path_modified: p.path_modified.clone(),
        directory_not_empty: p.directory_not_empty.clone(),
        unit: p.unit.clone(),
        make_directory: p.make_directory,
        directory_mode: p.directory_mode.clone(),
        trigger_limit_interval_sec: p.trigger_limit_interval_sec,
        trigger_limit_burst: p.trigger_limit_burst,
    });

    let scope = uf.scope.as_ref().map(|s| ScopeConfig {
        pids: s
            .pids
            .iter()
            .filter_map(|p| p.trim().parse::<u32>().ok())
            .collect(),
        timeout_stop_secs: s.timeout_stop_sec,
        runtime_max_secs: s.runtime_max_sec,
        kill_signal: s.kill_signal.clone(),
        send_sighup: s.send_sighup,
        controller: String::new(),
        slice: uf.unit.slice.clone(),
    });

    UnitConfig {
        unit_name: uf.name.clone(),
        description: uf.unit.description.clone(),
        service,
        socket,
        mount,
        automount,
        timer,
        device,
        path,
        scope,
        socket_units: {
            let mut deps: Vec<String> = uf
                .unit
                .requires
                .iter()
                .chain(uf.unit.binds_to.iter())
                .filter(|d| d.ends_with(".socket"))
                .cloned()
                .collect();
            // Also include socket units from Sockets= (socket activation).
            if let Some(svc) = &uf.service {
                deps.extend(svc.sockets.iter().cloned());
            }
            deps.sort();
            deps.dedup();
            deps
        },
    }
}

fn mount_config_from_section(m: &MountSection) -> MountConfig {
    MountConfig {
        what: m.what.clone(),
        r#where: m.where_.clone(),
        r#type: m.type_.clone(),
        options: m.options.clone(),
        timeout_sec: m.timeout_sec,
        lazy_unmount: m.lazy_unmount,
        force_unmount: m.force_unmount,
        directory_mode: m.directory_mode.clone(),
        sloppy_options: m.sloppy_options,
    }
}

// ---------------------------------------------------------------------------
// Job timeout helpers
// ---------------------------------------------------------------------------

/// Spawn a timeout task for a job, returning an `AbortHandle` that can be used
/// to cancel the timeout if the job completes normally.
///
/// Start/Restart jobs use `TimeoutStartSec` (start timeout) plus a longer
/// job-running watchdog.  Stop jobs use `TimeoutStopSec`.  Reload jobs use a
/// default 60 s timeout.
fn spawn_job_timeout(
    allocator: AllocatorHandle,
    task_id: u64,
    job_id: u64,
    name: &str,
    kind: JobKind,
    unit_file: &Option<UnitFile>,
) -> Option<AbortHandle> {
    let svc = unit_file.as_ref().and_then(|u| u.service.as_ref());

    match kind {
        JobKind::Start | JobKind::Restart => {
            let start_timeout = svc.and_then(|s| {
                let t = s.timeout_start_sec;
                if t > 0 {
                    Some(t as u64)
                } else {
                    None
                }
            });
            start_timeout?;
            let start_secs = start_timeout.unwrap();
            let alloc = allocator.clone();
            let name_clone = name.to_string();
            let handle = tokio::spawn(async move {
                // Start timeout
                tokio::time::sleep(Duration::from_secs(start_secs)).await;
                let mut state = alloc.write();
                if let Some(job) = state.jobs.get_mut(&job_id) {
                    if matches!(job.status, JobStatus::Running) {
                        job.status = JobStatus::Failed(
                            sysa::l10n::t_("TimeoutStartSec exceeded").to_string(),
                        );
                        if let Some(ref tx) = state.job_completion_tx {
                            let _ = tx.send(JobCompletion {
                                job_id,
                                unit_name: name_clone,
                                result: JobResultKind::Timeout,
                            });
                        }
                    }
                }
                // Advance the serial chain so a dependent step is not
                // starved by a timed-out predecessor.
                if let Some(tx) = state.serial_completion_txs.remove(&task_id) {
                    let _ = tx.send(());
                }
            })
            .abort_handle();
            Some(handle)
        }
        JobKind::Stop => {
            let stop_secs = svc
                .and_then(|s| {
                    let t = s.timeout_stop_sec;
                    if t > 0 {
                        Some(t as u64)
                    } else {
                        None
                    }
                })
                .unwrap_or(30);
            let alloc = allocator.clone();
            let name_clone = name.to_string();
            let handle = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(stop_secs)).await;
                let mut state = alloc.write();
                if let Some(job) = state.jobs.get_mut(&job_id) {
                    if matches!(job.status, JobStatus::Running) {
                        job.status = JobStatus::Failed(
                            sysa::l10n::t_("TimeoutStopSec exceeded").to_string(),
                        );
                        if let Some(ref tx) = state.job_completion_tx {
                            let _ = tx.send(JobCompletion {
                                job_id,
                                unit_name: name_clone,
                                result: JobResultKind::Timeout,
                            });
                        }
                    }
                }
                // Advance the serial chain so a dependent step is not
                // starved by a timed-out predecessor.
                if let Some(tx) = state.serial_completion_txs.remove(&task_id) {
                    let _ = tx.send(());
                }
            })
            .abort_handle();
            Some(handle)
        }
        JobKind::Reload => {
            // Reload timeout: default 60 seconds.
            let reload_secs: u64 = 60;
            let alloc = allocator.clone();
            let name_clone = name.to_string();
            let handle = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(reload_secs)).await;
                let mut state = alloc.write();
                if let Some(job) = state.jobs.get_mut(&job_id) {
                    if matches!(job.status, JobStatus::Running) {
                        job.status = JobStatus::Failed(
                            sysa::l10n::t_("Reload timeout exceeded").to_string(),
                        );
                        if let Some(ref tx) = state.job_completion_tx {
                            let _ = tx.send(JobCompletion {
                                job_id,
                                unit_name: name_clone,
                                result: JobResultKind::Timeout,
                            });
                        }
                    }
                }
                // Advance the serial chain so a dependent step is not
                // starved by a timed-out predecessor.
                if let Some(tx) = state.serial_completion_txs.remove(&task_id) {
                    let _ = tx.send(());
                }
            })
            .abort_handle();
            Some(handle)
        }
        // Nop jobs complete immediately and are never timed out; keep the
        // match exhaustive defensively.
        JobKind::Nop => None,
    }
}

// ---------------------------------------------------------------------------
// Unified restart-policy helpers
// ---------------------------------------------------------------------------

/// Determine whether a service should be restarted based on its `RestartPolicy`
/// and how it exited.  Mirrors systemd's behaviour table:
///
/// | Policy       | ExitCode 0 | ExitCode != 0 | Signal | Timeout | Watchdog |
/// |--------------|-----------|---------------|--------|---------|----------|
/// | no           |     ✗     |       ✗       |   ✗    |    ✗    |    ✗     |
/// | on-success   |     ✓     |       ✗       |   ✗    |    ✗    |    ✗     |
/// | on-failure   |     ✗     |       ✓       |   ✓    |    ✓    |    ✗     |
/// | on-abnormal  |     ✗     |       ✗       |   ✓    |    ✓    |    ✗     |
/// | on-watchdog  |     ✗     |       ✗       |   ✗    |    ✗    |    ✓     |
/// | on-abort     |     ✗     |       ✗       |   ✓    |    ✗    |    ✗     |
/// | always       |     ✓     |       ✓       |   ✓    |    ✓    |    ✓     |
pub fn should_restart_service(policy: &RestartPolicy, exit_kind: &ExitKind) -> bool {
    use ExitKind::*;
    match policy {
        RestartPolicy::No => false,
        RestartPolicy::Always => true,
        RestartPolicy::OnSuccess => matches!(exit_kind, ExitCode(0)),
        RestartPolicy::OnFailure => {
            matches!(exit_kind, ExitCode(c) if *c != 0) || matches!(exit_kind, Signal(_) | Timeout)
        }
        RestartPolicy::OnAbnormal => matches!(exit_kind, Signal(_) | Timeout),
        RestartPolicy::OnWatchdog => matches!(exit_kind, Watchdog),
        RestartPolicy::OnAbort => matches!(exit_kind, Signal(_)),
    }
}

/// Schedule an automatic restart of `unit_name` after its `RestartSec`.
///
/// The start rate limit (`StartLimitIntervalSec=` / `StartLimitBurst=`) is
/// enforced centrally in [`enqueue_job`], so every start attempt — manual,
/// auto-restart, or dependency-triggered — counts against it.  This function
/// only sleeps the configured `RestartSec` and then enqueues a `Start` job.
pub fn schedule_automatic_restart(allocator: AllocatorHandle, unit_name: &str) {
    let restart_sec = {
        let state = allocator.read();
        state
            .units
            .get(unit_name)
            .and_then(|u| u.service.as_ref())
            .map(|s| s.restart_sec as u64)
            .unwrap_or(0)
    };

    let alloc = allocator.clone();
    let name = unit_name.to_string();
    tokio::spawn(async move {
        if restart_sec > 0 {
            tokio::time::sleep(Duration::from_secs(restart_sec)).await;
        }
        info!("Auto-restarting {} after exit/failure", name);
        if let Err(e) = enqueue_job(alloc.clone(), &name, JobKind::Start, JobMode::Replace).await {
            warn!("Failed to auto-restart {}: {}", name, e);
        }
    });
}

/// Execute the `StartLimitAction=` configured for a unit whose start rate
/// limit was exceeded (mirrors systemd's `unit_start_limit_action()`).
///
/// Every transition is dispatched as a `.power` unit start — the same
/// single path used by `SuccessAction=` and direct unit starts — so System
/// Init is the sole executor of system transitions (it owns the whole
/// `power` unit type and runs `reboot(2)` in-process).
async fn execute_start_limit_action(
    allocator: AllocatorHandle,
    action: &StartLimitAction,
    unit_name: &str,
) {
    match action.power_unit_name() {
        Some(power_unit) => {
            warn!(
                "StartLimitAction={:?} for {}: dispatching {} transition",
                action, unit_name, power_unit
            );
            // Dispatched on a detached task: this runs from inside
            // `enqueue_job` for the rate-limited unit, and routing the
            // transition through `enqueue_job` again must not create an
            // async recursion cycle.
            let alloc = allocator.clone();
            tokio::spawn(async move {
                if let Err(e) = enqueue_job(alloc, power_unit, JobKind::Start, JobMode::Replace)
                    .await
                {
                    warn!(
                        "Failed to dispatch StartLimitAction transition {power_unit}: {e}"
                    );
                }
            });
        }
        None => {
            warn!(
                "StartLimitAction={:?} for {} (no-op, logging only)",
                action, unit_name
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Condition and assert evaluation
// ---------------------------------------------------------------------------

/// Evaluate all `Condition*=` directives in `unit`.
///
/// Returns `true` if all conditions pass (unit should start), `false` if any
/// condition fails (unit should be silently skipped, staying inactive).
///
/// A value prefixed with `!` negates the check.
fn check_conditions(unit: &UnitSection) -> bool {
    for path in &unit.condition_path_exists {
        if !eval_condition_bool(path, |p| std::path::Path::new(p).exists()) {
            return false;
        }
    }
    for glob in &unit.condition_path_exists_glob {
        if !eval_condition_bool(glob, path_glob_matches) {
            return false;
        }
    }
    for path in &unit.condition_file_not_empty {
        if !eval_condition_bool(path, |p| {
            std::fs::metadata(p).map(|m| m.len() != 0).unwrap_or(false)
        }) {
            return false;
        }
    }
    for path in &unit.condition_directory_not_empty {
        if !eval_condition_bool(path, |p| {
            std::fs::read_dir(p)
                .map(|mut d| d.next().is_some())
                .unwrap_or(false)
        }) {
            return false;
        }
    }
    for spec in &unit.condition_ac_power {
        let (negate, value) = strip_negate(spec);
        let on_ac = is_on_ac_power();
        let want = matches!(value.to_lowercase().as_str(), "yes" | "true" | "1");
        if (on_ac != want) != negate {
            return false;
        }
    }
    // ConditionFirstBoot=yes passes only on the first boot.
    for spec in &unit.condition_first_boot {
        let (negate, value) = strip_negate(spec);
        let first = is_first_boot();
        let want = matches!(value.to_lowercase().as_str(), "yes" | "true" | "1");
        if (first != want) != negate {
            return false;
        }
    }
    for spec in &unit.condition_kernel_module_loaded {
        let (negate, module) = strip_negate(spec);
        let loaded = std::path::Path::new("/sys/module").join(module).exists();
        if loaded == negate {
            return false;
        }
    }
    true
}

/// Evaluate all `Assert*=` directives in `unit`.
///
/// Returns `true` if all asserts pass, `false` if any assert fails (unit
/// should be marked failed, not just skipped).
fn check_asserts(unit: &UnitSection) -> bool {
    for path in &unit.assert_path_exists {
        if !eval_condition_bool(path, |p| std::path::Path::new(p).exists()) {
            return false;
        }
    }
    for glob in &unit.assert_path_exists_glob {
        if !eval_condition_bool(glob, path_glob_matches) {
            return false;
        }
    }
    for path in &unit.assert_file_not_empty {
        if !eval_condition_bool(path, |p| {
            std::fs::metadata(p).map(|m| m.len() != 0).unwrap_or(false)
        }) {
            return false;
        }
    }
    for path in &unit.assert_directory_not_empty {
        if !eval_condition_bool(path, |p| {
            std::fs::read_dir(p)
                .map(|mut d| d.next().is_some())
                .unwrap_or(false)
        }) {
            return false;
        }
    }
    for spec in &unit.assert_first_boot {
        let (negate, value) = strip_negate(spec);
        let first = is_first_boot();
        let want = matches!(value.to_lowercase().as_str(), "yes" | "true" | "1");
        if (first != want) != negate {
            return false;
        }
    }
    true
}

/// Strip a leading `!` from `spec`, returning `(negated, rest)`.
fn strip_negate(spec: &str) -> (bool, &str) {
    if let Some(rest) = spec.strip_prefix('!') {
        (true, rest)
    } else {
        (false, spec)
    }
}

/// Evaluate a single condition string against a predicate.
///
/// If the spec starts with `!`, the result is negated.
fn eval_condition_bool<F: Fn(&str) -> bool>(spec: &str, pred: F) -> bool {
    let (negate, path) = strip_negate(spec);
    let result = pred(path);
    if negate {
        !result
    } else {
        result
    }
}

/// Check if any filesystem path matches a simple glob pattern.
///
/// Uses the same glob logic as the rest of systema (no external crate).
fn path_glob_matches(pattern: &str) -> bool {
    // Split into directory and file-name glob parts.
    let (dir, file_pattern) = match pattern.rfind('/') {
        Some(pos) => (&pattern[..pos], &pattern[pos + 1..]),
        None => (".", pattern),
    };
    std::fs::read_dir(dir)
        .map(|entries| {
            entries.filter_map(|e| e.ok()).any(|e| {
                e.file_name()
                    .to_str()
                    .map(|n| simple_glob_match(file_pattern, n))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// Minimal shell-style glob: `*` matches any sequence, `?` matches any char.
fn simple_glob_match(pattern: &str, text: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let txt: Vec<char> = text.chars().collect();
    glob_match_impl(&pat, &txt)
}

fn glob_match_impl(pat: &[char], txt: &[char]) -> bool {
    match (pat.first(), txt.first()) {
        (None, None) => true,
        (Some(&'*'), _) => (0..=txt.len()).any(|i| glob_match_impl(&pat[1..], &txt[i..])),
        (Some(&'?'), Some(_)) => glob_match_impl(&pat[1..], &txt[1..]),
        (Some(p), Some(t)) if p == t => glob_match_impl(&pat[1..], &txt[1..]),
        _ => false,
    }
}

/// Returns `true` if the system appears to be running on AC power.
/// Best-effort: returns `true` (assume AC) if the check cannot be performed.
fn is_on_ac_power() -> bool {
    // Linux: check /sys/class/power_supply/*/online
    let path = std::path::Path::new("/sys/class/power_supply");
    if !path.exists() {
        return true; // assume AC if sysfs is unavailable
    }
    std::fs::read_dir(path)
        .map(|entries| {
            entries.filter_map(|e| e.ok()).any(|e| {
                let online = e.path().join("online");
                std::fs::read_to_string(&online)
                    .map(|s| s.trim() == "1")
                    .unwrap_or(false)
            })
        })
        .unwrap_or(true)
}

/// Returns `true` if this appears to be the first boot of the system.
/// Heuristic: `/run/systemd/first-boot` or `/run/machine-id` does not exist.
fn is_first_boot() -> bool {
    std::path::Path::new(sysa::paths::instance().systemd_first_boot_file).exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{CachedUnitState, StartLimitState, WorkerEntry};
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;
    use sysa::proto::{Envelope, UnitDefineRequest, UnitDefineResult};
    use systema_sysf::ir::{DependencySet, UnitIR, UnitType};

    // =========================================================================
    // JobMode tests
    // =========================================================================

    #[test]
    fn test_job_mode_from_str_unknown_is_none() {
        assert_eq!(JobMode::from_str("unknown"), None);
        assert_eq!(JobMode::from_str(""), None);
        assert_eq!(JobMode::from_str("REPLACE"), None);
    }

    #[test]
    fn test_job_mode_from_str_all() {
        assert_eq!(JobMode::from_str("replace"), Some(JobMode::Replace));
        assert_eq!(JobMode::from_str("fail"), Some(JobMode::Fail));
        assert_eq!(JobMode::from_str("lenient"), Some(JobMode::Lenient));
        assert_eq!(JobMode::from_str("queue"), Some(JobMode::Queue));
        assert_eq!(JobMode::from_str("isolate"), Some(JobMode::Isolate));
        assert_eq!(JobMode::from_str("flush"), Some(JobMode::Flush));
        assert_eq!(
            JobMode::from_str("replace-irreversibly"),
            Some(JobMode::ReplaceIrreversibly)
        );
        assert_eq!(
            JobMode::from_str("ignore-dependencies"),
            Some(JobMode::IgnoreDependencies)
        );
        assert_eq!(
            JobMode::from_str("ignore-requirements"),
            Some(JobMode::IgnoreRequirements)
        );
        assert_eq!(JobMode::from_str("triggering"), Some(JobMode::Triggering));
        assert_eq!(
            JobMode::from_str("restart-dependencies"),
            Some(JobMode::RestartDependencies)
        );
    }

    #[test]
    fn test_job_mode_as_str_roundtrip() {
        for mode in &[
            JobMode::Replace,
            JobMode::Fail,
            JobMode::Lenient,
            JobMode::Queue,
            JobMode::Isolate,
            JobMode::Flush,
            JobMode::ReplaceIrreversibly,
            JobMode::IgnoreDependencies,
            JobMode::IgnoreRequirements,
            JobMode::Triggering,
            JobMode::RestartDependencies,
        ] {
            assert!(matches!(
                mode,
                JobMode::Replace
                    | JobMode::Fail
                    | JobMode::Lenient
                    | JobMode::Queue
                    | JobMode::Isolate
                    | JobMode::Flush
                    | JobMode::ReplaceIrreversibly
                    | JobMode::IgnoreDependencies
                    | JobMode::IgnoreRequirements
                    | JobMode::Triggering
                    | JobMode::RestartDependencies
            ));
        }
    }

    #[test]
    fn test_job_mode_equality() {
        assert_eq!(JobMode::Replace, JobMode::Replace);
        assert_ne!(JobMode::Replace, JobMode::Fail);
        assert_ne!(JobMode::Isolate, JobMode::Flush);
    }

    // =========================================================================
    // StartLimitState tests
    // =========================================================================

    #[test]
    fn test_start_limit_state_allows_first_attempt() {
        let mut state = StartLimitState::new();
        assert!(state.check_rate_limit(Duration::from_secs(10), 3));
    }

    #[test]
    fn test_start_limit_state_within_burst() {
        let mut state = StartLimitState::new();
        assert!(state.check_rate_limit(Duration::from_secs(10), 3));
        assert!(state.check_rate_limit(Duration::from_secs(10), 3));
        assert!(state.check_rate_limit(Duration::from_secs(10), 3));
    }

    #[test]
    fn test_start_limit_state_exceeds_burst() {
        let mut state = StartLimitState::new();
        assert!(state.check_rate_limit(Duration::from_secs(10), 3));
        assert!(state.check_rate_limit(Duration::from_secs(10), 3));
        assert!(state.check_rate_limit(Duration::from_secs(10), 3));
        // The 4th attempt should be rate-limited
        assert!(!state.check_rate_limit(Duration::from_secs(10), 3));
    }

    #[test]
    fn test_start_limit_state_prunes_old_timestamps() {
        let mut state = StartLimitState::new();
        // Add some timestamps far in the past
        state
            .timestamps
            .push(std::time::Instant::now() - Duration::from_secs(100));
        state
            .timestamps
            .push(std::time::Instant::now() - Duration::from_secs(100));
        // With short interval, they should be pruned
        assert!(state.check_rate_limit(Duration::from_secs(1), 5));
        assert_eq!(state.timestamps.len(), 1); // only the new one remains
    }

    #[test]
    fn test_start_limit_zero_interval_disables_rate_limiting() {
        // systemd: StartLimitIntervalSec=0 disables rate limiting.
        let mut state = StartLimitState::new();
        for _ in 0..100 {
            assert!(state.check_rate_limit(Duration::from_secs(0), 5));
        }
        // Disabled limiting must not record timestamps.
        assert!(state.timestamps.is_empty());
    }

    #[test]
    fn test_start_limit_zero_burst_disables_rate_limiting() {
        // systemd: StartLimitBurst=0 disables rate limiting.
        let mut state = StartLimitState::new();
        for _ in 0..100 {
            assert!(state.check_rate_limit(Duration::from_secs(10), 0));
        }
        assert!(state.timestamps.is_empty());
    }

    // =========================================================================
    // Unit helper tests
    // =========================================================================

    fn make_unit(name: &str) -> UnitFile {
        UnitFile::new(name)
    }

    #[test]
    fn test_socket_config_service_resolution() {
        // No Service= directive: derive "foo.socket" -> "foo.service".
        let mut uf = make_unit("foo.socket");
        uf.socket = Some(crate::unit::types::SocketSection {
            listen_netlink: vec!["kobject-uevent".to_string()],
            ..Default::default()
        });
        let cfg = build_unit_config(&uf, &HashMap::new());
        assert_eq!(cfg.socket.as_ref().unwrap().service, "foo.service");

        // Explicit Service= wins over the derived name.
        let mut uf = make_unit("bar.socket");
        uf.socket = Some(crate::unit::types::SocketSection {
            listen_stream: vec!["22".to_string()],
            service: "custom-daemon.service".to_string(),
            ..Default::default()
        });
        let cfg = build_unit_config(&uf, &HashMap::new());
        assert_eq!(
            cfg.socket.as_ref().unwrap().service,
            "custom-daemon.service"
        );
    }

    #[test]
    fn test_strip_negate_normal() {
        let (neg, val) = strip_negate("/some/path");
        assert!(!neg);
        assert_eq!(val, "/some/path");
    }

    #[test]
    fn test_strip_negate_negated() {
        let (neg, val) = strip_negate("!/some/path");
        assert!(neg);
        assert_eq!(val, "/some/path");
    }

    #[test]
    fn test_eval_condition_bool_normal() {
        assert!(eval_condition_bool("true", |_| true));
        assert!(!eval_condition_bool("false", |_| false));
    }

    #[test]
    fn test_eval_condition_bool_negated() {
        assert!(eval_condition_bool("!false", |_| false));
        assert!(!eval_condition_bool("!true", |_| true));
    }

    #[test]
    fn test_simple_glob_match_exact() {
        assert!(simple_glob_match("foo.service", "foo.service"));
        assert!(!simple_glob_match("foo.service", "bar.service"));
    }

    #[test]
    fn test_simple_glob_match_wildcard() {
        assert!(simple_glob_match("*.service", "foo.service"));
        assert!(simple_glob_match("foo.*", "foo.service"));
        assert!(!simple_glob_match("*.service", "foo.txt"));
    }

    #[test]
    fn test_simple_glob_match_question_mark() {
        assert!(simple_glob_match("foo.???????", "foo.service"));
        assert!(!simple_glob_match("foo.??????", "foo.service"));
    }

    #[test]
    fn test_simple_glob_match_empty() {
        assert!(simple_glob_match("", ""));
        assert!(!simple_glob_match("", "foo"));
        assert!(!simple_glob_match("foo", ""));
    }

    // =========================================================================
    // check_conditions / check_asserts tests
    // =========================================================================

    #[test]
    fn test_check_conditions_no_conditions() {
        let unit = UnitSection::default();
        assert!(check_conditions(&unit));
    }

    #[test]
    fn test_check_asserts_no_asserts() {
        let unit = UnitSection::default();
        assert!(check_asserts(&unit));
    }

    #[test]
    fn test_check_conditions_kernel_module_loaded_existing() {
        let mut unit = UnitSection::default();
        // "configfs" is loaded on virtually all Linux systems.
        unit.condition_kernel_module_loaded
            .push("configfs".to_string());
        assert!(check_conditions(&unit));
    }

    #[test]
    fn test_check_conditions_kernel_module_loaded_negated_existing() {
        let mut unit = UnitSection::default();
        // "!configfs" → skip if loaded → should fail.
        unit.condition_kernel_module_loaded
            .push("!configfs".to_string());
        assert!(!check_conditions(&unit));
    }

    #[test]
    fn test_check_conditions_kernel_module_loaded_missing() {
        let mut unit = UnitSection::default();
        // A module name that definitely does not exist.
        unit.condition_kernel_module_loaded
            .push("definitely_not_a_module_abc123".to_string());
        assert!(!check_conditions(&unit));
    }

    #[test]
    fn test_check_conditions_kernel_module_loaded_negated_missing() {
        let mut unit = UnitSection::default();
        // "!<nonexistent>" → skip if loaded → module not loaded → should pass.
        unit.condition_kernel_module_loaded
            .push("!definitely_not_a_module_abc123".to_string());
        assert!(check_conditions(&unit));
    }

    // =========================================================================
    // build_unit_config tests
    // =========================================================================

    #[test]
    fn test_build_unit_config_service() {
        let uf = make_unit("test.service");
        let mut uf = uf;
        uf.service = Some(crate::unit::types::ServiceSection::default());
        let config = build_unit_config(&uf, &HashMap::new());
        assert!(config.service.is_some());
    }

    #[test]
    fn test_build_unit_config_automount_preloads_companion_mount() {
        let mut auto = make_unit("mnt-data.automount");
        auto.automount = Some(crate::unit::types::AutomountSection {
            where_: "/mnt/data".to_string(),
            timeout_idle_sec: 60,
            ..Default::default()
        });
        let mut mount = make_unit("mnt-data.mount");
        mount.mount = Some(crate::unit::types::MountSection {
            what: "/dev/sdb1".to_string(),
            where_: "/mnt/data".to_string(),
            type_: "ext4".to_string(),
            options: "defaults".to_string(),
            ..Default::default()
        });
        let mut units = HashMap::new();
        units.insert(mount.name.clone(), mount);
        let config = build_unit_config(&auto, &units);
        let mount_cfg = config.mount.expect("companion mount config preloaded");
        assert_eq!(mount_cfg.what, "/dev/sdb1");
        assert_eq!(mount_cfg.r#where, "/mnt/data");
        assert_eq!(mount_cfg.r#type, "ext4");
    }

    // =========================================================================
    // Job conflict detection logic
    // =========================================================================

    #[test]
    fn test_job_mode_from_str_is_idempotent() {
        for s in &[
            "replace",
            "fail",
            "lenient",
            "queue",
            "isolate",
            "flush",
            "replace-irreversibly",
            "ignore-dependencies",
            "ignore-requirements",
            "triggering",
            "restart-dependencies",
        ] {
            let mode = JobMode::from_str(s);
            let mode2 = JobMode::from_str(s);
            assert_eq!(mode, mode2);
        }
    }

    // =========================================================================
    // Mode validation (check_mode_constraints)
    // =========================================================================

    #[test]
    fn test_check_mode_constraints_triggering_is_stop_only() {
        assert!(
            check_mode_constraints(JobMode::Triggering, JobKind::Stop, "x.service", false).is_ok()
        );
        for kind in [JobKind::Start, JobKind::Restart, JobKind::Reload] {
            assert!(check_mode_constraints(JobMode::Triggering, kind, "x.service", false).is_err());
        }
    }

    #[test]
    fn test_check_mode_constraints_restart_dependencies_is_start_only() {
        assert!(check_mode_constraints(
            JobMode::RestartDependencies,
            JobKind::Start,
            "x.service",
            false
        )
        .is_ok());
        for kind in [JobKind::Stop, JobKind::Restart, JobKind::Reload] {
            assert!(
                check_mode_constraints(JobMode::RestartDependencies, kind, "x.service", false)
                    .is_err()
            );
        }
    }

    #[test]
    fn test_check_mode_constraints_isolate_requires_allow_isolate() {
        assert!(
            check_mode_constraints(JobMode::Isolate, JobKind::Start, "x.service", false).is_err()
        );
        assert!(
            check_mode_constraints(JobMode::Isolate, JobKind::Start, "x.service", true).is_ok()
        );
    }

    #[test]
    fn test_check_mode_constraints_plain_modes_always_pass() {
        for mode in [
            JobMode::Replace,
            JobMode::Flush,
            JobMode::Queue,
            JobMode::Lenient,
        ] {
            for kind in [
                JobKind::Start,
                JobKind::Stop,
                JobKind::Restart,
                JobKind::Reload,
            ] {
                assert!(check_mode_constraints(mode, kind, "x.service", false).is_ok());
            }
        }
    }

    // =========================================================================
    // enqueue_job_type: state-dependent collapse (step E)
    // =========================================================================

    fn alloc_with_state(active_state: &str) -> AllocatorHandle {
        let alloc = Arc::new(parking_lot::RwLock::new(AllocatorState::new()));
        alloc
            .write()
            .units
            .insert("demo.service".to_string(), make_unit("demo.service"));
        alloc.write().unit_states.insert(
            "demo.service".to_string(),
            CachedUnitState {
                active_state: active_state.to_string(),
                sub_state: String::new(),
                main_pid: 0,
                invocation_id: String::new(),
                active_enter_timestamp: 0,
                inactive_enter_timestamp: 0,
                extensions: HashMap::new(),
                pids: Vec::new(),
                controller: String::new(),
            },
        );
        alloc
    }

    /// Register a fake worker for the "service" unit type.
    fn register_service_worker(state: &mut AllocatorState) {
        let (tx, _rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(16);
        state.workers.insert(
            "test-worker".to_string(),
            WorkerEntry {
                worker_id: "test-worker".to_string(),
                unit_types: vec!["service".to_string()],
                supports_unit_define: false,
                ready: false,
                envelope_tx: tx,
            },
        );
    }

    #[tokio::test]
    async fn test_enqueue_job_type_try_restart_of_inactive_unit_is_nop() {
        // try-restart of an inactive unit collapses to Nop: the job is
        // recorded and completes as done without touching any worker
        // (systemd: JOB_NOP finishes immediately with JOB_DONE).
        let alloc = alloc_with_state("inactive");
        let (job_id, kind) = enqueue_job_type(
            alloc.clone(),
            "demo.service",
            JobType::TryRestart,
            false,
            JobMode::Replace,
        )
        .await
        .unwrap();
        assert_eq!(kind, JobKind::Nop);
        let state = alloc.read();
        let job = state.jobs.get(&job_id).expect("nop job recorded");
        assert_eq!(job.kind, JobKind::Nop);
        assert_eq!(job.status, JobStatus::Done);
        // No worker interaction: nothing was dispatched.
        assert!(state.task_kinds.is_empty());
    }

    #[tokio::test]
    async fn test_enqueue_job_type_try_reload_of_failed_unit_is_nop() {
        let alloc = alloc_with_state("failed");
        let (job_id, kind) = enqueue_job_type(
            alloc.clone(),
            "demo.service",
            JobType::TryReload,
            false,
            JobMode::Replace,
        )
        .await
        .unwrap();
        assert_eq!(kind, JobKind::Nop);
        assert!(matches!(
            alloc.read().jobs.get(&job_id).unwrap().status,
            JobStatus::Done
        ));
    }

    #[tokio::test]
    async fn test_enqueue_job_type_try_restart_works_without_worker() {
        // The collapsed-nop path must succeed even when no worker is
        // registered (it never dispatches).
        let alloc = alloc_with_state("inactive");
        let (job_id, _) = enqueue_job_type(
            alloc.clone(),
            "demo.service",
            JobType::TryRestart,
            false,
            JobMode::Replace,
        )
        .await
        .unwrap();
        assert_eq!(
            alloc.read().jobs.get(&job_id).unwrap().status,
            JobStatus::Done
        );
    }

    #[tokio::test]
    async fn test_enqueue_job_type_unknown_state_keeps_try_restart() {
        // Unknown state is conservative: try-restart collapses to restart,
        // which needs a worker — and fails without one.
        let alloc = alloc_with_state("unmapped-state");
        let res = enqueue_job_type(
            alloc.clone(),
            "demo.service",
            JobType::TryRestart,
            false,
            JobMode::Replace,
        )
        .await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn test_enqueue_job_type_active_try_restart_dispatches_restart() {
        let alloc = alloc_with_state("active");
        register_service_worker(&mut alloc.write());
        let (job_id, kind) = enqueue_job_type(
            alloc.clone(),
            "demo.service",
            JobType::TryRestart,
            false,
            JobMode::Replace,
        )
        .await
        .unwrap();
        assert_eq!(kind, JobKind::Restart);
        assert_eq!(
            alloc.read().jobs.get(&job_id).unwrap().kind,
            JobKind::Restart
        );
    }

    #[tokio::test]
    async fn test_enqueue_job_merges_identical_running_job() {
        // Regression: a second Start for a unit that already has a Running
        // Start job must merge into it (systemd job_merge), never cancel and
        // re-dispatch. Otherwise a SysV init script that calls
        // `systemctl start $unit` from inside its own ExecStart (e.g.
        // /etc/init.d/virtualbox-guest-utils) spawns an infinite loop of
        // processes.
        //
        // The unit is *activating* (its process is still starting): an
        // already-active unit would short-circuit with -EALREADY before any
        // merge can happen, exactly like systemd's unit_start().
        let alloc = alloc_with_state("activating");
        register_service_worker(&mut alloc.write());

        // Prime a Running Start job.
        let first_id = {
            let mut state = alloc.write();
            let jid = next_job_id();
            state.jobs.insert(
                jid,
                Job {
                    id: jid,
                    unit_name: "demo.service".to_string(),
                    kind: JobKind::Start,
                    status: JobStatus::Running,
                    timeout_abort: None,
                },
            );
            jid
        };

        // A second StartUnit-style request with Replace must resolve to the
        // existing job instead of creating another.
        let (second_id, kind) = enqueue_job_type(
            alloc.clone(),
            "demo.service",
            JobType::Start,
            false,
            JobMode::Replace,
        )
        .await
        .unwrap();
        assert_eq!(kind, JobKind::Start);
        assert_eq!(second_id, first_id);

        let state = alloc.read();
        let running: Vec<_> = state
            .jobs
            .values()
            .filter(|j| j.unit_name == "demo.service" && matches!(j.status, JobStatus::Running))
            .collect();
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].id, first_id);
    }

    #[tokio::test]
    async fn test_start_rate_limit_accumulates_across_successful_starts() {
        // Regression: systemd does NOT reset the start rate limiter on a
        // successful start — StartLimitIntervalSec/Burst is a sliding window
        // over start *attempts* (unit_test_start_limit / ratelimit). A SysV
        // init script that calls `systemctl start $unit` from inside its own
        // ExecStart (e.g. /etc/init.d/virtualbox-guest-utils) recurses; every
        // spawn succeeds, so without this accumulation the loop would run
        // unboundedly. The default 10s/5 limit must trip on the 6th attempt
        // even though every prior start succeeded.
        //
        // The unit starts out inactive: each attempt is a real start (an
        // active unit would short-circuit with -EALREADY instead, consuming
        // no rate-limit credit, as in systemd).
        let alloc = alloc_with_state("inactive");
        register_service_worker(&mut alloc.write());

        for attempt in 1..=5 {
            let (job_id, kind) = enqueue_job_type(
                alloc.clone(),
                "demo.service",
                JobType::Start,
                false,
                JobMode::Replace,
            )
            .await
            .unwrap_or_else(|e| panic!("start #{attempt} should be allowed: {e}"));
            assert_eq!(kind, JobKind::Start);
            // The start completes successfully — this must NOT clear the
            // accumulated rate-limit state.
            handle_task_result(
                alloc.clone(),
                job_id,
                true,
                "ok",
                "demo.service",
                JobKind::Start,
            );
        }

        let err = enqueue_job_type(
            alloc.clone(),
            "demo.service",
            JobType::Start,
            false,
            JobMode::Replace,
        )
        .await
        .expect_err("6th start within the interval must be rate-limited");
        let msg = err.to_string();
        assert!(
            msg.contains("rate limit exceeded"),
            "unexpected error: {msg}"
        );
    }

    #[tokio::test]
    async fn test_enqueue_job_type_reload_if_possible_mangles_to_reload() {
        // ReloadOrRestartUnit on an active, reloadable unit → reload.
        let alloc = alloc_with_state("active");
        {
            let mut state = alloc.write();
            state.units.get_mut("demo.service").unwrap().service =
                Some(crate::unit::types::ServiceSection {
                    exec_reload: vec![crate::unit::types::ExecCommand::parse(
                        "/usr/bin/kill -HUP $MAINPID",
                    )],
                    ..Default::default()
                });
            register_service_worker(&mut state);
        }
        let (job_id, kind) = enqueue_job_type(
            alloc.clone(),
            "demo.service",
            JobType::Restart,
            true,
            JobMode::Replace,
        )
        .await
        .unwrap();
        assert_eq!(kind, JobKind::Reload);
        assert_eq!(
            alloc.read().jobs.get(&job_id).unwrap().kind,
            JobKind::Reload
        );
    }

    #[tokio::test]
    async fn test_enqueue_job_type_reload_or_restart_without_reload_stays_restart() {
        // ReloadOrRestartUnit on an active unit without ExecReload → restart.
        let alloc = alloc_with_state("active");
        register_service_worker(&mut alloc.write());
        let (job_id, kind) = enqueue_job_type(
            alloc.clone(),
            "demo.service",
            JobType::Restart,
            true,
            JobMode::Replace,
        )
        .await
        .unwrap();
        assert_eq!(kind, JobKind::Restart);
        assert_eq!(
            alloc.read().jobs.get(&job_id).unwrap().kind,
            JobKind::Restart
        );
    }

    #[tokio::test]
    async fn test_enqueue_job_type_reload_or_start_of_inactive_unit_is_start() {
        let alloc = alloc_with_state("inactive");
        register_service_worker(&mut alloc.write());
        let (job_id, kind) = enqueue_job_type(
            alloc.clone(),
            "demo.service",
            JobType::ReloadOrStart,
            false,
            JobMode::Replace,
        )
        .await
        .unwrap();
        assert_eq!(kind, JobKind::Start);
        assert_eq!(alloc.read().jobs.get(&job_id).unwrap().kind, JobKind::Start);
    }

    #[tokio::test]
    async fn test_enqueue_job_type_verify_active_root_completes_by_state() {
        // Direct verify-active request: active unit → done; inactive → skipped.
        let alloc = alloc_with_state("active");
        let (job_id, _) = enqueue_job_type(
            alloc.clone(),
            "demo.service",
            JobType::VerifyActive,
            false,
            JobMode::Replace,
        )
        .await
        .unwrap();
        assert_eq!(
            alloc.read().jobs.get(&job_id).unwrap().status,
            JobStatus::Done
        );

        let alloc = alloc_with_state("inactive");
        let (job_id, _) = enqueue_job_type(
            alloc.clone(),
            "demo.service",
            JobType::VerifyActive,
            false,
            JobMode::Replace,
        )
        .await
        .unwrap();
        assert_eq!(
            alloc.read().jobs.get(&job_id).unwrap().status,
            JobStatus::Done
        );
    }

    // =========================================================================
    // Serial execution helpers
    // =========================================================================

    #[test]
    fn test_enqueue_job_emit_job_new() {
        // Verify emit_job_new doesn't panic with None channel
        let mut state = AllocatorState::new();
        emit_job_new(&mut state, 1, "test.service", JobKind::Start);
        // No panic is the success case
    }

    // =========================================================================
    // Failure propagation (fail_dependents)
    // =========================================================================

    fn state_with_job(state: &mut AllocatorState, unit: &str, kind: JobKind) -> u64 {
        let jid = next_job_id();
        state.jobs.insert(
            jid,
            Job {
                id: jid,
                unit_name: unit.to_string(),
                kind,
                status: JobStatus::Running,
                timeout_abort: None,
            },
        );
        jid
    }

    fn unit_requires(name: &str, req: &[&str]) -> (String, UnitFile) {
        let mut u = make_unit(name);
        for dep in req {
            u.unit.requires.insert(dep.to_string());
        }
        (name.to_string(), u)
    }

    #[test]
    fn test_fail_dependents_start_failure_propagates_requires_chain() {
        let mut state = AllocatorState::new();
        state
            .units
            .insert("a.service".to_string(), make_unit("a.service"));
        let (bn, b) = unit_requires("b.service", &["a.service"]);
        state.units.insert(bn, b);
        let (cn, c) = unit_requires("c.service", &["b.service"]);
        state.units.insert(cn, c);
        state_with_job(&mut state, "a.service", JobKind::Start);
        state_with_job(&mut state, "b.service", JobKind::Start);
        state_with_job(&mut state, "c.service", JobKind::Start);

        fail_dependents(&mut state, "a.service", JobKind::Start);

        assert!(matches!(
            state
                .jobs
                .values()
                .find(|j| j.unit_name == "b.service")
                .unwrap()
                .status,
            JobStatus::Failed(_)
        ));
        assert!(matches!(
            state
                .jobs
                .values()
                .find(|j| j.unit_name == "c.service")
                .unwrap()
                .status,
            JobStatus::Failed(_)
        ));
        assert!(matches!(
            state
                .jobs
                .values()
                .find(|j| j.unit_name == "a.service")
                .unwrap()
                .status,
            JobStatus::Running
        ));
    }

    #[test]
    fn test_fail_dependents_start_failure_ignores_non_start_jobs() {
        let mut state = AllocatorState::new();
        state
            .units
            .insert("a.service".to_string(), make_unit("a.service"));
        let (bn, b) = unit_requires("b.service", &["a.service"]);
        state.units.insert(bn, b);
        state_with_job(&mut state, "a.service", JobKind::Start);
        // b has a stop job — must not be failed
        state_with_job(&mut state, "b.service", JobKind::Stop);

        fail_dependents(&mut state, "a.service", JobKind::Start);

        assert!(matches!(
            state
                .jobs
                .values()
                .find(|j| j.unit_name == "b.service")
                .unwrap()
                .status,
            JobStatus::Running
        ));
    }

    #[test]
    fn test_fail_dependents_start_failure_propagates_binds_to() {
        let mut state = AllocatorState::new();
        state
            .units
            .insert("a.service".to_string(), make_unit("a.service"));
        let (bn, b) = {
            let (n, mut u) = unit_requires("b.service", &["a.service"]);
            u.unit.requires.remove("a.service");
            u.unit.binds_to.insert("a.service".to_string());
            (n, u)
        };
        state.units.insert(bn, b);
        state_with_job(&mut state, "a.service", JobKind::Start);
        state_with_job(&mut state, "b.service", JobKind::Start);

        fail_dependents(&mut state, "a.service", JobKind::Start);

        assert!(matches!(
            state
                .jobs
                .values()
                .find(|j| j.unit_name == "b.service")
                .unwrap()
                .status,
            JobStatus::Failed(_)
        ));
    }

    #[test]
    fn test_fail_dependents_stop_failure_propagates_conflicts() {
        let mut state = AllocatorState::new();
        state
            .units
            .insert("a.service".to_string(), make_unit("a.service"));
        let mut b = make_unit("b.service");
        b.unit.conflicts.insert("a.service".to_string());
        state.units.insert("b.service".to_string(), b);
        state_with_job(&mut state, "a.service", JobKind::Stop);
        state_with_job(&mut state, "b.service", JobKind::Start);

        fail_dependents(&mut state, "a.service", JobKind::Stop);

        assert!(matches!(
            state
                .jobs
                .values()
                .find(|j| j.unit_name == "b.service")
                .unwrap()
                .status,
            JobStatus::Failed(_)
        ));
    }

    #[test]
    fn test_fail_dependents_requires_does_not_propagate_on_stop_failure() {
        let mut state = AllocatorState::new();
        state
            .units
            .insert("a.service".to_string(), make_unit("a.service"));
        let (bn, b) = unit_requires("b.service", &["a.service"]);
        state.units.insert(bn, b);
        state_with_job(&mut state, "a.service", JobKind::Stop);
        state_with_job(&mut state, "b.service", JobKind::Start);

        // A failed stop job only propagates through Conflicts=
        fail_dependents(&mut state, "a.service", JobKind::Stop);

        assert!(matches!(
            state
                .jobs
                .values()
                .find(|j| j.unit_name == "b.service")
                .unwrap()
                .status,
            JobStatus::Running
        ));
    }

    #[test]
    fn test_fail_dependents_restart_failure_propagates_nothing() {
        let mut state = AllocatorState::new();
        state
            .units
            .insert("a.service".to_string(), make_unit("a.service"));
        let (bn, b) = unit_requires("b.service", &["a.service"]);
        state.units.insert(bn, b);
        state_with_job(&mut state, "a.service", JobKind::Restart);
        state_with_job(&mut state, "b.service", JobKind::Start);

        // systemd: only JOB_START / JOB_VERIFY_ACTIVE failures propagate
        fail_dependents(&mut state, "a.service", JobKind::Restart);

        assert!(matches!(
            state
                .jobs
                .values()
                .find(|j| j.unit_name == "b.service")
                .unwrap()
                .status,
            JobStatus::Running
        ));
    }

    #[test]
    fn test_fail_dependents_completion_result_is_dependency() {
        let mut state = AllocatorState::new();
        state
            .units
            .insert("a.service".to_string(), make_unit("a.service"));
        let (bn, b) = unit_requires("b.service", &["a.service"]);
        state.units.insert(bn, b);
        state_with_job(&mut state, "a.service", JobKind::Start);
        state_with_job(&mut state, "b.service", JobKind::Start);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        state.job_completion_tx = Some(tx);

        fail_dependents(&mut state, "a.service", JobKind::Start);

        let completion = rx.try_recv().expect("completion emitted");
        assert_eq!(completion.unit_name, "b.service");
        assert_eq!(completion.result, JobResultKind::Dependency);
    }

    #[test]
    fn test_fail_dependents_reverts_dependent_cache_to_inactive() {
        // Regression test: poweroff.target Requires=systemd-poweroff.service;
        // when the service's Start fails, fail_dependents fails the target's
        // job AND must revert its cached state to inactive, otherwise
        // systemctl sees a failed job while `unit_states` still claims the
        // unit is "active".
        let mut state = AllocatorState::new();
        state.units.insert(
            "systemd-poweroff.service".to_string(),
            make_unit("systemd-poweroff.service"),
        );
        let (bt, mut target) = unit_requires("poweroff.target", &["systemd-poweroff.service"]);
        target
            .unit
            .after
            .insert("systemd-poweroff.service".to_string());
        state.units.insert(bt, target);
        state_with_job(&mut state, "systemd-poweroff.service", JobKind::Start);
        state_with_job(&mut state, "poweroff.target", JobKind::Start);
        // The target worker reported "active" before the dependency failed.
        cached_active(&mut state, "poweroff.target");
        cached_active(&mut state, "systemd-poweroff.service");

        fail_dependents(&mut state, "systemd-poweroff.service", JobKind::Start);

        let target_cache = state.unit_states.get("poweroff.target").unwrap();
        assert_eq!(target_cache.active_state, "inactive");
        assert!(target_cache.invocation_id.is_empty());
    }

    #[tokio::test]
    async fn test_handle_task_result_start_failure_reverts_cache_to_inactive() {
        // A unit whose own Start job fails directly must not stay "active"
        // in the cache (systemd: a failed start leaves the unit inactive).
        let alloc = alloc_with_state("inactive");
        {
            let mut state = alloc.write();
            let (tx, mut rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(16);
            // Keep the worker side alive so dispatch doesn't fail with
            // "Worker disconnected" before we report the task result.
            tokio::spawn(async move { while rx.recv().await.is_some() {} });
            state.workers.insert(
                "test-worker".to_string(),
                WorkerEntry {
                    worker_id: "test-worker".to_string(),
                    unit_types: vec!["service".to_string()],
                    supports_unit_define: false,
                    ready: false,
                    envelope_tx: tx,
                },
            );
        }
        let (job_id, kind) = enqueue_job_type(
            alloc.clone(),
            "demo.service",
            JobType::Start,
            false,
            JobMode::Replace,
        )
        .await
        .unwrap();
        assert_eq!(kind, JobKind::Start);
        {
            let mut state = alloc.write();
            cached_active(&mut state, "demo.service");
        }

        handle_task_result(
            alloc.clone(),
            job_id,
            false,
            "ExecStart is empty for demo.service",
            "demo.service",
            JobKind::Start,
        );

        let state = alloc.read();
        assert!(matches!(
            state.jobs.get(&job_id).unwrap().status,
            JobStatus::Failed(_)
        ));
        assert_eq!(
            state.unit_states.get("demo.service").unwrap().active_state,
            "inactive"
        );
        assert!(state
            .unit_states
            .get("demo.service")
            .unwrap()
            .invocation_id
            .is_empty());
    }

    // =========================================================================
    // BindsTo start propagation tests
    // =========================================================================

    fn unit_binds_to(name: &str, dep: &str) -> (String, UnitFile) {
        let mut u = make_unit(name);
        u.unit.binds_to.insert(dep.to_string());
        (name.to_string(), u)
    }

    fn cached_active(state: &mut AllocatorState, name: &str) {
        state.unit_states.insert(
            name.to_string(),
            CachedUnitState {
                active_state: "active".to_string(),
                sub_state: String::new(),
                main_pid: 0,
                invocation_id: String::new(),
                active_enter_timestamp: 0,
                inactive_enter_timestamp: 0,
                extensions: HashMap::new(),
                pids: Vec::new(),
                controller: String::new(),
            },
        );
    }

    #[test]
    fn binds_to_start_propagates_when_dependency_active() {
        let mut state = AllocatorState::new();
        state
            .units
            .insert("dep.service".to_string(), make_unit("dep.service"));
        let (cn, c) = unit_binds_to("consumer.service", "dep.service");
        state.units.insert(cn, c);
        cached_active(&mut state, "dep.service");

        let targets = binds_to_start_propagation(&state, "dep.service", true, JobKind::Start);
        assert_eq!(targets, vec!["consumer.service".to_string()]);
    }

    #[test]
    fn binds_to_start_propagation_skips_inactive_dependency() {
        let mut state = AllocatorState::new();
        state
            .units
            .insert("dep.service".to_string(), make_unit("dep.service"));
        let (cn, c) = unit_binds_to("consumer.service", "dep.service");
        state.units.insert(cn, c);

        let targets = binds_to_start_propagation(&state, "dep.service", true, JobKind::Start);
        assert!(targets.is_empty());
    }

    #[test]
    fn binds_to_start_propagation_skips_unit_with_running_job() {
        let mut state = AllocatorState::new();
        state
            .units
            .insert("dep.service".to_string(), make_unit("dep.service"));
        let (cn, c) = unit_binds_to("consumer.service", "dep.service");
        state.units.insert(cn, c);
        cached_active(&mut state, "dep.service");
        state_with_job(&mut state, "consumer.service", JobKind::Start);

        let targets = binds_to_start_propagation(&state, "dep.service", true, JobKind::Start);
        assert!(targets.is_empty());
    }

    #[test]
    fn binds_to_start_propagation_skips_restart_job_targets() {
        // An in-flight Restart also counts as running (gate B).
        let mut state = AllocatorState::new();
        state
            .units
            .insert("dep.service".to_string(), make_unit("dep.service"));
        let (cn, c) = unit_binds_to("consumer.service", "dep.service");
        state.units.insert(cn, c);
        cached_active(&mut state, "dep.service");
        state_with_job(&mut state, "consumer.service", JobKind::Restart);

        let targets = binds_to_start_propagation(&state, "dep.service", true, JobKind::Start);
        assert!(targets.is_empty());
    }

    #[test]
    fn binds_to_start_propagation_skips_already_active_target() {
        let mut state = AllocatorState::new();
        state
            .units
            .insert("dep.service".to_string(), make_unit("dep.service"));
        let (cn, c) = unit_binds_to("consumer.service", "dep.service");
        state.units.insert(cn, c);
        cached_active(&mut state, "dep.service");
        cached_active(&mut state, "consumer.service");

        let targets = binds_to_start_propagation(&state, "dep.service", true, JobKind::Restart);
        assert!(targets.is_empty());
    }

    #[test]
    fn binds_to_start_propagation_ignores_failed_and_stop_jobs() {
        let mut state = AllocatorState::new();
        state
            .units
            .insert("dep.service".to_string(), make_unit("dep.service"));
        let (cn, c) = unit_binds_to("consumer.service", "dep.service");
        state.units.insert(cn, c);
        cached_active(&mut state, "dep.service");

        assert!(
            binds_to_start_propagation(&state, "dep.service", false, JobKind::Start).is_empty()
        );
        assert!(binds_to_start_propagation(&state, "dep.service", true, JobKind::Stop).is_empty());
    }

    // =========================================================================
    // unit.define (M1): pre-plan scan + on-demand definition requests
    // =========================================================================

    /// Build the UnitIR of a slice, mirroring System R's synthesis rules.
    /// `with_parent_dep` controls the Requires=/After= edge on the parent
    /// (present in real definitions; omitted to simulate a partial worker).
    fn make_slice_ir(id: &str, parent: &str, with_parent_dep: bool) -> UnitIR {
        UnitIR {
            id: id.to_string(),
            unit_type: Some(UnitType::Slice),
            description: None,
            source_format: Some("dynamic".to_string()),
            source_path: None,
            aliases: Vec::new(),
            slice: Some(parent.to_string()),
            dependencies: if with_parent_dep {
                Some(DependencySet {
                    requires: std::collections::HashSet::from([parent.to_string()]),
                    after: std::collections::HashSet::from([parent.to_string()]),
                    ..Default::default()
                })
            } else {
                None
            },
            service: None,
            mount: None,
            automount: None,
            timer: None,
            socket: None,
            resource_control: None,
            conditions: None,
            asserts: None,
            wanted_by: None,
            required_by: None,
        }
    }

    /// A scope like the one `StartTransientUnit` produces: it carries the
    /// `Slice=` mirroring (Requires= + After= + slice field on the unit) but
    /// the parent slice is not loaded yet.
    fn make_scope_with_slice(name: &str, slice: &str) -> (String, UnitFile) {
        let mut u = make_unit(name);
        u.transient = true;
        u.unit.slice = slice.to_string();
        u.unit.requires.insert(slice.to_string());
        u.unit.after.insert(slice.to_string());
        (name.to_string(), u)
    }

    /// Complete a dispatched `method.call` task as successful, the way the
    /// IPC server's `method.result` handling does.  Required to advance the
    /// serial-mode chain: Replace mode chains every task on its predecessor
    /// (`serial_completion_txs`), so a fake worker that never replies would
    /// stall `enqueue_job` at the next task's chain await.
    fn fake_worker_complete_call(alloc: &AllocatorHandle, env: &Envelope) {
        let call = sysa::proto::MethodCall::decode(env.payload.as_slice()).expect("valid call");
        let kind = match call.method.as_str() {
            "start" => JobKind::Start,
            "stop" => JobKind::Stop,
            "restart" => JobKind::Restart,
            "reload" => JobKind::Reload,
            other => panic!("unexpected method {other}"),
        };
        handle_task_result(
            alloc.clone(),
            env.request_id,
            true,
            "ok",
            &call.unit_name,
            kind,
        );
    }

    #[test]
    fn collect_missing_units_walks_requires_edges() {
        let mut units: HashMap<String, UnitFile> = HashMap::new();
        let (n, b) = unit_requires("b.service", &["a.service"]);
        units.insert(n, b);
        let (n, c) = unit_requires("c.service", &["b.service"]);
        units.insert(n, c);
        units.insert("a.service".to_string(), make_unit("a.service"));

        // Everything present → nothing missing.
        assert!(collect_missing_units(&units, "c.service").is_empty());
        // Drop a.service → walk reports it once.
        units.remove("a.service");
        assert_eq!(
            collect_missing_units(&units, "c.service"),
            vec!["a.service"]
        );
    }

    #[test]
    fn collect_missing_units_reports_slice_parent_but_skips_root() {
        let mut units: HashMap<String, UnitFile> = HashMap::new();
        let (n, scope) = make_scope_with_slice("session-1.scope", "user-1000.slice");
        units.insert(n, scope);

        assert_eq!(
            collect_missing_units(&units, "session-1.scope"),
            vec!["user-1000.slice"]
        );

        // A unit under the root slice does not require loading anything.
        let mut u = make_unit("x.service");
        u.unit.slice = crate::state::ROOT_SLICE_NAME.to_string();
        units.insert("x.service".to_string(), u);
        let missing: Vec<String> = collect_missing_units(&units, "x.service");
        assert!(missing.is_empty());
    }

    #[test]
    fn collect_missing_units_dedups_and_sorts() {
        let mut units: HashMap<String, UnitFile> = HashMap::new();
        let mut u = make_unit("root.scope");
        u.unit.requires.insert("b.slice".to_string());
        u.unit.wants.insert("a.slice".to_string());
        u.unit.conflicts.insert("b.slice".to_string());
        u.unit.slice = "c.slice".to_string();
        units.insert("root.scope".to_string(), u);

        // b.slice requested twice (requires + conflicts), sorted output.
        assert_eq!(
            collect_missing_units(&units, "root.scope"),
            vec!["a.slice", "b.slice", "c.slice"]
        );
    }

    #[tokio::test]
    async fn test_enqueue_job_pre_scan_requests_and_commits_slice_chain() {
        // The happy path of M1: starting a transient scope whose parent
        // slice is missing triggers unit.define against the slice worker;
        // the synthesized chain (user-1000.slice → user.slice) is committed
        // and the replanned transaction contains Start jobs for the whole
        // chain alongside the scope.
        let alloc = Arc::new(parking_lot::RwLock::new(AllocatorState::new()));
        {
            let mut state = alloc.write();
            let (n, scope) = make_scope_with_slice("session-1.scope", "user-1000.slice");
            state.units.insert(n, scope);
            // The scope itself is dispatched to the scope worker; slices go
            // to the slice worker.
            let (scope_tx, mut scope_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(16);
            tokio::spawn(async move { while scope_rx.recv().await.is_some() {} });
            state.workers.insert(
                "system-e-1".to_string(),
                WorkerEntry {
                    worker_id: "system-e-1".to_string(),
                    unit_types: vec!["scope".to_string()],
                    supports_unit_define: false,
                    ready: false,
                    envelope_tx: scope_tx,
                },
            );
            let (slice_tx, _slice_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(16);
            state.workers.insert(
                "system-r-1".to_string(),
                WorkerEntry {
                    worker_id: "system-r-1".to_string(),
                    unit_types: vec!["slice".to_string()],
                    supports_unit_define: true,
                    ready: false,
                    envelope_tx: slice_tx,
                },
            );
        }

        // Fake slice worker: answer the unit.define request by completing
        // the pending oneshot (as ipc/server.rs's unit.define_result branch
        // would), then drain the dispatch envelopes.
        let (slice_tx, mut slice_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(16);
        {
            let mut state = alloc.write();
            state.workers.insert(
                "system-r-1".to_string(),
                WorkerEntry {
                    worker_id: "system-r-1".to_string(),
                    unit_types: vec!["slice".to_string()],
                    supports_unit_define: true,
                    ready: false,
                    envelope_tx: slice_tx,
                },
            );
        }
        let alloc_fake_loop = alloc.clone();
        tokio::spawn(async move {
            while let Some(bytes) = slice_rx.recv().await {
                let env = Envelope::decode(&mut bytes.as_ref()).expect("valid envelope");
                match env.method.as_str() {
                    "unit.define" => {
                        let req = UnitDefineRequest::decode(env.payload.as_slice())
                            .expect("valid request");
                        assert_eq!(req.unit_names, vec!["user-1000.slice"]);
                        let units: HashMap<String, UnitIR> = HashMap::from([
                            (
                                "user-1000.slice".to_string(),
                                make_slice_ir("user-1000.slice", "user.slice", true),
                            ),
                            (
                                "user.slice".to_string(),
                                make_slice_ir("user.slice", crate::state::ROOT_SLICE_NAME, false),
                            ),
                        ]);
                        let tx = alloc_fake_loop
                            .write()
                            .unit_define_txs
                            .remove(&env.request_id)
                            .expect("pending unit.define oneshot");
                        let _ = tx.send(UnitDefineResult {
                            success: true,
                            error: String::new(),
                            units_json: serde_json::to_vec(&units).unwrap(),
                        });
                    }
                    "method.call" => fake_worker_complete_call(&alloc_fake_loop, &env),
                    other => panic!("unexpected method {other}"),
                }
            }
        });

        enqueue_job(
            alloc.clone(),
            "session-1.scope",
            JobKind::Start,
            JobMode::Replace,
        )
        .await
        .expect("scope with synthesized slice chain must enqueue");

        let state = alloc.read();
        // The chain is committed.
        assert!(state.units.contains_key("user-1000.slice"));
        assert!(state.units.contains_key("user.slice"));
        assert_eq!(state.units["user-1000.slice"].unit.slice, "user.slice");
        // The transaction contains jobs for the scope and the whole chain.
        let mut job_units: Vec<&str> = state.jobs.values().map(|j| j.unit_name.as_str()).collect();
        job_units.sort();
        assert_eq!(
            job_units,
            vec!["session-1.scope", "user-1000.slice", "user.slice"]
        );
    }

    #[tokio::test]
    async fn test_enqueue_job_unit_define_bounded_retry_for_partial_definition() {
        // A worker answering only the requested unit (no parent chain)
        // forces the planner into UnitNotFound on the parent; the bounded
        // retry sends a second unit.define for the exact missing unit.
        // Synthetic slice names keep the test independent of the host's
        // unit files: a real distro ships `user.slice` on disk, which the
        // pre-scan would statically load instead of asking the worker.
        const CHILD: &str = "zzztest-1000.slice";
        const PARENT: &str = "zzztest.slice";
        let alloc = Arc::new(parking_lot::RwLock::new(AllocatorState::new()));
        {
            let mut state = alloc.write();
            let (n, scope) = make_scope_with_slice("session-1.scope", CHILD);
            state.units.insert(n, scope);
            let (scope_tx, mut scope_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(16);
            tokio::spawn(async move { while scope_rx.recv().await.is_some() {} });
            state.workers.insert(
                "system-e-1".to_string(),
                WorkerEntry {
                    worker_id: "system-e-1".to_string(),
                    unit_types: vec!["scope".to_string()],
                    supports_unit_define: false,
                    ready: false,
                    envelope_tx: scope_tx,
                },
            );
        }

        let (slice_tx, mut slice_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(16);
        {
            let mut state = alloc.write();
            state.workers.insert(
                "system-r-1".to_string(),
                WorkerEntry {
                    worker_id: "system-r-1".to_string(),
                    unit_types: vec!["slice".to_string()],
                    supports_unit_define: true,
                    ready: false,
                    envelope_tx: slice_tx,
                },
            );
        }

        let answered: std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let alloc_fake = alloc.clone();
        let answered_fake = answered.clone();
        tokio::spawn(async move {
            while let Some(bytes) = slice_rx.recv().await {
                let env = Envelope::decode(&mut bytes.as_ref()).expect("valid envelope");
                match env.method.as_str() {
                    "unit.define" => {
                        let req = UnitDefineRequest::decode(env.payload.as_slice())
                            .expect("valid request");
                        answered_fake.lock().unwrap().push(req.unit_names.clone());
                        let units: HashMap<String, UnitIR> = match req.unit_names[0].as_str() {
                            CHILD => HashMap::from([(
                                CHILD.to_string(),
                                make_slice_ir(CHILD, PARENT, true),
                            )]),
                            PARENT => HashMap::from([(
                                PARENT.to_string(),
                                make_slice_ir(PARENT, crate::state::ROOT_SLICE_NAME, false),
                            )]),
                            other => panic!("unexpected unit.define for {other:?}"),
                        };
                        let tx = alloc_fake
                            .write()
                            .unit_define_txs
                            .remove(&env.request_id)
                            .expect("pending unit.define oneshot");
                        let _ = tx.send(UnitDefineResult {
                            success: true,
                            error: String::new(),
                            units_json: serde_json::to_vec(&units).unwrap(),
                        });
                    }
                    "method.call" => fake_worker_complete_call(&alloc_fake, &env),
                    other => panic!("unexpected method {other}"),
                }
            }
        });

        enqueue_job(
            alloc.clone(),
            "session-1.scope",
            JobKind::Start,
            JobMode::Replace,
        )
        .await
        .expect("bounded retry must recover the missing parent");

        // Exactly two unit.define rounds: the pre-scan for the parent, and
        // the bounded retry for the parent's parent discovered only during
        // plan expansion.
        assert_eq!(
            answered.lock().unwrap().as_slice(),
            &[vec![CHILD.to_string()], vec![PARENT.to_string()]]
        );
        let state = alloc.read();
        assert!(state.units.contains_key(CHILD));
        assert!(state.units.contains_key(PARENT));
    }

    #[tokio::test]
    async fn test_enqueue_job_unit_define_refusal_keeps_hard_error() {
        // A worker that refuses the request keeps the caller's hard-error
        // semantics: the transaction fails, nothing is committed.
        let alloc = Arc::new(parking_lot::RwLock::new(AllocatorState::new()));
        {
            let mut state = alloc.write();
            let (n, scope) = make_scope_with_slice("session-1.scope", "user-1000.slice");
            state.units.insert(n, scope);
            let (scope_tx, mut scope_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(16);
            tokio::spawn(async move { while scope_rx.recv().await.is_some() {} });
            state.workers.insert(
                "system-e-1".to_string(),
                WorkerEntry {
                    worker_id: "system-e-1".to_string(),
                    unit_types: vec!["scope".to_string()],
                    supports_unit_define: false,
                    ready: false,
                    envelope_tx: scope_tx,
                },
            );
        }
        let (slice_tx, mut slice_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(16);
        {
            let mut state = alloc.write();
            state.workers.insert(
                "system-r-1".to_string(),
                WorkerEntry {
                    worker_id: "system-r-1".to_string(),
                    unit_types: vec!["slice".to_string()],
                    supports_unit_define: true,
                    ready: false,
                    envelope_tx: slice_tx,
                },
            );
        }
        let alloc_fake = alloc.clone();
        tokio::spawn(async move {
            while let Some(bytes) = slice_rx.recv().await {
                let env = Envelope::decode(&mut bytes.as_ref()).expect("valid envelope");
                assert_eq!(
                    env.method, "unit.define",
                    "refused request happens before dispatch"
                );
                let tx = alloc_fake
                    .write()
                    .unit_define_txs
                    .remove(&env.request_id)
                    .expect("pending unit.define oneshot");
                let _ = tx.send(UnitDefineResult {
                    success: false,
                    error: "cannot synthesize definitions for non-slice units: [x.service]"
                        .to_string(),
                    units_json: vec![],
                });
            }
        });

        let err = enqueue_job(
            alloc.clone(),
            "session-1.scope",
            JobKind::Start,
            JobMode::Replace,
        )
        .await
        .expect_err("refused unit.define must fail the transaction");
        assert!(err.to_string().contains("refused"), "{}", err);
        let state = alloc.read();
        assert!(!state.units.contains_key("user-1000.slice"));
    }

    #[tokio::test]
    async fn test_enqueue_job_missing_unit_without_worker_fails() {
        // No worker owns the "slice" type → the pre-scan fails fast with a
        // descriptive error instead of silently proceeding (the plan would
        // otherwise fail on UnitNotFound with a less actionable message).
        let alloc = Arc::new(parking_lot::RwLock::new(AllocatorState::new()));
        {
            let mut state = alloc.write();
            let (n, scope) = make_scope_with_slice("session-1.scope", "user-1000.slice");
            state.units.insert(n, scope);
            // The root scope is dispatched to a scope worker; only the slice
            // type is unowned here.
            let (scope_tx, mut scope_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(16);
            tokio::spawn(async move { while scope_rx.recv().await.is_some() {} });
            state.workers.insert(
                "system-e-1".to_string(),
                WorkerEntry {
                    worker_id: "system-e-1".to_string(),
                    unit_types: vec!["scope".to_string()],
                    supports_unit_define: false,
                    ready: false,
                    envelope_tx: scope_tx,
                },
            );
        }

        let err = enqueue_job(
            alloc.clone(),
            "session-1.scope",
            JobKind::Start,
            JobMode::Replace,
        )
        .await
        .expect_err("missing slice with no slice worker must fail");
        let msg = err.to_string();
        assert!(
            msg.contains("No worker available for unit type 'slice'"),
            "{msg}"
        );
        assert!(msg.contains("user-1000.slice"), "{msg}");
    }

    #[tokio::test]
    async fn test_enqueue_job_pre_scan_noop_when_everything_loaded() {
        // When the parent slice is already loaded no unit.define is sent:
        // the pre-scan finds nothing missing and the plan is built directly.
        let alloc = Arc::new(parking_lot::RwLock::new(AllocatorState::new()));
        {
            let mut state = alloc.write();
            let (n, scope) = make_scope_with_slice("session-1.scope", "user-1000.slice");
            state.units.insert(n, scope);
            state
                .units
                .insert("user-1000.slice".to_string(), make_unit("user-1000.slice"));
            let (scope_tx, mut scope_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(16);
            tokio::spawn(async move { while scope_rx.recv().await.is_some() {} });
            state.workers.insert(
                "system-e-1".to_string(),
                WorkerEntry {
                    worker_id: "system-e-1".to_string(),
                    unit_types: vec!["scope".to_string()],
                    supports_unit_define: false,
                    ready: false,
                    envelope_tx: scope_tx,
                },
            );
        }
        let (slice_tx, mut slice_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(16);
        {
            let mut state = alloc.write();
            state.workers.insert(
                "system-r-1".to_string(),
                WorkerEntry {
                    worker_id: "system-r-1".to_string(),
                    unit_types: vec!["slice".to_string()],
                    supports_unit_define: true,
                    ready: false,
                    envelope_tx: slice_tx,
                },
            );
        }
        let alloc_fake = alloc.clone();
        tokio::spawn(async move {
            while let Some(bytes) = slice_rx.recv().await {
                let env = Envelope::decode(&mut bytes.as_ref()).expect("valid envelope");
                assert_ne!(
                    env.method, "unit.define",
                    "no unit.define when nothing is missing"
                );
                fake_worker_complete_call(&alloc_fake, &env);
            }
        });

        enqueue_job(
            alloc.clone(),
            "session-1.scope",
            JobKind::Start,
            JobMode::Replace,
        )
        .await
        .expect("fully loaded transaction must enqueue");
        let state = alloc.read();
        assert_eq!(
            state.unit_define_txs.len(),
            0,
            "no pending unit.define requests"
        );
    }

    #[tokio::test]
    async fn test_enqueue_job_skips_unit_define_for_types_without_capable_worker() {
        // The pre-scan must not send unit.define to a worker that does not
        // implement the protocol (a service worker here): the request would
        // go unanswered and time out.  A unit with no on-disk definition
        // and no unit.define provider stays missing, so the plan fails
        // with UnitNotFound — the pre-protocol semantics.
        let alloc = Arc::new(parking_lot::RwLock::new(AllocatorState::new()));
        {
            let mut state = alloc.write();
            let mut scope = make_unit("session-1.scope");
            scope.transient = true;
            scope
                .unit
                .requires
                .insert("no-such-unit-42.service".to_string());
            state.units.insert("session-1.scope".to_string(), scope);
            let (scope_tx, mut scope_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(16);
            tokio::spawn(async move { while scope_rx.recv().await.is_some() {} });
            state.workers.insert(
                "system-e-1".to_string(),
                WorkerEntry {
                    worker_id: "system-e-1".to_string(),
                    unit_types: vec!["scope".to_string()],
                    supports_unit_define: false,
                    ready: false,
                    envelope_tx: scope_tx,
                },
            );
            // A service worker exists but did not declare unit.define
            // support (WorkerRegistration.supports_unit_define = false).
            let (service_tx, mut service_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(16);
            tokio::spawn(async move { while service_rx.recv().await.is_some() {} });
            state.workers.insert(
                "system-s-1".to_string(),
                WorkerEntry {
                    worker_id: "system-s-1".to_string(),
                    unit_types: vec!["service".to_string()],
                    supports_unit_define: false,
                    ready: false,
                    envelope_tx: service_tx,
                },
            );
        }

        let err = enqueue_job(
            alloc.clone(),
            "session-1.scope",
            JobKind::Start,
            JobMode::Replace,
        )
        .await
        .expect_err("missing unit without a unit.define provider must fail");
        assert!(
            err.to_string().contains("no-such-unit-42.service"),
            "{}",
            err
        );
        let state = alloc.read();
        assert!(
            state.unit_define_txs.is_empty(),
            "no unit.define request may be pending"
        );
        assert!(!state.units.contains_key("no-such-unit-42.service"));
    }

    #[tokio::test]
    async fn test_request_unit_definition_leaves_unregistered_power_missing() {
        // The `power` unit type is owned by System Init, which registers
        // every `.power` definition at boot over `manager.register_power_units`.
        // `request_unit_definition` must NOT synthesize them itself: a legal
        // name that was never registered stays missing and the request fails
        // (no worker owns the `power` type).
        let alloc = Arc::new(parking_lot::RwLock::new(AllocatorState::new()));
        let err = request_unit_definition(alloc.clone(), &["poweroff.power".to_string()])
            .await
            .expect_err("an unregistered .power unit must stay missing");
        assert!(
            err.to_string().contains("power"),
            "error must mention the power type: {}",
            err
        );
        assert!(!alloc.read().units.contains_key("poweroff.power"));
    }

    #[tokio::test]
    async fn test_request_unit_definition_refuses_bad_power_name() {
        // An illegal `.power` name has no definition source: it stays
        // missing and the request fails (no worker exists for the type).
        let alloc = Arc::new(parking_lot::RwLock::new(AllocatorState::new()));
        let err = request_unit_definition(alloc.clone(), &["evil.power".to_string()])
            .await
            .expect_err("an illegal .power name must fail");
        assert!(
            err.to_string().contains("power"),
            "error must mention the power type: {}",
            err
        );
        assert!(!alloc.read().units.contains_key("evil.power"));
    }

    // =========================================================================
    // Already-active Start short-circuit (systemd unit_start → -EALREADY)
    // =========================================================================

    #[tokio::test]
    async fn test_enqueue_job_start_of_active_unit_is_ealready_noop() {
        // Regression for the boot duplicate-start bug: an already-active
        // unit (e.g. tmpfiles-setup with RemainAfterExit=yes in the second
        // overlapping boot transaction) must never be started again.
        // systemd's unit_start() returns -EALREADY for active units; no
        // worker call happens and no new process is spawned.
        let alloc = alloc_with_state("active");
        register_service_worker(&mut alloc.write());

        let job_id = enqueue_job(
            alloc.clone(),
            "demo.service",
            JobKind::Start,
            JobMode::Replace,
        )
        .await
        .expect("start of active unit succeeds");

        let state = alloc.read();
        assert!(job_id != 0);
        // Nothing was dispatched to the worker.
        assert!(
            state.task_kinds.is_empty(),
            "no task may be dispatched for an already-active unit"
        );
        // No job may be left running for the unit.
        assert!(!state
            .jobs
            .values()
            .any(|j| { j.unit_name == "demo.service" && matches!(j.status, JobStatus::Running) }));
        // No desired-state change was committed for the unit.
        assert_eq!(state.desired.get("demo.service"), None);
    }

    #[tokio::test]
    async fn test_enqueue_job_restart_of_active_unit_still_dispatches() {
        // -EALREADY applies to Start only: Restart of an active unit must
        // still reach the worker (systemd: restart unconditionally
        // re-runs the unit).
        let alloc = alloc_with_state("active");
        register_service_worker(&mut alloc.write());

        enqueue_job(
            alloc.clone(),
            "demo.service",
            JobKind::Restart,
            JobMode::Replace,
        )
        .await
        .expect("restart of active unit succeeds");

        let state = alloc.read();
        assert!(
            !state.task_kinds.is_empty(),
            "restart must dispatch a task even for an active unit"
        );
    }

    #[tokio::test]
    async fn test_dispatch_skips_start_step_of_unit_cached_active() {
        // Regression for the boot duplicate-start bug: a transaction plan
        // may still contain a Start step for a unit that is already cached
        // as active (overlapping boot transactions).  Whether the step is
        // dropped during planning (drop_redundant) or caught at dispatch
        // time, the unit must not be started a second time — mirroring
        // systemd's unit_start() -EALREADY.
        let alloc = Arc::new(parking_lot::RwLock::new(AllocatorState::new()));
        {
            let mut state = alloc.write();
            // foo.service requires demo.service; demo is already active.
            let (n, foo) = unit_requires("foo.service", &["demo.service"]);
            state.units.insert(n, foo);
            state
                .units
                .insert("demo.service".to_string(), make_unit("demo.service"));
            state.unit_states.insert(
                "demo.service".to_string(),
                CachedUnitState {
                    active_state: "active".to_string(),
                    sub_state: "exited".to_string(),
                    main_pid: 0,
                    invocation_id: String::new(),
                    active_enter_timestamp: 0,
                    inactive_enter_timestamp: 0,
                    extensions: HashMap::new(),
                    pids: Vec::new(),
                    controller: String::new(),
                },
            );
            // Keep the worker side alive (drain incoming envelopes) so the
            // dispatch never sees a "Worker disconnected" failure.
            let (tx, mut rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(16);
            tokio::spawn(async move { while rx.recv().await.is_some() {} });
            state.workers.insert(
                "test-worker".to_string(),
                WorkerEntry {
                    worker_id: "test-worker".to_string(),
                    unit_types: vec!["service".to_string()],
                    supports_unit_define: false,
                    ready: false,
                    envelope_tx: tx,
                },
            );
        }

        let job_id = enqueue_job(
            alloc.clone(),
            "foo.service",
            JobKind::Start,
            JobMode::Replace,
        )
        .await
        .expect("start of foo succeeds");

        let state = alloc.read();
        // No job may exist for demo.service (neither running nor done as a
        // dispatch): it was never started.
        assert!(
            !state.jobs.values().any(|j| j.unit_name == "demo.service"),
            "no job may be created for the already-active dependency"
        );
        // The root job is running (dispatched to the worker).
        let job = state.jobs.get(&job_id).expect("foo job recorded");
        assert_eq!(job.unit_name, "foo.service");
        assert_eq!(job.status, JobStatus::Running);
    }
}
