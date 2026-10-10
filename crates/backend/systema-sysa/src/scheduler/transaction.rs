//! systemd-compatible transaction planner.
//!
//! A faithful Rust port of the transaction machinery in systemd's
//! `src/core/transaction.c`, plus the job-ordering logic of
//! `src/core/job.c` (`job_compare`, `job_is_runnable`).
//!
//! Given a requested root operation and a snapshot of loaded units, runtime
//! states and installed jobs, [`build_plan`] runs the same pipeline as
//! systemd's `transaction_activate()`:
//!
//! 1. **Build** (`add_job_and_dependencies`) — recursively pull in
//!    dependency jobs through the systemd dependency atoms, recording
//!    per-edge `matters`/`conflicts` flags.
//! 2. **Matters-to-anchor** (`transaction_find_jobs_that_matter_to_anchor`)
//!    — mark every job reachable from an anchor through mattering edges.
//! 3. **Minimize impact** (`transaction_minimize_impact`) — in `Fail` /
//!    `Lenient` modes, drop (or refuse) jobs that would stop a running
//!    unit or change an existing job.
//! 4. **Drop redundant** (`transaction_drop_redundant`) — remove jobs that
//!    are no-ops given the runtime state (anchors and job-changing jobs are
//!    kept).
//! 5. **Verify order** (`transaction_verify_order`) — DFS over the ordering
//!    graph (gated by the systemd `job_compare` priority rules), breaking
//!    cycles by deleting non-mattering jobs.
//! 6. **Merge** (`transaction_merge_jobs`) — per-unit job-type merging via
//!    `job_type_merge_and_collapse`, deleting unmergeable jobs.
//! 7. **Is destructive** (`transaction_is_destructive`) — refuse
//!    transactions that contradict existing jobs in `Fail`/`Lenient` modes.
//! 8. **Finalize** — produce a deterministic serial execution order that
//!    respects the `job_is_runnable()` wait relation (After=/Before=
//!    normalized edges gated by stop/restart priority).
//!
//! ## Deviations from systemd
//!
//! - systema stores no inverse dependency edges at load time; the planner
//!   builds a [`ReverseIndex`] over the unit map instead.
//! - `EnqueueUnitJobMany()`-style multi-anchor transactions: supported via
//!   [`build_plan_multi`].  Each root in the batch becomes an anchor job
//!   inside a single [`Transaction`], matching systemd's
//!   `manager_add_jobs()` semantics.
//! - Units are preloaded; a missing dependency unit is a hard error
//!   (`UnitNotFound`) instead of systemd's on-demand load.

use std::collections::{HashMap, HashSet, VecDeque};

use tracing::{debug, warn};

use crate::state::JobMode;
use crate::unit::types::UnitFile;

use super::job_type::{
    job_type_collapse, job_type_is_redundant, job_type_lookup_merge, job_type_merge_and_collapse,
    JobType, UnitActiveState,
};

// ---------------------------------------------------------------------------
// Transaction flags
// ---------------------------------------------------------------------------

/// `TransactionAddFlags` from systemd's `transaction.h`.
pub mod transaction_flags {
    pub const MATTERS: u32 = 1 << 0;
    pub const IGNORE_REQUIREMENTS: u32 = 1 << 1;
    pub const IGNORE_ORDER: u32 = 1 << 2;
    pub const CONFLICTS: u32 = 1 << 3;
    /// Restart dependencies instead of just starting them (`JOB_RESTART_DEPENDENCIES`).
    pub const PROPAGATE_START_AS_RESTART: u32 = 1 << 4;
}

use transaction_flags::*;

// ---------------------------------------------------------------------------
// Planner mode
// ---------------------------------------------------------------------------

/// The scheduling semantics of a transaction, mapped from the public
/// [`JobMode`] (systemd's `JobMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlannerMode {
    /// Replace existing jobs; destructive transactions are allowed.
    Replace,
    /// Refuse to stop running units or change existing jobs.
    Fail,
    /// Like `Fail` but the root job itself is also rejected when it would
    /// stop a running unit or change an existing job (systemd `JOB_LENIENT`).
    Lenient,
    /// Start the root and stop every other active unit.
    Isolate,
    /// Like `Replace`; the caller cancels all pending jobs first.
    Flush,
    /// Skip requirement pull-ins, keep ordering.
    IgnoreDependencies,
    /// Skip requirement pull-ins and ordering.
    IgnoreRequirements,
    /// Like `Replace`; systemd marks the resulting jobs irreversible so
    /// later transactions cannot replace them. System A does not model
    /// job irreversibility, so the semantics coincide with `Replace`.
    ReplaceIrreversibly,
    /// `JOB_TRIGGERING`: adds `TRIGGERED_BY` dependencies to the
    /// transaction. System A does not model trigger units, so the planner
    /// treats this like `Replace`; only valid for stop jobs (validated by
    /// the scheduler).
    Triggering,
    /// `JOB_RESTART_DEPENDENCIES`: a start job for the root becomes a
    /// restart for every unit that pulls it in. Only valid for start jobs
    /// (validated by the scheduler).
    RestartDependencies,
}

impl PlannerMode {
    pub fn from_job_mode(mode: JobMode) -> PlannerMode {
        match mode {
            JobMode::Replace | JobMode::Queue => PlannerMode::Replace,
            JobMode::Fail => PlannerMode::Fail,
            JobMode::Lenient => PlannerMode::Lenient,
            JobMode::Isolate => PlannerMode::Isolate,
            JobMode::Flush => PlannerMode::Flush,
            JobMode::IgnoreDependencies => PlannerMode::IgnoreDependencies,
            JobMode::IgnoreRequirements => PlannerMode::IgnoreRequirements,
            JobMode::ReplaceIrreversibly => PlannerMode::ReplaceIrreversibly,
            JobMode::Triggering => PlannerMode::Triggering,
            JobMode::RestartDependencies => PlannerMode::RestartDependencies,
        }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Transaction failure reasons, mirroring systemd's bus error conditions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// `BUS_ERROR_TRANSACTION_ORDER_IS_CYCLIC`: ordering graph contains an
    /// unfixable cycle.
    Cyclic,
    /// `BUS_ERROR_TRANSACTION_JOBS_CONFLICTING`: conflicting jobs for the
    /// same unit that could not be resolved.
    Conflicting,
    /// `BUS_ERROR_TRANSACTION_IS_DESTRUCTIVE`: transaction would stop a
    /// running unit or change an existing job.
    Destructive,
    /// A unit the transaction depends on is not loaded.
    UnitNotFound(String),
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanError::Cyclic => write!(f, "{}", sysa::l10n::t_("Transaction order is cyclic")),
            PlanError::Conflicting => {
                write!(
                    f,
                    "{}",
                    sysa::l10n::t_("Transaction contains conflicting jobs")
                )
            }
            PlanError::Destructive => write!(f, "{}", sysa::l10n::t_("Transaction is destructive")),
            PlanError::UnitNotFound(u) => write!(
                f,
                "{}",
                sysa::l10n::fmt(
                    sysa::l10n::t_("Unit {u} is not loaded"),
                    &[("u", &u.to_string())]
                )
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Plan output
// ---------------------------------------------------------------------------

/// One unit operation to execute, in final execution order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanStep {
    pub unit: String,
    pub job_type: JobType,
    /// Directly requested root of the transaction.
    pub anchor: bool,
    /// Reachable from an anchor through mattering dependency edges.
    pub matters_to_anchor: bool,
    /// Ordering constraints are ignored for this job.
    pub ignore_order: bool,
}

/// The result of a transaction: the ordered set of unit operations.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransactionPlan {
    pub steps: Vec<PlanStep>,
}

// ---------------------------------------------------------------------------
// Reverse dependency index
// ---------------------------------------------------------------------------

/// Inverse-dependency lookup over the loaded unit set.
///
/// systemd stores inverse edges at load time (`unit_add_dependency_impl`),
/// so `u->dependencies[UNIT_ATOM_REQUIRED_BY]` yields every unit that
/// `Requires=u`. systema stores only the forward declarations, so the
/// planner builds this index once per transaction.
#[derive(Debug, Default)]
struct ReverseIndex {
    /// Units that declare `After= u`.
    after: HashMap<String, Vec<String>>,
    /// Units that declare `Before= u`.
    before: HashMap<String, Vec<String>>,
    /// Units that declare `Requires= u`.
    requires: HashMap<String, Vec<String>>,
    /// Units that declare `Requisite= u`.
    requisite: HashMap<String, Vec<String>>,
    /// Units that declare `PartOf= u`.
    part_of: HashMap<String, Vec<String>>,
    /// Units that declare `Conflicts= u`.
    ///
    /// systemd stores this as the inverse `UNIT_ATOM_CONFLICTED_BY` edge:
    /// when `u` is started, every unit that declared `Conflicts= u` gets a
    /// Stop job.  The per-unit `Conflicts=` injection of `shutdown.target`
    /// (`add_type_default_dependencies`) is what shuts the whole world down
    /// when `shutdown.target` starts.
    conflicted_by: HashMap<String, Vec<String>>,
}

impl ReverseIndex {
    fn build(units: &HashMap<String, UnitFile>) -> Self {
        let mut r = Self::default();
        for (name, uf) in units {
            for a in &uf.unit.after {
                r.after.entry(a.clone()).or_default().push(name.clone());
            }
            for b in &uf.unit.before {
                r.before.entry(b.clone()).or_default().push(name.clone());
            }
            for q in &uf.unit.requires {
                r.requires.entry(q.clone()).or_default().push(name.clone());
            }
            for q in &uf.unit.requisite {
                r.requisite.entry(q.clone()).or_default().push(name.clone());
            }
            for p in &uf.unit.part_of {
                r.part_of.entry(p.clone()).or_default().push(name.clone());
            }
            for c in &uf.unit.conflicts {
                r.conflicted_by
                    .entry(c.clone())
                    .or_default()
                    .push(name.clone());
            }
        }
        r
    }

    /// The normalized `UNIT_ATOM_AFTER` dependency set of `u`: declared
    /// `After=` targets plus the units that declare `Before= u`.
    ///
    /// Socket units pulled in through `Requires=`/`BindsTo=` or `Sockets=`
    /// are treated as `After=` targets: System S requests the listener fds of
    /// `socket_units` at spawn time, so the socket must be bound before the
    /// dependent unit starts (systemd relies on sockets.target having
    /// already activated them; System A has no such early activation, so
    /// the ordering edge is made explicit here).
    fn after_deps(&self, units: &HashMap<String, UnitFile>, u: &str) -> Vec<String> {
        let mut out: HashSet<String> = units
            .get(u)
            .map(|uf| uf.unit.after.iter().cloned().collect())
            .unwrap_or_default();
        if let Some(v) = self.before.get(u) {
            out.extend(v.iter().cloned());
        }
        if let Some(uf) = units.get(u) {
            for dep in uf.unit.requires.iter().chain(uf.unit.binds_to.iter()) {
                if dep.ends_with(".socket") {
                    out.insert(dep.clone());
                }
            }
            // Socket units from Sockets= (socket activation) also require
            // ordering: the socket must be bound before the service starts.
            if let Some(svc) = &uf.service {
                for dep in &svc.sockets {
                    out.insert(dep.clone());
                }
            }
        }
        // A unit that is ordered after a `.socket` must also run after the
        // socket's activated service is *started*.  Real systemd guarantees
        // this because socket activation wakes the service as soon as its
        // first event arrives (e.g. an early kernel uevent activates udevd
        // before `udevadm trigger` cold-plugs the rest).  System A has no
        // such early activation: the only thing that starts the service is
        // an explicit Start job, which can otherwise race the dependent unit
        // and lose events (coldplug MODALIAS -> kmod load never fires because
        // udevd is not yet bound to the kernel netlink socket).  Resolve each
        // `.socket` ordering target to its `[Socket] Service=` unit (or the
        // "foo.socket" -> "foo.service" convention) and order after it too.
        for dep in out.clone().iter() {
            if !dep.ends_with(".socket") {
                continue;
            }
            let svc: Option<String> = units
                .get(dep)
                .and_then(|du| du.socket.as_ref())
                .map(|s| s.service.clone())
                .filter(|s| !s.is_empty())
                .or_else(|| Some(dep.replace(".socket", ".service")));
            if let Some(svc) = svc {
                // Never order a unit after its own socket's service when that
                // resolves back to the unit itself ("foo.socket" ->
                // "foo.service" for a service that owns the socket).  A unit
                // cannot appear in its own `after_deps`; a self-edge turns
                // every mattering Stop job on the unit into an unbreakable
                // ordering cycle (e.g. poweroff.target pulling in
                // dbus.service/systemd-networkd.service via inverse-Conflicts).
                if svc != u {
                    out.insert(svc);
                }
            }
        }
        let mut v: Vec<String> = out.into_iter().collect();
        v.sort();
        v
    }

    /// The normalized `UNIT_ATOM_BEFORE` dependency set of `u`: declared
    /// `Before=` targets plus the units that declare `After= u`.
    fn before_deps(&self, units: &HashMap<String, UnitFile>, u: &str) -> Vec<String> {
        let mut out: HashSet<String> = units
            .get(u)
            .map(|uf| uf.unit.before.iter().cloned().collect())
            .unwrap_or_default();
        if let Some(v) = self.after.get(u) {
            out.extend(v.iter().cloned());
        }
        let mut v: Vec<String> = out.into_iter().collect();
        v.sort();
        v
    }

    /// Units that must be stopped when `u` stops: the inverse of
    /// `Requires=`, `Requisite=` and `PartOf=` (`UNIT_ATOM_PROPAGATE_STOP`
    /// members `REQUIRED_BY`, `REQUISITE_OF`, `CONSISTS_OF`).
    fn propagate_stop(&self, u: &str) -> Vec<String> {
        let mut out: HashSet<String> = HashSet::new();
        for m in [&self.requires, &self.requisite, &self.part_of] {
            if let Some(v) = m.get(u) {
                out.extend(v.iter().cloned());
            }
        }
        let mut v: Vec<String> = out.into_iter().collect();
        v.sort();
        v
    }

    /// Units that declare `Conflicts= u` (`UNIT_ATOM_CONFLICTED_BY`): they
    /// must be stopped when `u` is started (`CONFLICTED_BY` dependency atom).
    fn conflicted_by(&self, u: &str) -> Vec<String> {
        self.conflicted_by.get(u).cloned().unwrap_or_default()
    }
}

fn state_of(states: &HashMap<String, UnitActiveState>, u: &str) -> UnitActiveState {
    states.get(u).copied().unwrap_or(UnitActiveState::Unknown)
}

/// Whether a conflict- or propagation-derived `Stop` job for `unit` would be
/// a no-op: the unit is not active/activating and has no running job.
///
/// systemd only stops units that are (or could be) running — an idle unit
/// has nothing to stop, so the `Stop` job is never installed (it would be
/// redundant).  `Unknown` state (no cached entry) is treated the same way
/// here: a unit that has never reported any state and has no in-flight job
/// cannot be running.  This deliberately relaxes `job_type_is_redundant()`'s
/// conservative `Unknown` policy for *stop* injection: it keeps the
/// operation only when there is a running job or an active-like state to
/// act on, keeping later `Start`/`Stop` pairs from colliding over units
/// that were never started (e.g. `lightdm.service` `Conflicts=`
/// `plymouth-quit.service` during boot).
fn stop_job_is_noop(
    states: &HashMap<String, UnitActiveState>,
    installed: &HashMap<String, JobType>,
    unit: &str,
) -> bool {
    if installed.contains_key(unit) {
        return false;
    }
    !state_of(states, unit).is_active_or_activating()
}

/// systemd `job_type_is_conflicting()`: start/verify-active/reload jobs
/// conflict with every non-positive job.
fn job_type_is_conflicting(a: JobType, b: JobType) -> bool {
    let positive =
        |t: JobType| matches!(t, JobType::Start | JobType::VerifyActive | JobType::Reload);
    positive(a) != positive(b)
}

// ---------------------------------------------------------------------------
// Transaction internals
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Edge {
    subject: usize,
    object: usize,
    matters: bool,
    conflicts: bool,
}

#[derive(Debug)]
struct TJob {
    unit: String,
    type_: JobType,
    ignore_order: bool,
    anchor: bool,
    matters_to_anchor: bool,
    /// Edges where this job is the object (who pulled us in).
    incoming: Vec<Edge>,
    /// Edges where this job is the subject (what we pulled in).
    outgoing: Vec<Edge>,
}

/// The transaction job set, mirroring systemd's `Transaction` (a map of
/// unit → per-unit job lists, with a job arena standing in for the C
/// pointers).
struct Transaction {
    /// Per-unit lists of job arena indices; jobs are deduplicated by type.
    jobs: HashMap<String, Vec<usize>>,
    arena: Vec<Option<TJob>>,
}

impl Transaction {
    fn new() -> Self {
        Transaction {
            jobs: HashMap::new(),
            arena: Vec::new(),
        }
    }

    fn get(&self, idx: usize) -> &TJob {
        self.arena[idx].as_ref().expect("live job")
    }

    fn get_mut(&mut self, idx: usize) -> &mut TJob {
        self.arena[idx].as_mut().expect("live job")
    }

    fn is_live(&self, idx: usize) -> bool {
        matches!(self.arena.get(idx), Some(Some(_)))
    }

    /// Whether any job of `unit` matters to an anchor job.
    fn unit_matters(&self, unit: &str) -> bool {
        self.jobs
            .get(unit)
            .is_some_and(|l| l.iter().any(|&i| self.get(i).matters_to_anchor))
    }

    /// `transaction_add_one_job()`: find or create the job for (unit, type).
    fn add_one_job(&mut self, unit: &str, type_: JobType) -> (usize, bool) {
        if let Some(list) = self.jobs.get(unit) {
            if let Some(&i) = list.iter().find(|&&i| self.get(i).type_ == type_) {
                return (i, false);
            }
        }
        let idx = self.arena.len();
        self.arena.push(Some(TJob {
            unit: unit.to_string(),
            type_,
            ignore_order: false,
            anchor: false,
            matters_to_anchor: false,
            incoming: Vec::new(),
            outgoing: Vec::new(),
        }));
        self.jobs.entry(unit.to_string()).or_default().push(idx);
        (idx, true)
    }

    /// `job_dependency_new()`: record the edge subject → object, deduplicated
    /// with flags OR-ed in.
    fn add_edge(&mut self, subject: usize, object: usize, matters: bool, conflicts: bool) {
        if self
            .get(subject)
            .outgoing
            .iter()
            .any(|e| e.object == object)
        {
            if let Some(e) = self
                .get_mut(subject)
                .outgoing
                .iter_mut()
                .find(|e| e.object == object)
            {
                e.matters |= matters;
                e.conflicts |= conflicts;
            }
            if let Some(e) = self
                .get_mut(object)
                .incoming
                .iter_mut()
                .find(|e| e.subject == subject)
            {
                e.matters |= matters;
                e.conflicts |= conflicts;
            }
            return;
        }
        let e = Edge {
            subject,
            object,
            matters,
            conflicts,
        };
        self.get_mut(subject).outgoing.push(e.clone());
        self.get_mut(object).incoming.push(e);
    }

    fn remove_incoming(&mut self, object: usize, subject: usize) {
        if let Some(j) = self.arena.get_mut(object).and_then(|o| o.as_mut()) {
            j.incoming.retain(|e| e.subject != subject);
        }
    }

    fn remove_outgoing(&mut self, subject: usize, object: usize) {
        if let Some(j) = self.arena.get_mut(subject).and_then(|o| o.as_mut()) {
            j.outgoing.retain(|e| e.object != object);
        }
    }

    /// `transaction_delete_job()`: remove a job (and optionally, recursively,
    /// everything it pulled in).
    fn delete_job(&mut self, idx: usize, delete_dependencies: bool) {
        if !self.is_live(idx) {
            return;
        }
        if delete_dependencies {
            let targets: Vec<usize> = self.get(idx).outgoing.iter().map(|e| e.object).collect();
            for t in targets {
                self.delete_job(t, true);
            }
        }
        let (unit, outgoing, incoming) = {
            let j = self.get_mut(idx);
            (
                j.unit.clone(),
                std::mem::take(&mut j.outgoing),
                std::mem::take(&mut j.incoming),
            )
        };
        for e in outgoing {
            self.remove_incoming(e.object, idx);
        }
        for e in incoming {
            self.remove_outgoing(e.subject, idx);
        }
        if let Some(list) = self.jobs.get_mut(&unit) {
            list.retain(|&i| i != idx);
            if list.is_empty() {
                self.jobs.remove(&unit);
            }
        }
        self.arena[idx] = None;
    }

    /// `transaction_delete_unit()`: delete every job of the unit.
    fn delete_unit(&mut self, unit: &str) {
        let list = self.jobs.get(unit).cloned().unwrap_or_default();
        for &i in &list {
            self.delete_job(i, false);
        }
        self.jobs.remove(unit);
    }

    /// Whether `j` was pulled in by at least one `Conflicts=` edge
    /// (`job_is_conflicted_by()`).
    fn conflicted_by(&self, idx: usize) -> bool {
        self.get(idx).incoming.iter().any(|e| e.conflicts)
    }

    // ------------------------------------------------------------------
    // Build phase (`transaction_add_job_and_dependencies`)
    // ------------------------------------------------------------------

    // `installed` is only threaded through the recursion to mirror
    // systemd's `transaction_add_job_and_dependencies()`; the signature
    // intentionally keeps the dependency graph context flat.
    #[allow(clippy::too_many_arguments, clippy::only_used_in_recursion)]
    fn add_job_and_dependencies(
        &mut self,
        units: &HashMap<String, UnitFile>,
        states: &HashMap<String, UnitActiveState>,
        installed: &HashMap<String, JobType>,
        rev: &ReverseIndex,
        unit: &str,
        type_: JobType,
        by: Option<usize>,
        flags: u32,
    ) -> Result<(), PlanError> {
        let Some(uf) = units.get(unit) else {
            return Err(PlanError::UnitNotFound(unit.to_string()));
        };
        let (job, is_new) = self.add_one_job(unit, type_);
        if flags & IGNORE_ORDER != 0 {
            self.get_mut(job).ignore_order = true;
        }
        if let Some(by) = by {
            self.add_edge(by, job, flags & MATTERS != 0, flags & CONFLICTS != 0);
        } else {
            self.get_mut(job).anchor = true;
        }
        if !is_new || flags & IGNORE_REQUIREMENTS != 0 || type_ == JobType::Nop {
            return Ok(());
        }

        let section = &uf.unit;
        if matches!(type_, JobType::Start | JobType::Restart) {
            for dep in section.requires.iter().chain(section.binds_to.iter()) {
                self.add_job_and_dependencies(
                    units,
                    states,
                    installed,
                    rev,
                    dep,
                    JobType::Start,
                    Some(job),
                    MATTERS | (flags & IGNORE_ORDER),
                )?;
            }
            for dep in section.wants.iter().chain(section.upholds.iter()) {
                if let Err(e) = self.add_job_and_dependencies(
                    units,
                    states,
                    installed,
                    rev,
                    dep,
                    JobType::Start,
                    Some(job),
                    flags & IGNORE_ORDER,
                ) {
                    warn!("Cannot add dependency job for {dep}: {e}");
                }
            }
            // Socket units from Sockets= are pulled in like Wants= (soft
            // dependency): the socket should be started, but failure to
            // start it does not prevent the service from starting.
            if let Some(svc) = &uf.service {
                for dep in &svc.sockets {
                    if let Err(e) = self.add_job_and_dependencies(
                        units,
                        states,
                        installed,
                        rev,
                        dep,
                        JobType::Start,
                        Some(job),
                        flags & IGNORE_ORDER,
                    ) {
                        warn!("Cannot add socket activation job for {dep}: {e}");
                    }
                }
            }
            for dep in &section.requisite {
                self.add_job_and_dependencies(
                    units,
                    states,
                    installed,
                    rev,
                    dep,
                    JobType::VerifyActive,
                    Some(job),
                    MATTERS | (flags & IGNORE_ORDER),
                )?;
            }
            // Conflicts= with a missing unit is a no-op: a non-existent
            // unit can never be active, so there is nothing to stop.
            for dep in &section.conflicts {
                if let Err(e) = self.add_job_and_dependencies(
                    units,
                    states,
                    installed,
                    rev,
                    dep,
                    JobType::Stop,
                    Some(job),
                    MATTERS | CONFLICTS | (flags & IGNORE_ORDER),
                ) {
                    warn!("Cannot add conflict stop job for {dep}: {e}");
                }
            }
            // Inverse Conflicts= (`UNIT_ATOM_CONFLICTED_BY`): every unit
            // that declares `Conflicts=<this unit>` must be stopped when
            // this unit is STARTED.  systemd injects `Conflicts=shutdown.target`
            // into every default-dependencies unit, so starting
            // `shutdown.target` stops the whole system — this is what makes
            // poweroff/halt actually shut services down before the hardware
            // power transition.
            for u in rev.conflicted_by(unit) {
                // A Stop for a unit that is neither running nor being
                // started would be a no-op — and, worse, would later
                // collide with a legitimate Start for the same unit (e.g. a
                // display manager that `Conflicts=` the plymouth boot
                // splash).  systemd never installs it; skip it here too.
                if stop_job_is_noop(states, installed, &u) {
                    debug!(
                        "Skipping inverse-conflict stop for {u} via {unit}: not active/activating, no running job"
                    );
                    continue;
                }
                if let Err(e) = self.add_job_and_dependencies(
                    units,
                    states,
                    installed,
                    rev,
                    &u,
                    JobType::Stop,
                    Some(job),
                    MATTERS | CONFLICTS | (flags & IGNORE_ORDER),
                ) {
                    warn!("Cannot add inverse conflict stop job for {u}: {e}");
                }
            }
        }

        if matches!(type_, JobType::Restart | JobType::Stop)
            || (type_ == JobType::Start && flags & PROPAGATE_START_AS_RESTART != 0)
        {
            let is_stop = type_ == JobType::Stop;
            for x in rev.propagate_stop(unit) {
                let nt = job_type_collapse(
                    if is_stop {
                        JobType::Stop
                    } else {
                        JobType::TryRestart
                    },
                    state_of(states, &x),
                );
                if nt == JobType::Nop {
                    continue;
                }
                // Idle units have nothing to stop; skipping them avoids
                // both a spurious worker round-trip and a later
                // Start/Stop collision on the same unit.
                if is_stop && stop_job_is_noop(states, installed, &x) {
                    debug!(
                        "Skipping propagate-stop for {x} via {unit}: not active/activating, no running job"
                    );
                    continue;
                }
                self.add_job_and_dependencies(
                    units,
                    states,
                    installed,
                    rev,
                    &x,
                    nt,
                    Some(job),
                    MATTERS | (flags & IGNORE_ORDER),
                )?;
            }
        }

        if type_ == JobType::Reload {
            for dep in &section.propagates_reload_to {
                let nt = job_type_collapse(JobType::TryReload, state_of(states, dep));
                if nt == JobType::Nop {
                    continue;
                }
                if let Err(e) = self.add_job_and_dependencies(
                    units,
                    states,
                    installed,
                    rev,
                    dep,
                    nt,
                    Some(job),
                    flags & IGNORE_ORDER,
                ) {
                    warn!("Cannot add dependency reload job for {dep}: {e}");
                }
            }
        }

        Ok(())
    }

    /// `transaction_add_isolate_jobs()`: stop every active unit that is not
    /// part of the transaction.
    fn add_isolate_jobs(
        &mut self,
        units: &HashMap<String, UnitFile>,
        states: &HashMap<String, UnitActiveState>,
        installed: &HashMap<String, JobType>,
        rev: &ReverseIndex,
    ) {
        let Some(anchor) = self
            .arena
            .iter()
            .enumerate()
            .find(|(_, j)| j.as_ref().is_some_and(|j| j.anchor))
            .map(|(i, _)| i)
        else {
            return;
        };
        let mut names: Vec<&String> = units.keys().collect();
        names.sort();
        for name in names {
            if self.jobs.contains_key(name) {
                continue;
            }
            let state = state_of(states, name);
            if state.is_inactive_or_failed() && !installed.contains_key(name) {
                continue;
            }
            if let Err(e) = self.add_job_and_dependencies(
                units,
                states,
                installed,
                rev,
                name,
                JobType::Stop,
                Some(anchor),
                MATTERS,
            ) {
                warn!("Cannot add isolate stop job for {name}: {e}");
            }
        }
    }

    // ------------------------------------------------------------------
    // Matters-to-anchor (`transaction_find_jobs_that_matter_to_anchor`)
    // ------------------------------------------------------------------

    fn find_matters(&mut self) {
        let mut stack: Vec<usize> = self
            .arena
            .iter()
            .enumerate()
            .filter(|(_, j)| j.as_ref().is_some_and(|j| j.anchor))
            .map(|(i, _)| i)
            .collect();
        while let Some(i) = stack.pop() {
            if self.get(i).matters_to_anchor {
                continue;
            }
            self.get_mut(i).matters_to_anchor = true;
            let targets: Vec<usize> = self
                .get(i)
                .outgoing
                .iter()
                .filter(|e| e.matters)
                .map(|e| e.object)
                .collect();
            stack.extend(targets);
        }
    }

    // ------------------------------------------------------------------
    // Minimize impact (`transaction_minimize_impact`)
    // ------------------------------------------------------------------

    fn minimize_impact(
        &mut self,
        states: &HashMap<String, UnitActiveState>,
        installed: &HashMap<String, JobType>,
        mode: PlannerMode,
    ) -> Result<(), PlanError> {
        if !matches!(mode, PlannerMode::Fail | PlannerMode::Lenient) {
            return Ok(());
        }
        'rescan: loop {
            let mut names: Vec<String> = self.jobs.keys().cloned().collect();
            names.sort();
            for unit in names {
                let state = state_of(states, &unit);
                let list = self.jobs.get(&unit).cloned().unwrap_or_default();
                for &i in &list {
                    let matters = self.get(i).matters_to_anchor;
                    if matters && mode != PlannerMode::Lenient {
                        continue;
                    }
                    let stops_running =
                        self.get(i).type_ == JobType::Stop && state.is_active_or_activating();
                    let changes_existing = installed
                        .get(&unit)
                        .is_some_and(|t| job_type_is_conflicting(self.get(i).type_, *t));
                    if !stops_running && !changes_existing {
                        continue;
                    }
                    if matters {
                        return Err(PlanError::Destructive);
                    }
                    let u = self.get(i).unit.clone();
                    debug!("Deleting {u} to minimize impact");
                    self.delete_job(i, true);
                    continue 'rescan;
                }
            }
            break;
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Drop redundant (`transaction_drop_redundant`)
    // ------------------------------------------------------------------

    fn drop_redundant(
        &mut self,
        states: &HashMap<String, UnitActiveState>,
        installed: &HashMap<String, JobType>,
    ) {
        loop {
            let mut to_delete: Option<String> = None;
            let mut names: Vec<String> = self.jobs.keys().cloned().collect();
            names.sort();
            for unit in names {
                let state = state_of(states, &unit);
                let keep = self.jobs[&unit].iter().copied().any(|i| {
                    let j = self.get(i);
                    j.anchor
                        || !job_type_is_redundant(j.type_, state)
                        || installed
                            .get(&unit)
                            .is_some_and(|t| job_type_is_conflicting(j.type_, *t))
                });
                if !keep {
                    to_delete = Some(unit);
                    break;
                }
            }
            match to_delete {
                Some(unit) => self.delete_unit(&unit),
                None => break,
            }
        }
    }

    // ------------------------------------------------------------------
    // Garbage collection (`transaction_collect_garbage`)
    // ------------------------------------------------------------------

    fn collect_garbage(&mut self) {
        loop {
            let mut to_delete: Option<usize> = None;
            let mut names: Vec<String> = self.jobs.keys().cloned().collect();
            names.sort();
            for unit in names {
                let list = self.jobs.get(&unit).cloned().unwrap_or_default();
                for &i in &list {
                    let j = self.get(i);
                    if j.anchor {
                        continue;
                    }
                    if j.incoming.is_empty() {
                        to_delete = Some(i);
                        break;
                    }
                }
                if to_delete.is_some() {
                    break;
                }
            }
            match to_delete {
                Some(i) => {
                    let u = self.get(i).unit.clone();
                    debug!("Garbage collecting job for {u}");
                    self.delete_job(i, true);
                }
                None => break,
            }
        }
    }

    // ------------------------------------------------------------------
    // Order verification (`transaction_verify_order`)
    // ------------------------------------------------------------------

    fn job_type_of(&self, j: &JobId, installed: &HashMap<String, JobType>) -> JobType {
        match j {
            JobId::Tr(i) => self.get(*i).type_,
            JobId::Inst(u) => installed.get(u).copied().unwrap_or(JobType::Nop),
        }
    }

    fn job_ignore_of(&self, j: &JobId) -> bool {
        match j {
            JobId::Tr(i) => self.get(*i).ignore_order,
            JobId::Inst(_) => false,
        }
    }

    /// Resolve an ordering-dependency target to a transaction job or an
    /// installed job.
    fn resolve_job(&self, unit: &str, installed: &HashMap<String, JobType>) -> Option<JobId> {
        if let Some(list) = self.jobs.get(unit) {
            return list.first().map(|&i| JobId::Tr(i));
        }
        if installed.contains_key(unit) {
            return Some(JobId::Inst(unit.to_string()));
        }
        None
    }

    /// systemd `job_compare(a, b, assume_dep)` with `assume_dep`
    /// (`Dir::After` = a is assumed after b, `Dir::Before` = a is assumed
    /// before b). Returns >0 if a should run after b, <0 if a should run
    /// before b, 0 if independent.
    fn job_compare(a: JobType, b: JobType, a_ignore: bool, b_ignore: bool, dir: Dir) -> i32 {
        if a == JobType::Nop || b == JobType::Nop {
            return 0;
        }
        if a_ignore || b_ignore {
            return 0;
        }
        match dir {
            Dir::After => -Self::job_compare(b, a, b_ignore, a_ignore, Dir::Before),
            Dir::Before => {
                if matches!(b, JobType::Stop | JobType::Restart) {
                    1
                } else {
                    -1
                }
            }
        }
    }

    /// `transaction_verify_order_one()`: recursive DFS over the ordering
    /// graph. Returns `Ok(true)` when a cycle was broken by deleting a job
    /// (systemd's `-EAGAIN`).
    #[allow(clippy::too_many_arguments)]
    fn verify_order_one(
        &mut self,
        units: &HashMap<String, UnitFile>,
        installed: &HashMap<String, JobType>,
        rev: &ReverseIndex,
        j: JobId,
        from: Option<JobId>,
        generation: u32,
        dfs: &mut HashMap<JobId, DfsState>,
    ) -> Result<bool, PlanError> {
        if let Some(s) = dfs.get(&j) {
            if s.generation == generation {
                if s.marker.is_none() {
                    return Ok(false);
                }
                // We are on the path again: ordering cycle. Walk back along
                // the markers and find the first non-mattering job to delete.
                let mut delete: Option<JobId> = None;
                let mut k = from;
                while let Some(kk) = k {
                    if delete.is_none() {
                        if let JobId::Tr(i) = kk {
                            if self.is_live(i) && !self.unit_matters(&self.get(i).unit) {
                                delete = Some(kk.clone());
                            }
                        }
                    }
                    if kk == j {
                        break;
                    }
                    k = match dfs.get(&kk) {
                        Some(s) if s.generation == generation && s.marker.as_ref() != Some(&kk) => {
                            s.marker.clone()
                        }
                        _ => None,
                    };
                }
                if let Some(JobId::Tr(i)) = delete {
                    let unit = self.get(i).unit.clone();
                    warn!("Deleting {unit} to break ordering cycle");
                    self.delete_unit(&unit);
                    return Ok(true);
                }
                return Err(PlanError::Cyclic);
            }
        }

        let marker = from.or(Some(j.clone()));
        dfs.insert(j.clone(), DfsState { generation, marker });

        let (junit, jtype, jignore) = match &j {
            JobId::Tr(i) => {
                let t = self.get(*i);
                (t.unit.clone(), t.type_, t.ignore_order)
            }
            JobId::Inst(u) => (
                u.clone(),
                installed.get(u).copied().unwrap_or(JobType::Nop),
                false,
            ),
        };

        // directions: BEFORE, AFTER (systemd's `directions[]` order).
        for dep in rev.before_deps(units, &junit) {
            let Some(od) = self.resolve_job(&dep, installed) else {
                continue;
            };
            if Self::job_compare(
                jtype,
                self.job_type_of(&od, installed),
                jignore,
                self.job_ignore_of(&od),
                Dir::Before,
            ) >= 0
            {
                continue;
            }
            if self.verify_order_one(units, installed, rev, od, Some(j.clone()), generation, dfs)? {
                return Ok(true);
            }
        }
        for dep in rev.after_deps(units, &junit) {
            let Some(od) = self.resolve_job(&dep, installed) else {
                continue;
            };
            if Self::job_compare(
                jtype,
                self.job_type_of(&od, installed),
                jignore,
                self.job_ignore_of(&od),
                Dir::After,
            ) >= 0
            {
                continue;
            }
            if self.verify_order_one(units, installed, rev, od, Some(j.clone()), generation, dfs)? {
                return Ok(true);
            }
        }

        if let Some(s) = dfs.get_mut(&j) {
            s.marker = None;
        }
        Ok(false)
    }

    /// `transaction_verify_order()`: check the ordering graph for cycles,
    /// breaking them when possible. Returns `Ok(true)` if a cycle was broken.
    fn verify_order(
        &mut self,
        units: &HashMap<String, UnitFile>,
        installed: &HashMap<String, JobType>,
        rev: &ReverseIndex,
        generation: u32,
    ) -> Result<bool, PlanError> {
        let mut dfs: HashMap<JobId, DfsState> = HashMap::new();
        let mut names: Vec<String> = self.jobs.keys().cloned().collect();
        names.sort();
        for unit in names {
            if let Some(&head) = self.jobs.get(&unit).and_then(|l| l.first()) {
                if self.verify_order_one(
                    units,
                    installed,
                    rev,
                    JobId::Tr(head),
                    None,
                    generation,
                    &mut dfs,
                )? {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    // ------------------------------------------------------------------
    // Merging (`transaction_merge_jobs`)
    // ------------------------------------------------------------------

    /// `transaction_drop_nop()`: drop a NOP job when a regular job for the
    /// same unit exists (handing over any anchor identity).
    fn drop_nop(&mut self) {
        let mut to_remove: Vec<(String, usize)> = Vec::new();
        let mut names: Vec<String> = self.jobs.keys().cloned().collect();
        names.sort();
        for unit in names {
            let list = self.jobs.get(&unit).cloned().unwrap_or_default();
            let nop = list
                .iter()
                .copied()
                .find(|&i| self.get(i).type_ == JobType::Nop);
            let regular = list
                .iter()
                .copied()
                .find(|&i| self.get(i).type_ != JobType::Nop);
            if let (Some(n), Some(r)) = (nop, regular) {
                if self.get(n).anchor {
                    self.get_mut(r).anchor = true;
                }
                to_remove.push((unit, n));
            }
        }
        for (unit, n) in to_remove {
            debug!("Dropping NOP job for {unit}");
            self.delete_job(n, false);
        }
    }

    /// `delete_one_unmergeable_job()`: pick the job to delete from an
    /// unmergeable per-unit pair. Returns the deleted job index.
    fn delete_one_unmergeable_job(&mut self, unit: &str) -> Option<usize> {
        let list = self.jobs.get(unit).cloned().unwrap_or_default();
        for (i, &j) in list.iter().enumerate() {
            for &k in &list[i + 1..] {
                if job_type_lookup_merge(self.get(j).type_, self.get(k).type_).is_some() {
                    continue;
                }
                let jm = self.get(j).matters_to_anchor;
                let km = self.get(k).matters_to_anchor;
                let d = if !jm && !km {
                    let jc = self.conflicted_by(j);
                    let kc = self.conflicted_by(k);
                    if self.get(j).type_ == JobType::Stop && jc {
                        k
                    } else if (self.get(k).type_ == JobType::Stop && kc)
                        || self.get(j).type_ == JobType::Stop
                    {
                        j
                    } else if self.get(k).type_ == JobType::Stop {
                        k
                    } else {
                        j
                    }
                } else if !jm {
                    j
                } else if !km {
                    k
                } else {
                    return None;
                };
                let u = self.get(d).unit.clone();
                debug!("Fixing conflicting jobs by deleting {u}");
                self.delete_job(d, true);
                return Some(d);
            }
        }
        None
    }

    /// `transaction_ensure_mergeable()`: drop unmergeable jobs for the given
    /// mattering class. Returns `Ok(true)` when a job was deleted.
    fn ensure_mergeable(
        &mut self,
        states: &HashMap<String, UnitActiveState>,
        matters_param: bool,
    ) -> Result<bool, PlanError> {
        let mut names: Vec<String> = self.jobs.keys().cloned().collect();
        names.sort();
        for unit in names {
            if self.unit_matters(&unit) != matters_param {
                continue;
            }
            let state = state_of(states, &unit);
            let list = self.jobs.get(&unit).cloned().unwrap_or_default();
            if list.len() < 2 {
                continue;
            }
            let t = self.get(list[0]).type_;
            for &k in &list[1..] {
                if job_type_merge_and_collapse(t, self.get(k).type_, state).is_some() {
                    continue;
                }
                match self.delete_one_unmergeable_job(&unit) {
                    Some(_) => return Ok(true),
                    None => return Err(PlanError::Conflicting),
                }
            }
        }
        Ok(false)
    }

    /// `transaction_merge_and_delete_job()`: fold `other` into `survivor`,
    /// repointing all edges and OR-ing the flags.
    fn merge_job_into(&mut self, survivor: usize, other: usize, merged_type: JobType) {
        let outgoing = std::mem::take(&mut self.get_mut(other).outgoing);
        for e in outgoing {
            self.add_edge(survivor, e.object, e.matters, e.conflicts);
            self.remove_incoming(e.object, other);
        }
        let incoming = std::mem::take(&mut self.get_mut(other).incoming);
        for e in incoming {
            self.add_edge(e.subject, survivor, e.matters, e.conflicts);
            self.remove_outgoing(e.subject, other);
        }
        let (om, oa) = {
            let o = self.get(other);
            (o.matters_to_anchor, o.anchor)
        };
        let s = self.get_mut(survivor);
        s.type_ = merged_type;
        s.matters_to_anchor |= om;
        s.anchor |= oa;
        self.delete_job(other, false);
    }

    /// `transaction_merge_jobs()`: ensure per-unit lists are mergeable, then
    /// merge each unit's jobs into one.
    fn merge_jobs(
        &mut self,
        states: &HashMap<String, UnitActiveState>,
    ) -> Result<MergeOutcome, PlanError> {
        self.drop_nop();
        if self.ensure_mergeable(states, true)? {
            return Ok(MergeOutcome::Deleted);
        }
        if self.ensure_mergeable(states, false)? {
            return Ok(MergeOutcome::Deleted);
        }
        let mut names: Vec<String> = self.jobs.keys().cloned().collect();
        names.sort();
        for unit in names {
            let list = self.jobs.get(&unit).cloned().unwrap_or_default();
            if list.len() <= 1 {
                continue;
            }
            let state = state_of(states, &unit);
            let mut t = self.get(list[0]).type_;
            for &k in &list[1..] {
                match job_type_merge_and_collapse(t, self.get(k).type_, state) {
                    Some(nt) => t = nt,
                    None => return Err(PlanError::Conflicting),
                }
            }
            let survivor = list
                .iter()
                .copied()
                .find(|&k| self.get(k).anchor)
                .unwrap_or(list[0]);
            for &k in &list {
                if k != survivor {
                    self.merge_job_into(survivor, k, t);
                }
            }
        }
        Ok(MergeOutcome::Done)
    }

    // ------------------------------------------------------------------
    // Destructive check (`transaction_is_destructive`)
    // ------------------------------------------------------------------

    fn is_destructive(
        &self,
        installed: &HashMap<String, JobType>,
        mode: PlannerMode,
    ) -> Result<(), PlanError> {
        if !matches!(mode, PlannerMode::Fail | PlannerMode::Lenient) {
            return Ok(());
        }
        for (unit, list) in &self.jobs {
            if let Some(it) = installed.get(unit) {
                if list
                    .iter()
                    .any(|&i| job_type_is_conflicting(self.get(i).type_, *it))
                {
                    return Err(PlanError::Destructive);
                }
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Finalize: execution order
    // ------------------------------------------------------------------

    /// systemd `job_is_runnable()`: whether step `a` must wait for step `b`
    /// in serial execution order.
    fn step_must_wait(
        &self,
        a: &PlanStep,
        b: &PlanStep,
        units: &HashMap<String, UnitFile>,
        rev: &ReverseIndex,
    ) -> bool {
        if a.job_type == JobType::Nop || b.job_type == JobType::Nop {
            return false;
        }
        if a.ignore_order || b.ignore_order {
            return false;
        }
        let after = rev.after_deps(units, &a.unit);
        let before = rev.before_deps(units, &a.unit);
        // After= deps of a: a waits unless a is a stop/restart (which always
        // run first).
        if after.contains(&b.unit) && !matches!(a.job_type, JobType::Stop | JobType::Restart) {
            return true;
        }
        // Before= deps of a: a waits when the target is a stop/restart.
        if before.contains(&b.unit) && matches!(b.job_type, JobType::Stop | JobType::Restart) {
            return true;
        }
        false
    }

    fn finalize(
        &self,
        units: &HashMap<String, UnitFile>,
        rev: &ReverseIndex,
    ) -> Result<TransactionPlan, PlanError> {
        let mut names: Vec<String> = self.jobs.keys().cloned().collect();
        names.sort();
        let mut steps: Vec<PlanStep> = Vec::with_capacity(names.len());
        for unit in &names {
            let list = &self.jobs[unit];
            let &j = list.first().expect("merged job");
            let t = self.get(j);
            steps.push(PlanStep {
                unit: unit.clone(),
                job_type: t.type_,
                anchor: t.anchor,
                matters_to_anchor: t.matters_to_anchor,
                ignore_order: t.ignore_order,
            });
        }

        // Build the wait relation (a must run after b).
        let n = steps.len();
        let mut waits: Vec<Vec<usize>> = vec![Vec::new(); n];
        for i in 0..n {
            for k in 0..n {
                if i == k {
                    continue;
                }
                if self.step_must_wait(&steps[i], &steps[k], units, rev) {
                    waits[i].push(k);
                }
            }
        }

        // Kahn's algorithm; deterministic (sorted unit order as the queue).
        let mut indeg: Vec<usize> = waits.iter().map(|w| w.len()).collect();
        let mut ready: VecDeque<usize> = (0..n).filter(|&i| indeg[i] == 0).collect();
        let mut order: Vec<usize> = Vec::with_capacity(n);
        while let Some(i) = ready.pop_front() {
            order.push(i);
            for (idx, w) in waits.iter().enumerate() {
                if w.contains(&i) {
                    indeg[idx] -= 1;
                    if indeg[idx] == 0 {
                        ready.push_back(idx);
                    }
                }
            }
        }
        if order.len() != n {
            return Err(PlanError::Cyclic);
        }

        let ordered: Vec<PlanStep> = order.into_iter().map(|i| steps[i].clone()).collect();
        Ok(TransactionPlan { steps: ordered })
    }
}

enum MergeOutcome {
    Done,
    Deleted,
}

/// Identifies a vertex of the ordering graph: a transaction job or an
/// installed (running) job.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum JobId {
    Tr(usize),
    Inst(String),
}

#[derive(Debug, Clone)]
struct DfsState {
    generation: u32,
    /// The vertex we came from; `Some(self)` marks the path start.
    marker: Option<JobId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dir {
    Before,
    After,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Build a transaction plan for the requested root operation.
///
/// Mirrors `manager_add_job_full()` + `transaction_activate()`:
/// the root job is anchored with `TRANSACTION_MATTERS` plus the flags
/// derived from `mode`, then the full pipeline runs and the surviving jobs
/// are returned in deterministic execution order.
pub fn build_plan(
    units: &HashMap<String, UnitFile>,
    states: &HashMap<String, UnitActiveState>,
    installed: &HashMap<String, JobType>,
    root: &str,
    root_type: JobType,
    mode: PlannerMode,
) -> Result<TransactionPlan, PlanError> {
    build_plan_multi(units, states, installed, &[(root, root_type)], mode)
}

/// `EnqueueUnitJobMany()`-style multi-anchor transaction planner.
///
/// Creates a single [`Transaction`] with every root in `roots` added as an
/// anchor job (`by=NULL`), then runs the full systemd pipeline:
///
/// 1. `find_matters` — walk mattering edges from every anchor.
/// 2. `minimize_impact` — drop non-mattering destructive jobs.
/// 3. `drop_redundant` — remove jobs for already-active units (anchors
///    are never dropped).
/// 4. `collect_garbage` + `verify_order` (fixpoint loop).
/// 5. `merge_jobs` + `collect_garbage` (fixpoint loop).
/// 6. `drop_redundant` again.
/// 7. `is_destructive` check.
/// 8. `finalize` — topological sort via `step_must_wait`.
pub fn build_plan_multi(
    units: &HashMap<String, UnitFile>,
    states: &HashMap<String, UnitActiveState>,
    installed: &HashMap<String, JobType>,
    roots: &[(&str, JobType)],
    mode: PlannerMode,
) -> Result<TransactionPlan, PlanError> {
    for (root, _) in roots {
        if !units.contains_key(*root) {
            return Err(PlanError::UnitNotFound(root.to_string()));
        }
    }
    let rev = ReverseIndex::build(units);
    let mut tr = Transaction::new();

    let mut flags = MATTERS;
    if matches!(
        mode,
        PlannerMode::IgnoreDependencies | PlannerMode::IgnoreRequirements
    ) {
        flags |= IGNORE_REQUIREMENTS;
    }
    if mode == PlannerMode::IgnoreDependencies {
        flags |= IGNORE_ORDER;
    }
    if mode == PlannerMode::RestartDependencies {
        flags |= PROPAGATE_START_AS_RESTART;
    }
    for (root, root_type) in roots {
        tr.add_job_and_dependencies(
            units, states, installed, &rev, root, *root_type, None, flags,
        )?;
    }

    if mode == PlannerMode::Isolate {
        tr.add_isolate_jobs(units, states, installed, &rev);
    }

    tr.find_matters();
    tr.minimize_impact(states, installed, mode)?;
    tr.drop_redundant(states, installed);

    let mut generation: u32 = 1;
    loop {
        if mode != PlannerMode::Isolate {
            tr.collect_garbage();
        }
        if !tr.verify_order(units, installed, &rev, generation)? {
            break;
        }
        generation = generation.wrapping_add(1);
    }

    loop {
        match tr.merge_jobs(states)? {
            MergeOutcome::Done => break,
            MergeOutcome::Deleted => {
                if mode != PlannerMode::Isolate {
                    tr.collect_garbage();
                }
            }
        }
    }

    tr.drop_redundant(states, installed);
    tr.is_destructive(installed, mode)?;
    tr.finalize(units, &rev)
}

#[cfg(test)]
mod tests {
    use super::super::job_type::{JobType, UnitActiveState};
    use super::*;
    use crate::unit::types::UnitFile;
    use std::collections::HashMap;

    use JobType::*;
    use PlannerMode::*;
    use UnitActiveState::*;

    // ------------------------------------------------------------------
    // Unit helpers
    // ------------------------------------------------------------------

    fn make_unit(name: &str) -> UnitFile {
        UnitFile::new(name)
    }

    fn with_requires(mut u: UnitFile, deps: &[&str]) -> UnitFile {
        for d in deps {
            u.unit.requires.insert(d.to_string());
        }
        u
    }

    fn with_wants(mut u: UnitFile, deps: &[&str]) -> UnitFile {
        for d in deps {
            u.unit.wants.insert(d.to_string());
        }
        u
    }

    fn with_binds_to(mut u: UnitFile, deps: &[&str]) -> UnitFile {
        for d in deps {
            u.unit.binds_to.insert(d.to_string());
        }
        u
    }

    fn with_requisite(mut u: UnitFile, deps: &[&str]) -> UnitFile {
        for d in deps {
            u.unit.requisite.insert(d.to_string());
        }
        u
    }

    fn with_conflicts(mut u: UnitFile, deps: &[&str]) -> UnitFile {
        for d in deps {
            u.unit.conflicts.insert(d.to_string());
        }
        u
    }

    fn with_after(mut u: UnitFile, deps: &[&str]) -> UnitFile {
        for d in deps {
            u.unit.after.insert(d.to_string());
        }
        u
    }

    fn with_before(mut u: UnitFile, deps: &[&str]) -> UnitFile {
        for d in deps {
            u.unit.before.insert(d.to_string());
        }
        u
    }

    fn with_part_of(mut u: UnitFile, deps: &[&str]) -> UnitFile {
        for d in deps {
            u.unit.part_of.insert(d.to_string());
        }
        u
    }

    fn with_upholds(mut u: UnitFile, deps: &[&str]) -> UnitFile {
        for d in deps {
            u.unit.upholds.insert(d.to_string());
        }
        u
    }

    fn with_reload_to(mut u: UnitFile, deps: &[&str]) -> UnitFile {
        for d in deps {
            u.unit.propagates_reload_to.insert(d.to_string());
        }
        u
    }

    fn with_sockets(mut u: UnitFile, sockets: &[&str]) -> UnitFile {
        let svc = u.service.get_or_insert_with(Default::default);
        svc.sockets = sockets.iter().map(|s| s.to_string()).collect();
        u
    }

    fn with_socket_service(mut u: UnitFile, service: &str) -> UnitFile {
        let s = u.socket.get_or_insert_with(Default::default);
        s.service = service.to_string();
        u
    }

    fn map(units: Vec<UnitFile>) -> HashMap<String, UnitFile> {
        units.into_iter().map(|u| (u.name.clone(), u)).collect()
    }

    fn steps(
        units: &HashMap<String, UnitFile>,
        states: &HashMap<String, UnitActiveState>,
        installed: &HashMap<String, JobType>,
        root: &str,
        t: JobType,
        mode: PlannerMode,
    ) -> Vec<PlanStep> {
        build_plan(units, states, installed, root, t, mode)
            .expect("plan should build")
            .steps
    }

    fn steps_multi(
        units: &HashMap<String, UnitFile>,
        states: &HashMap<String, UnitActiveState>,
        installed: &HashMap<String, JobType>,
        roots: &[(&str, JobType)],
        mode: PlannerMode,
    ) -> Vec<PlanStep> {
        build_plan_multi(units, states, installed, roots, mode)
            .expect("plan should build")
            .steps
    }

    fn names(steps: &[PlanStep]) -> Vec<&str> {
        steps.iter().map(|s| s.unit.as_str()).collect()
    }

    // ------------------------------------------------------------------
    // Build phase
    // ------------------------------------------------------------------

    /// Regression test for the original bug: `After=` must not pull units
    /// into the activation set.
    #[test]
    fn after_only_does_not_pull_in_units() {
        let units = map(vec![
            make_unit("a.service"),
            with_after(make_unit("b.service"), &["a.service"]),
        ]);
        let s = steps(
            &units,
            &HashMap::new(),
            &HashMap::new(),
            "b.service",
            Start,
            Replace,
        );
        assert_eq!(names(&s), vec!["b.service"]);
        assert_eq!(s[0].job_type, Start);
        assert!(s[0].anchor);
    }

    #[test]
    fn requires_pulls_in_dependency() {
        let units = map(vec![
            make_unit("a.service"),
            with_requires(make_unit("b.service"), &["a.service"]),
        ]);
        let s = steps(
            &units,
            &HashMap::new(),
            &HashMap::new(),
            "b.service",
            Start,
            Replace,
        );
        let mut names: Vec<&str> = names(&s);
        names.sort_unstable();
        assert_eq!(names, vec!["a.service", "b.service"]);
        let a = s.iter().find(|x| x.unit == "a.service").unwrap();
        assert_eq!(a.job_type, Start);
        assert!(a.matters_to_anchor);
        assert!(!a.anchor);
        let b = s.iter().find(|x| x.unit == "b.service").unwrap();
        assert!(b.anchor);
    }

    #[test]
    fn wants_pulls_in_without_mattering() {
        let units = map(vec![
            make_unit("a.service"),
            with_wants(make_unit("b.service"), &["a.service"]),
        ]);
        let s = steps(
            &units,
            &HashMap::new(),
            &HashMap::new(),
            "b.service",
            Start,
            Replace,
        );
        let a = s.iter().find(|x| x.unit == "a.service").unwrap();
        assert_eq!(a.job_type, Start);
        assert!(!a.matters_to_anchor, "wants pull-ins must not matter");
    }

    #[test]
    fn binds_to_pulls_in_dependency() {
        let units = map(vec![
            make_unit("a.service"),
            with_binds_to(make_unit("b.service"), &["a.service"]),
        ]);
        let s = steps(
            &units,
            &HashMap::new(),
            &HashMap::new(),
            "b.service",
            Start,
            Replace,
        );
        let a = s.iter().find(|x| x.unit == "a.service").unwrap();
        assert_eq!(a.job_type, Start);
        assert!(a.matters_to_anchor);
    }

    #[test]
    fn upholds_pulls_in_start_ignored() {
        let units = map(vec![
            make_unit("a.service"),
            with_upholds(make_unit("b.service"), &["a.service"]),
        ]);
        let s = steps(
            &units,
            &HashMap::new(),
            &HashMap::new(),
            "b.service",
            Start,
            Replace,
        );
        let a = s.iter().find(|x| x.unit == "a.service").unwrap();
        assert_eq!(a.job_type, Start);
        assert!(!a.matters_to_anchor);
    }

    #[test]
    fn requisite_pulls_in_verify_active() {
        let units = map(vec![
            make_unit("a.service"),
            with_requisite(make_unit("b.service"), &["a.service"]),
        ]);
        let s = steps(
            &units,
            &HashMap::new(),
            &HashMap::new(),
            "b.service",
            Start,
            Replace,
        );
        let a = s.iter().find(|x| x.unit == "a.service").unwrap();
        assert_eq!(a.job_type, VerifyActive);
        assert!(a.matters_to_anchor);
    }

    #[test]
    fn conflicts_pull_in_stop_job() {
        let units = map(vec![
            make_unit("a.service"),
            with_conflicts(make_unit("b.service"), &["a.service"]),
        ]);
        let s = steps(
            &units,
            &HashMap::new(),
            &HashMap::new(),
            "b.service",
            Start,
            Replace,
        );
        let a = s.iter().find(|x| x.unit == "a.service").unwrap();
        assert_eq!(a.job_type, Stop);
        assert!(a.matters_to_anchor);
    }

    #[test]
    fn inverse_conflicts_stop_units_that_declare_conflicts() {
        // Starting `a.service` must stop every *running* unit that declares
        // `Conflicts=a.service` (systemd `UNIT_ATOM_CONFLICTED_BY`), not
        // only the target's own forward Conflicts= list.
        let units = map(vec![
            make_unit("a.service"),
            with_conflicts(make_unit("x.service"), &["a.service"]),
            with_conflicts(make_unit("y.service"), &["a.service"]),
        ]);
        let states = {
            let mut m = HashMap::new();
            m.insert("x.service".to_string(), Active);
            m.insert("y.service".to_string(), Active);
            m
        };
        let s = steps(
            &units,
            &states,
            &HashMap::new(),
            "a.service",
            Start,
            Replace,
        );
        let x = s.iter().find(|x| x.unit == "x.service").unwrap();
        let y = s.iter().find(|x| x.unit == "y.service").unwrap();
        assert_eq!(x.job_type, Stop);
        assert_eq!(y.job_type, Stop);
        assert!(x.matters_to_anchor && y.matters_to_anchor);
    }

    #[test]
    fn inverse_conflicts_skip_stop_for_idle_conflicting_unit() {
        // Regression test for the lightdm boot hang: `lightdm.service`
        // declares `Conflicts=plymouth-quit.service` ("replaces plymouth-quit
        // since lightdm quits plymouth on its own").  Starting
        // plymouth-quit during boot must NOT emit a Stop for lightdm when
        // lightdm is not running — systemd only stops units that are (or
        // could be) active.  Emitting the spurious Stop made lightdm report
        // inactive, then the later `graphical.target` transaction collided
        // its Start with the leftover Stop and deleted lightdm entirely.
        let units = map(vec![
            make_unit("plymouth-quit.service"),
            with_conflicts(make_unit("lightdm.service"), &["plymouth-quit.service"]),
        ]);
        // lightdm was never started: no cached state, no running job.
        let s = steps(
            &units,
            &HashMap::new(),
            &HashMap::new(),
            "plymouth-quit.service",
            Start,
            Replace,
        );
        assert!(
            !s.iter().any(|x| x.unit == "lightdm.service"),
            "idle conflicting unit must not get a Stop job (plan: {:?})",
            names(&s)
        );

        // Contrast: once lightdm is actually running, the inverse-Conflicts
        // Stop must still be emitted.
        let states = {
            let mut m = HashMap::new();
            m.insert("lightdm.service".to_string(), Active);
            m
        };
        let s = steps(
            &units,
            &states,
            &HashMap::new(),
            "plymouth-quit.service",
            Start,
            Replace,
        );
        let lightdm = s.iter().find(|x| x.unit == "lightdm.service").unwrap();
        assert_eq!(lightdm.job_type, Stop);
        assert!(lightdm.matters_to_anchor);

        // A running job for the idle-looking unit also keeps the Stop.
        let installed = {
            let mut m = HashMap::new();
            m.insert("lightdm.service".to_string(), Start);
            m
        };
        let s = steps(
            &units,
            &HashMap::new(),
            &installed,
            "plymouth-quit.service",
            Start,
            Replace,
        );
        let lightdm = s.iter().find(|x| x.unit == "lightdm.service").unwrap();
        assert_eq!(lightdm.job_type, Stop);
    }

    #[test]
    fn propagate_stop_skip_idle_requirers() {
        // Stop-propagation (inverse Requires=) must only stop units that are
        // running or have a running job, same no-op gate as inverse-Conflicts.
        let units = map(vec![
            make_unit("x.target"),
            with_requires(make_unit("a.service"), &["x.target"]),
            with_requires(make_unit("b.service"), &["x.target"]),
        ]);
        let states = {
            let mut m = HashMap::new();
            m.insert("a.service".to_string(), Active);
            m.insert("b.service".to_string(), Inactive);
            m
        };
        let s = steps(
            &units,
            &states,
            &HashMap::new(),
            "x.target",
            Stop,
            Replace,
        );
        let a = s.iter().find(|x| x.unit == "a.service").unwrap();
        assert_eq!(a.job_type, Stop, "active requirer must be stopped");
        assert!(
            !s.iter().any(|x| x.unit == "b.service"),
            "idle requirer must not get a Stop job (plan: {:?})",
            names(&s)
        );
        let x = s.iter().find(|x| x.unit == "x.target").unwrap();
        assert_eq!(x.job_type, Stop);
        assert!(x.anchor);
    }

    #[test]
    fn starting_shutdown_target_stops_active_services() {
        // Mirror of the loader's default-dependencies injection: every
        // default-dep unit declares `Conflicts=shutdown.target` and
        // `Before=shutdown.target`.  Starting `shutdown.target` (as the
        // poweroff transaction does via `systemd-poweroff.service`) must
        // emit Stop jobs for the active services, ordered before the
        // shutdown.target start.
        let units = map(vec![
            with_conflicts(
                with_before(make_unit("sshd.service"), &["shutdown.target"]),
                &["shutdown.target"],
            ),
            with_conflicts(
                with_before(make_unit("nginx.service"), &["shutdown.target"]),
                &["shutdown.target"],
            ),
            make_unit("shutdown.target"),
        ]);
        let states = {
            let mut m = HashMap::new();
            m.insert("sshd.service".to_string(), Active);
            m.insert("nginx.service".to_string(), Active);
            m
        };
        let s = steps(
            &units,
            &states,
            &HashMap::new(),
            "shutdown.target",
            Start,
            Replace,
        );
        for svc in ["sshd.service", "nginx.service"] {
            let step = s.iter().find(|x| x.unit == svc).unwrap();
            assert_eq!(step.job_type, Stop, "{svc} should get a Stop job");
            assert!(step.matters_to_anchor, "{svc} Stop must matter");
        }
        let shutdown = s.iter().find(|x| x.unit == "shutdown.target").unwrap();
        assert_eq!(shutdown.job_type, Start);
        for svc in ["sshd.service", "nginx.service"] {
            let pos_svc = s.iter().position(|x| x.unit == svc).unwrap();
            let pos_shutdown = s.iter().position(|x| x.unit == "shutdown.target").unwrap();
            assert!(
                pos_svc < pos_shutdown,
                "{svc} stop must run before shutdown.target start (plan: {:?})",
                names(&s)
            );
        }
    }

    #[test]
    fn socket_resolution_does_not_self_order_service() {
        // A service that is ordered after its own socket (or requires it)
        // must not appear in its own `after_deps`.  Real systemd examples that
        // trip this: `systemd-networkd.service` declares `After=
        // systemd-networkd.socket` and `dbus.service` has `Requires=dbus.socket`
        // — the `.socket -> .service` convention resolution inserts the unit
        // into its own `after_deps`, which turns every mattering Stop job on
        // the unit into an unbreakable ordering cycle during poweroff.
        let units = map(vec![
            // "foo.socket" --[convention]--> foo.service
            with_conflicts(
                with_before(make_unit("foo.socket"), &["shutdown.target"]),
                &["shutdown.target"],
            ),
            // foo.service ordered after its own socket + inverse-Conflicts
            with_after(
                with_conflicts(
                    with_before(make_unit("foo.service"), &["shutdown.target"]),
                    &["shutdown.target"],
                ),
                &["foo.socket"],
            ),
            make_unit("shutdown.target"),
        ]);
        let rev = ReverseIndex::build(&units);
        let after = rev.after_deps(&units, "foo.service");
        assert!(
            !after.iter().any(|d| d == "foo.service"),
            "foo.service must not appear in its own after_deps: {after:?}"
        );
        // Same guarantee when the socket is pulled in via Requires=.
        let units = map(vec![
            with_conflicts(
                with_before(make_unit("bar.socket"), &["shutdown.target"]),
                &["shutdown.target"],
            ),
            with_requires(
                with_conflicts(
                    with_before(make_unit("bar.service"), &["shutdown.target"]),
                    &["shutdown.target"],
                ),
                &["bar.socket"],
            ),
            make_unit("shutdown.target"),
        ]);
        let rev = ReverseIndex::build(&units);
        let after = rev.after_deps(&units, "bar.service");
        assert!(
            !after.iter().any(|d| d == "bar.service"),
            "bar.service must not appear in its own after_deps: {after:?}"
        );
    }

    #[test]
    fn poweroff_target_builds_with_socket_owning_service() {
        // Mirror the real-host failure (dbus.service / systemd-networkd.service
        // ordering after their own sockets): with the inverse-Conflicts fix,
        // starting shutdown.target stops the services that conflict with it.
        // Before the self-order guard these services produced an unbreakable
        // ordering cycle; now the poweroff transaction must build and order
        // the stops before the shutdown.target start.
        let units = map(vec![
            with_after(
                with_conflicts(
                    with_before(make_unit("dbus.service"), &["shutdown.target"]),
                    &["shutdown.target"],
                ),
                &["dbus.socket"],
            ),
            with_conflicts(
                with_before(make_unit("dbus.socket"), &["shutdown.target"]),
                &["shutdown.target"],
            ),
            make_unit("shutdown.target"),
        ]);
        let states = {
            let mut m = HashMap::new();
            m.insert("dbus.service".to_string(), Active);
            m.insert("dbus.socket".to_string(), Active);
            m
        };
        let s = steps(
            &units,
            &states,
            &HashMap::new(),
            "shutdown.target",
            Start,
            ReplaceIrreversibly,
        );
        let svc = s.iter().find(|x| x.unit == "dbus.service").unwrap();
        assert_eq!(svc.job_type, Stop);
        assert!(svc.matters_to_anchor);
        let shutdown_pos = s.iter().position(|x| x.unit == "shutdown.target").unwrap();
        let svc_pos = s.iter().position(|x| x.unit == "dbus.service").unwrap();
        assert!(
            svc_pos < shutdown_pos,
            "service stop must run before shutdown.target start (plan: {:?})",
            names(&s)
        );
    }

    #[test]
    fn stop_propagates_to_requirers() {
        let units = map(vec![
            make_unit("a.service"),
            with_requires(make_unit("b.service"), &["a.service"]),
        ]);
        let states = {
            let mut m = HashMap::new();
            m.insert("b.service".to_string(), Active);
            m
        };
        let s = steps(
            &units,
            &states,
            &HashMap::new(),
            "a.service",
            Stop,
            Replace,
        );
        let b = s.iter().find(|x| x.unit == "b.service").unwrap();
        assert_eq!(b.job_type, Stop);
        assert!(b.matters_to_anchor);
    }

    #[test]
    fn stop_propagates_to_part_of_units() {
        let units = map(vec![
            make_unit("a.service"),
            with_part_of(make_unit("b.service"), &["a.service"]),
        ]);
        let states = {
            let mut m = HashMap::new();
            m.insert("b.service".to_string(), Active);
            m
        };
        let s = steps(
            &units,
            &states,
            &HashMap::new(),
            "a.service",
            Stop,
            Replace,
        );
        let b = s.iter().find(|x| x.unit == "b.service").unwrap();
        assert_eq!(b.job_type, Stop);
    }

    #[test]
    fn stop_propagates_to_requisite_units() {
        let units = map(vec![
            make_unit("a.service"),
            with_requisite(make_unit("b.service"), &["a.service"]),
        ]);
        let states = {
            let mut m = HashMap::new();
            m.insert("b.service".to_string(), Active);
            m
        };
        let s = steps(
            &units,
            &states,
            &HashMap::new(),
            "a.service",
            Stop,
            Replace,
        );
        let b = s.iter().find(|x| x.unit == "b.service").unwrap();
        assert_eq!(b.job_type, Stop);
    }

    #[test]
    fn reload_propagates_reload_to() {
        let units = map(vec![
            make_unit("a.service"),
            with_reload_to(make_unit("b.service"), &["a.service"]),
        ]);
        let s = steps(
            &units,
            &HashMap::new(),
            &HashMap::new(),
            "b.service",
            Reload,
            Replace,
        );
        let a = s.iter().find(|x| x.unit == "a.service").unwrap();
        assert_eq!(a.job_type, Reload);
    }

    #[test]
    fn reload_propagation_skipped_for_inactive_units() {
        let units = map(vec![
            make_unit("a.service"),
            with_reload_to(make_unit("b.service"), &["a.service"]),
        ]);
        let mut states = HashMap::new();
        states.insert("a.service".to_string(), Inactive);
        let s = steps(
            &units,
            &states,
            &HashMap::new(),
            "b.service",
            Reload,
            Replace,
        );
        assert_eq!(names(&s), vec!["b.service"]);
    }

    #[test]
    fn missing_dependency_unit_fails() {
        let units = map(vec![with_requires(
            make_unit("b.service"),
            &["ghost.service"],
        )]);
        let err = build_plan(
            &units,
            &HashMap::new(),
            &HashMap::new(),
            "b.service",
            Start,
            Replace,
        )
        .unwrap_err();
        assert!(matches!(err, PlanError::UnitNotFound(u) if u == "ghost.service"));
    }

    // ------------------------------------------------------------------
    // Modes
    // ------------------------------------------------------------------

    #[test]
    fn ignore_requirements_skips_pull_ins() {
        let units = map(vec![
            make_unit("a.service"),
            with_requires(make_unit("b.service"), &["a.service"]),
        ]);
        let s = steps(
            &units,
            &HashMap::new(),
            &HashMap::new(),
            "b.service",
            Start,
            IgnoreRequirements,
        );
        assert_eq!(names(&s), vec!["b.service"]);
    }

    #[test]
    fn ignore_dependencies_skips_ordering_cycle() {
        let units = map(vec![
            with_after(make_unit("a.service"), &["b.service"]),
            with_after(make_unit("b.service"), &["a.service"]),
        ]);
        let s = steps(
            &units,
            &HashMap::new(),
            &HashMap::new(),
            "a.service",
            Start,
            IgnoreDependencies,
        );
        assert_eq!(names(&s), vec!["a.service"]);
        assert!(s[0].ignore_order);
    }

    #[test]
    fn replace_mode_allows_stopping_running_unit() {
        let units = map(vec![make_unit("a.service")]);
        let mut states = HashMap::new();
        states.insert("a.service".to_string(), Active);
        let s = steps(&units, &states, &HashMap::new(), "a.service", Stop, Replace);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].job_type, Stop);
    }

    #[test]
    fn lenient_mode_rejects_stopping_running_unit() {
        let units = map(vec![make_unit("a.service")]);
        let mut states = HashMap::new();
        states.insert("a.service".to_string(), Active);
        let err =
            build_plan(&units, &states, &HashMap::new(), "a.service", Stop, Lenient).unwrap_err();
        assert_eq!(err, PlanError::Destructive);
    }

    #[test]
    fn fail_mode_rejects_transaction_changing_existing_job() {
        let units = map(vec![make_unit("a.service")]);
        let mut installed = HashMap::new();
        installed.insert("a.service".to_string(), Stop);
        let err = build_plan(
            &units,
            &HashMap::new(),
            &installed,
            "a.service",
            Start,
            Fail,
        )
        .unwrap_err();
        assert_eq!(err, PlanError::Destructive);
    }

    #[test]
    fn replace_mode_replaces_existing_job() {
        let units = map(vec![make_unit("a.service")]);
        let mut installed = HashMap::new();
        installed.insert("a.service".to_string(), Stop);
        let s = steps(
            &units,
            &HashMap::new(),
            &installed,
            "a.service",
            Start,
            Replace,
        );
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].job_type, Start);
    }

    #[test]
    fn isolate_stops_unrelated_active_units() {
        let units = map(vec![
            make_unit("a.service"),
            make_unit("x.service"),
            make_unit("y.service"),
        ]);
        let mut states = HashMap::new();
        states.insert("x.service".to_string(), Active);
        states.insert("y.service".to_string(), Inactive);
        let s = steps(
            &units,
            &states,
            &HashMap::new(),
            "a.service",
            Start,
            Isolate,
        );
        let names: Vec<&str> = names(&s);
        assert!(names.contains(&"a.service"));
        assert!(names.contains(&"x.service"));
        assert!(
            !names.contains(&"y.service"),
            "inactive unit without job stays"
        );
        let x = s.iter().find(|p| p.unit == "x.service").unwrap();
        assert_eq!(x.job_type, Stop);
        assert!(x.matters_to_anchor);
    }

    #[test]
    fn isolate_stops_inactive_units_with_installed_jobs() {
        let units = map(vec![make_unit("a.service"), make_unit("x.service")]);
        let mut installed = HashMap::new();
        installed.insert("x.service".to_string(), Start);
        let s = steps(
            &units,
            &HashMap::new(),
            &installed,
            "a.service",
            Start,
            Isolate,
        );
        assert!(names(&s).contains(&"x.service"));
    }

    // ------------------------------------------------------------------
    // Redundancy
    // ------------------------------------------------------------------

    #[test]
    fn stop_of_inactive_unit_is_kept_as_anchor() {
        let units = map(vec![make_unit("a.service")]);
        let mut states = HashMap::new();
        states.insert("a.service".to_string(), Inactive);
        let s = steps(&units, &states, &HashMap::new(), "a.service", Stop, Replace);
        assert_eq!(names(&s), vec!["a.service"]);
        assert!(s[0].anchor, "anchors are never dropped as redundant");
    }

    #[test]
    fn redundant_wants_dependency_is_dropped() {
        let units = map(vec![
            make_unit("a.service"),
            with_wants(make_unit("b.service"), &["a.service"]),
        ]);
        let mut states = HashMap::new();
        states.insert("a.service".to_string(), Active);
        let s = steps(
            &units,
            &states,
            &HashMap::new(),
            "b.service",
            Start,
            Replace,
        );
        assert_eq!(names(&s), vec!["b.service"]);
    }

    #[test]
    fn redundant_verify_active_dependency_is_dropped() {
        let units = map(vec![
            make_unit("a.service"),
            with_requisite(make_unit("b.service"), &["a.service"]),
        ]);
        let mut states = HashMap::new();
        states.insert("a.service".to_string(), Active);
        let s = steps(
            &units,
            &states,
            &HashMap::new(),
            "b.service",
            Start,
            Replace,
        );
        assert_eq!(names(&s), vec!["b.service"]);
    }

    #[test]
    fn start_dependency_kept_when_state_unknown() {
        let units = map(vec![
            make_unit("a.service"),
            with_requires(make_unit("b.service"), &["a.service"]),
        ]);
        // Unknown state never marks a job redundant.
        let s = steps(
            &units,
            &HashMap::new(),
            &HashMap::new(),
            "b.service",
            Start,
            Replace,
        );
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn root_nop_yields_single_nop_step() {
        let units = map(vec![make_unit("a.service")]);
        let s = steps(
            &units,
            &HashMap::new(),
            &HashMap::new(),
            "a.service",
            Nop,
            Replace,
        );
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].job_type, Nop);
        assert!(s[0].anchor);
    }

    // ------------------------------------------------------------------
    // Merging
    // ------------------------------------------------------------------

    #[test]
    fn duplicate_pull_ins_merge_to_single_job() {
        let units = map(vec![
            make_unit("a.service"),
            with_requires(
                with_wants(make_unit("b.service"), &["a.service"]),
                &["a.service"],
            ),
        ]);
        let s = steps(
            &units,
            &HashMap::new(),
            &HashMap::new(),
            "b.service",
            Start,
            Replace,
        );
        let a = s.iter().filter(|x| x.unit == "a.service").count();
        assert_eq!(a, 1);
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn requisite_merges_into_start() {
        let units = map(vec![
            make_unit("a.service"),
            with_requisite(
                with_requires(make_unit("b.service"), &["a.service"]),
                &["a.service"],
            ),
        ]);
        let s = steps(
            &units,
            &HashMap::new(),
            &HashMap::new(),
            "b.service",
            Start,
            Replace,
        );
        let a = s.iter().find(|x| x.unit == "a.service").unwrap();
        assert_eq!(a.job_type, Start, "Start + VerifyActive merges into Start");
    }

    #[test]
    fn conflicting_requires_and_conflicts_fail() {
        let units = map(vec![
            make_unit("a.service"),
            with_conflicts(
                with_requires(make_unit("b.service"), &["a.service"]),
                &["a.service"],
            ),
        ]);
        // Start and Stop for the same unit, both mattering: unfixable.
        let err = build_plan(
            &units,
            &HashMap::new(),
            &HashMap::new(),
            "b.service",
            Start,
            Replace,
        )
        .unwrap_err();
        assert_eq!(err, PlanError::Conflicting);
    }

    // ------------------------------------------------------------------
    // Ordering and cycles
    // ------------------------------------------------------------------

    #[test]
    fn plan_respects_ordering_edges() {
        let units = map(vec![
            make_unit("a.service"),
            with_after(
                with_requires(make_unit("b.service"), &["a.service"]),
                &["a.service"],
            ),
            with_after(
                with_requires(make_unit("c.service"), &["a.service", "b.service"]),
                &["b.service"],
            ),
        ]);
        let s = steps(
            &units,
            &HashMap::new(),
            &HashMap::new(),
            "c.service",
            Start,
            Replace,
        );
        let pos = |n: &str| s.iter().position(|x| x.unit == n).unwrap();
        // Requires= pulls units in but does not order them; only the
        // After= edges (b After= a, c After= b) create the wait chain.
        assert!(pos("a.service") < pos("b.service"));
        assert!(pos("b.service") < pos("c.service"));
    }

    #[test]
    fn socket_units_start_before_dependent_service() {
        let units = map(vec![
            make_unit("svc.socket"),
            with_requires(make_unit("svc.service"), &["svc.socket"]),
            with_after(
                with_requires(make_unit("root.service"), &["svc.service"]),
                &["svc.service"],
            ),
        ]);
        let s = steps(
            &units,
            &HashMap::new(),
            &HashMap::new(),
            "root.service",
            Start,
            Replace,
        );
        let pos = |n: &str| s.iter().position(|x| x.unit == n).unwrap();
        // Requires= does not order, but the socket must be bound before the
        // service spawns (System S requests listener fds at spawn time).
        assert!(pos("svc.socket") < pos("svc.service"));
        assert!(pos("svc.service") < pos("root.service"));
    }

    #[test]
    fn binds_to_socket_orders_socket_before_service() {
        let units = map(vec![
            make_unit("svc.socket"),
            with_binds_to(make_unit("svc.service"), &["svc.socket"]),
        ]);
        let s = steps(
            &units,
            &HashMap::new(),
            &HashMap::new(),
            "svc.service",
            Start,
            Replace,
        );
        let pos = |n: &str| s.iter().position(|x| x.unit == n).unwrap();
        assert!(pos("svc.socket") < pos("svc.service"));
    }

    #[test]
    fn after_socket_also_orders_after_socket_service() {
        // A unit ordered `After=` a socket (e.g. systemd-udev-trigger.service
        // `After=systemd-udevd-kernel.socket`) must also run after the
        // socket's activated service is started, so the service (udevd) is
        // already bound to its netlink socket and ready to consume the
        // events the dependent unit produces (coldplug -> kmod).
        // Explicit [Socket] Service= (as in systemd-udevd-kernel.socket ->
        // Service=systemd-udevd.service): ordering after the socket must also
        // order after that resolved service, so udevd is bound to its netlink
        // socket before udevadm trigger cold-plugs devices.
        let units = map(vec![
            with_socket_service(make_unit("udevd-kernel.socket"), "udevd.service"),
            make_unit("udevd.service"),
            with_after(
                with_requires(
                    make_unit("udev-trigger.service"),
                    &["udevd-kernel.socket", "udevd.service"],
                ),
                &["udevd-kernel.socket"],
            ),
        ]);
        let s = steps(
            &units,
            &HashMap::new(),
            &HashMap::new(),
            "udev-trigger.service",
            Start,
            Replace,
        );
        let pos = |n: &str| s.iter().position(|x| x.unit == n).unwrap();
        assert!(pos("udevd.service") < pos("udev-trigger.service"));
        assert!(pos("udevd-kernel.socket") < pos("udevd.service"));

        // Convention-based resolution ("foo.socket" -> "foo.service").
        let units = map(vec![
            make_unit("trig.socket"),
            make_unit("trig.service"),
            with_after(
                with_requires(make_unit("svc.service"), &["trig.socket", "trig.service"]),
                &["trig.socket"],
            ),
        ]);
        let s = steps(
            &units,
            &HashMap::new(),
            &HashMap::new(),
            "svc.service",
            Start,
            Replace,
        );
        let pos = |n: &str| s.iter().position(|x| x.unit == n).unwrap();
        assert!(pos("trig.service") < pos("svc.service"));
    }

    #[test]
    fn after_socket_resolves_explicit_service_in_after_deps() {
        // Direct check of after_deps(): a unit ordered After= a socket whose
        // [Socket] Service= is explicit must also depend on that service.
        let units = map(vec![
            with_socket_service(make_unit("udevd-kernel.socket"), "udevd.service"),
            make_unit("udevd.service"),
            with_after(make_unit("udev-trigger.service"), &["udevd-kernel.socket"]),
        ]);
        let rev = ReverseIndex::build(&units);
        let after = rev.after_deps(&units, "udev-trigger.service");
        assert!(after.contains(&"udevd-kernel.socket".to_string()));
        // Explicit Service= resolution: udevd-kernel.socket -> udevd.service.
        assert!(after.contains(&"udevd.service".to_string()));
    }

    #[test]
    fn after_socket_resolves_convention_service_in_after_deps() {
        // Without an explicit [Socket] Service=, the "foo.socket" ->
        // "foo.service" convention applies.
        let units = map(vec![
            make_unit("trig.socket"),
            make_unit("trig.service"),
            with_after(make_unit("svc.service"), &["trig.socket"]),
        ]);
        let rev = ReverseIndex::build(&units);
        let after = rev.after_deps(&units, "svc.service");
        assert!(after.contains(&"trig.socket".to_string()));
        assert!(after.contains(&"trig.service".to_string()));
    }

    #[test]
    fn sockets_directive_socket_also_resolves_to_activated_service() {
        // A socket pulled in via [Service] Sockets= is an ordering target;
        // the resolution step must also order after that socket's activated
        // service (the service is started to consume events).
        let units = map(vec![
            with_socket_service(make_unit("kernel.socket"), "udevd.service"),
            with_sockets(make_unit("udevd.service"), &["kernel.socket"]),
            with_after(make_unit("udev-trigger.service"), &["kernel.socket"]),
        ]);
        let rev = ReverseIndex::build(&units);
        let after = rev.after_deps(&units, "udev-trigger.service");
        assert!(after.contains(&"kernel.socket".to_string()));
        // kernel.socket -> Service=udevd.service, so trigger must also wait
        // for udevd.service to be started.
        assert!(after.contains(&"udevd.service".to_string()));
    }

    #[test]
    fn after_plain_service_is_not_treated_as_socket() {
        // The resolution only kicks in for `.socket` targets; a plain
        // `After=some.service` is left exactly as declared.
        let units = map(vec![
            make_unit("plain.service"),
            with_after(make_unit("svc.service"), &["plain.service"]),
        ]);
        let rev = ReverseIndex::build(&units);
        let after = rev.after_deps(&units, "svc.service");
        assert!(after.contains(&"plain.service".to_string()));
        assert_eq!(after.len(), 1, "no extra deps beyond the declared After=");
    }

    #[test]
    fn sockets_directive_orders_trigger_after_udevd() {
        // End-to-end mirror of the real boot relationship:
        //   systemd-udevd.service          Sockets=udevd-control.socket udevd-kernel.socket
        //   systemd-udevd-kernel.socket    [Socket] Service=systemd-udevd.service
        //   systemd-udev-trigger.service   After=udevd-control.socket udevd-kernel.socket
        // The trigger must start after udevd is ready so its coldplug uevent
        // (MODALIAS -> kmod bochs) is not dropped.
        let units = map(vec![
            with_wants(
                with_after(
                    make_unit("udev-trigger.service"),
                    &["udevd-control.socket", "udevd-kernel.socket"],
                ),
                &["udevd.service"],
            ),
            with_socket_service(make_unit("udevd-kernel.socket"), "udevd.service"),
            make_unit("udevd-control.socket"),
            with_sockets(
                make_unit("udevd.service"),
                &["udevd-control.socket", "udevd-kernel.socket"],
            ),
        ]);
        let s = steps(
            &units,
            &HashMap::new(),
            &HashMap::new(),
            "udev-trigger.service",
            Start,
            Replace,
        );
        let pos = |n: &str| s.iter().position(|x| x.unit == n).unwrap();
        // Both sockets, and hence udevd, must start before the trigger.
        assert!(pos("udevd-control.socket") < pos("udev-trigger.service"));
        assert!(pos("udevd-kernel.socket") < pos("udev-trigger.service"));
        assert!(pos("udevd.service") < pos("udev-trigger.service"));
    }

    #[test]
    fn before_edges_are_normalized_into_after() {
        let units = map(vec![
            make_unit("a.service"),
            make_unit("b.service"),
            make_unit("c.service"),
            with_requires(make_unit("root.service"), &["a.service", "b.service"]),
            with_before(make_unit("a.service"), &["b.service"]),
        ]);
        let s = steps(
            &units,
            &HashMap::new(),
            &HashMap::new(),
            "root.service",
            Start,
            Replace,
        );
        let pos = |n: &str| s.iter().position(|x| x.unit == n).unwrap();
        // a Before= b means b must start after a.
        assert!(pos("a.service") < pos("b.service"));
    }

    #[test]
    fn restart_job_does_not_wait_for_after_dependency() {
        let units = map(vec![
            make_unit("a.service"),
            with_after(
                with_requires(make_unit("b.service"), &["a.service"]),
                &["a.service"],
            ),
        ]);
        let s = steps(
            &units,
            &HashMap::new(),
            &HashMap::new(),
            "b.service",
            Restart,
            Replace,
        );
        // job_compare(b=Restart, a=Start, AFTER) < 0: a restart (stop phase)
        // never waits for its After= dependencies. Both jobs are runnable
        // immediately and the serialization emits them in unit order.
        let a = s.iter().find(|x| x.unit == "a.service").unwrap();
        let b = s.iter().find(|x| x.unit == "b.service").unwrap();
        assert_eq!(a.job_type, Start);
        assert_eq!(b.job_type, Restart);
        assert!(b.anchor);
        assert!(!a.anchor);
    }

    #[test]
    fn stop_job_runs_first_when_ordered_after() {
        let units = map(vec![
            make_unit("a.service"),
            make_unit("c.service"),
            with_after(
                with_conflicts(make_unit("b.service"), &["c.service"]),
                &["c.service"],
            ),
        ]);
        let s = steps(
            &units,
            &HashMap::new(),
            &HashMap::new(),
            "b.service",
            Start,
            Replace,
        );
        let pos = |n: &str| s.iter().position(|x| x.unit == n).unwrap();
        // c gets a Stop (conflicts); b has After=c, so the stop runs first.
        let c = s.iter().find(|x| x.unit == "c.service").unwrap();
        assert_eq!(c.job_type, Stop);
        assert!(pos("c.service") < pos("b.service"));
    }

    #[test]
    fn ordering_cycle_broken_by_deleting_non_mattering_job() {
        let units = map(vec![
            with_wants(
                with_requires(make_unit("a.service"), &["c.service"]),
                &["b.service"],
            ),
            make_unit("b.service"),
            make_unit("c.service"),
            with_after(make_unit("b.service"), &["c.service"]),
            with_after(make_unit("c.service"), &["b.service"]),
        ]);
        // root a: requires c (matters); wants b (does not matter).
        // b After= c and c After= b form a cycle; b does not matter → deleted.
        let s = steps(
            &units,
            &HashMap::new(),
            &HashMap::new(),
            "a.service",
            Start,
            Replace,
        );
        let names: Vec<&str> = names(&s);
        assert!(names.contains(&"a.service"));
        assert!(names.contains(&"c.service"));
        assert!(!names.contains(&"b.service"));
    }

    #[test]
    fn ordering_cycle_with_all_mattering_jobs_fails() {
        let units = map(vec![
            with_requires(make_unit("a.service"), &["b.service"]),
            with_after(
                with_requires(make_unit("b.service"), &["c.service"]),
                &["c.service"],
            ),
            with_after(make_unit("c.service"), &["b.service"]),
        ]);
        // a requires b; b requires c. b After= c, c After= b: cycle b↔c,
        // both mattering → Cyclic.
        let err = build_plan(
            &units,
            &HashMap::new(),
            &HashMap::new(),
            "a.service",
            Start,
            Replace,
        )
        .unwrap_err();
        assert_eq!(err, PlanError::Cyclic);
    }

    #[test]
    fn after_cycle_detected_when_both_units_have_jobs() {
        // a requires b pulls b into the plan; the After= pair then forms an
        // unfixable ordering cycle (both jobs matter).
        let units = map(vec![
            with_requires(
                with_after(make_unit("a.service"), &["b.service"]),
                &["b.service"],
            ),
            with_after(make_unit("b.service"), &["a.service"]),
        ]);
        let err = build_plan(
            &units,
            &HashMap::new(),
            &HashMap::new(),
            "a.service",
            Start,
            Replace,
        )
        .unwrap_err();
        assert_eq!(err, PlanError::Cyclic);
    }

    #[test]
    fn after_cycle_with_single_job_is_not_a_cycle() {
        // a After= b + b After= a with only a in the transaction: the
        // ordering graph has no second vertex, so there is no cycle
        // (mirrors systemd: `o = hashmap_get(tr->jobs, u)` finds no job).
        let units = map(vec![
            with_after(make_unit("a.service"), &["b.service"]),
            with_after(make_unit("b.service"), &["a.service"]),
        ]);
        let s = steps(
            &units,
            &HashMap::new(),
            &HashMap::new(),
            "a.service",
            Start,
            Replace,
        );
        assert_eq!(names(&s), vec!["a.service"]);
    }

    // ------------------------------------------------------------------
    // PlannerMode mapping
    // ------------------------------------------------------------------

    #[test]
    fn planner_mode_from_job_mode() {
        use crate::state::JobMode;
        assert_eq!(PlannerMode::from_job_mode(JobMode::Replace), Replace);
        assert_eq!(PlannerMode::from_job_mode(JobMode::Fail), Fail);
        assert_eq!(PlannerMode::from_job_mode(JobMode::Isolate), Isolate);
        assert_eq!(PlannerMode::from_job_mode(JobMode::Flush), Flush);
        assert_eq!(PlannerMode::from_job_mode(JobMode::Queue), Replace);
        assert_eq!(
            PlannerMode::from_job_mode(JobMode::IgnoreDependencies),
            IgnoreDependencies
        );
        assert_eq!(
            PlannerMode::from_job_mode(JobMode::IgnoreRequirements),
            IgnoreRequirements
        );
        assert_eq!(PlannerMode::from_job_mode(JobMode::Lenient), Lenient);
        assert_eq!(
            PlannerMode::from_job_mode(JobMode::ReplaceIrreversibly),
            ReplaceIrreversibly
        );
        assert_eq!(PlannerMode::from_job_mode(JobMode::Triggering), Triggering);
        assert_eq!(
            PlannerMode::from_job_mode(JobMode::RestartDependencies),
            RestartDependencies
        );
    }

    #[test]
    fn plan_error_display() {
        assert_eq!(PlanError::Cyclic.to_string(), "Transaction order is cyclic");
        assert_eq!(
            PlanError::Conflicting.to_string(),
            "Transaction contains conflicting jobs"
        );
        assert_eq!(
            PlanError::Destructive.to_string(),
            "Transaction is destructive"
        );
        assert_eq!(
            PlanError::UnitNotFound("x.service".into()).to_string(),
            "Unit x.service is not loaded"
        );
    }

    // ------------------------------------------------------------------
    // Multi-anchor (EnqueueUnitJobMany) tests
    // ------------------------------------------------------------------

    /// The core regression for the /run/nologin bug: when two anchors are
    /// planned together, a redundant-but-dependent unit is NOT dropped
    /// (it is an anchor or has a non-redundant job), and ordering is
    /// preserved between all roots.
    #[test]
    fn multi_anchor_orders_after_before() {
        // a.service has Before=b.service
        // b.service has Requires=c.service, After=c.service
        // c.service is the "sysinit" equivalent
        let units = map(vec![
            with_before(make_unit("a.service"), &["b.service"]),
            with_requires(
                with_after(make_unit("b.service"), &["c.service"]),
                &["c.service"],
            ),
            make_unit("c.service"),
        ]);
        let s = steps_multi(
            &units,
            &HashMap::new(),
            &HashMap::new(),
            &[("a.service", Start), ("b.service", Start)],
            Replace,
        );
        let n = names(&s);
        // c must come before b (Requires + After), a must come before b (Before)
        assert!(
            n.iter().position(|&x| x == "c.service").unwrap()
                < n.iter().position(|&x| x == "b.service").unwrap(),
            "c must start before b: {:?}",
            n
        );
        assert!(
            n.iter().position(|&x| x == "a.service").unwrap()
                < n.iter().position(|&x| x == "b.service").unwrap(),
            "a must start before b: {:?}",
            n
        );
        // Both anchors should be present
        assert!(s.iter().any(|s| s.unit == "a.service" && s.anchor));
        assert!(s.iter().any(|s| s.unit == "b.service" && s.anchor));
    }

    /// When a root unit is already active (redundant Start), it is still
    /// kept because it is an anchor — matching systemd's behaviour that
    /// anchor jobs are never dropped.
    #[test]
    fn multi_anchor_active_root_kept_as_anchor() {
        let units = map(vec![
            with_requires(make_unit("b.service"), &["c.service"]),
            make_unit("c.service"),
        ]);
        let mut states = HashMap::new();
        states.insert("c.service".into(), UnitActiveState::Active);
        let s = steps_multi(
            &units,
            &states,
            &HashMap::new(),
            &[("b.service", Start)],
            Replace,
        );
        // b is the only root; c is a dependency but already active.
        // c should be dropped (redundant, not anchor).
        assert_eq!(names(&s), vec!["b.service"]);
    }

    /// Reproduction of the /run/nologin boot race seen on the VM
    /// (2026-08-25): `systemd-tmpfiles-setup.service` (`f+! /run/nologin`)
    /// and `systemd-user-sessions.service` (`rm -f /run/nologin`) were both
    /// planned as anchors of the flat start_units transaction.  Without the
    /// DefaultDependencies injection (After=sysinit.target) there is no
    /// ordering edge between them, so the planner may run them concurrently
    /// and tmpfiles can re-create /run/nologin after user-sessions removed
    /// it.  With the injection (as real systemd guarantees) sysinit.target
    /// is pulled in and orders tmpfiles-setup before user-sessions.
    #[test]
    fn nologin_race_requires_default_dependencies() {
        // Real unit graph on the VM rootfs.
        let tmpfiles = with_after(
            with_before(
                make_unit("systemd-tmpfiles-setup.service"),
                &["sysinit.target"],
            ),
            &[
                "local-fs.target",
                "systemd-sysusers.service",
                "systemd-journald.service",
            ],
        );
        let user_sessions = with_after(
            make_unit("systemd-user-sessions.service"),
            &[
                "remote-fs.target",
                "nss-user-lookup.target",
                "network.target",
                "home.mount",
            ],
        );
        let sysinit = make_unit("sysinit.target");
        let remote_fs = make_unit("remote-fs.target");

        // Without injection: no edge between the two anchors -> no ordering
        // guarantee (this mirrors what the VM actually ran).
        let units = map(vec![
            tmpfiles.clone(),
            user_sessions.clone(),
            sysinit.clone(),
            remote_fs,
        ]);
        let s = steps_multi(
            &units,
            &HashMap::new(),
            &HashMap::new(),
            &[
                ("systemd-tmpfiles-setup.service", Start),
                ("systemd-user-sessions.service", Start),
            ],
            Replace,
        );
        assert_eq!(
            names(&s).len(),
            2,
            "no pull-in without Requires: {:?}",
            names(&s)
        );

        // With injection: user-sessions gains Requires+After=sysinit.target,
        // tmpfiles keeps Before=sysinit.target -> strict ordering via sysinit.
        let injected = with_requires(
            with_after(user_sessions, &["sysinit.target"]),
            &["sysinit.target"],
        );
        let units = map(vec![
            tmpfiles,
            injected,
            sysinit,
            make_unit("local-fs.target"),
            make_unit("systemd-journald.service"),
        ]);
        let s = steps_multi(
            &units,
            &HashMap::new(),
            &HashMap::new(),
            &[
                ("systemd-tmpfiles-setup.service", Start),
                ("systemd-user-sessions.service", Start),
            ],
            Replace,
        );
        let n = names(&s);
        assert!(n.contains(&"sysinit.target"), "sysinit pulled in: {:?}", n);
        let pos = |u: &str| {
            n.iter()
                .position(|&x| x == u)
                .unwrap_or_else(|| panic!("{u} not in plan {n:?}"))
        };
        assert!(
            pos("systemd-tmpfiles-setup.service") < pos("systemd-user-sessions.service"),
            "tmpfiles must complete before user-sessions: {n:?}",
        );
    }

    /// Two roots that share a dependency: the dependency appears once and
    /// is ordered before both roots via After=.
    #[test]
    fn multi_anchor_shared_dependency() {
        let units = map(vec![
            with_after(
                with_requires(make_unit("a.service"), &["shared.service"]),
                &["shared.service"],
            ),
            with_after(
                with_requires(make_unit("b.service"), &["shared.service"]),
                &["shared.service"],
            ),
            make_unit("shared.service"),
        ]);
        let s = steps_multi(
            &units,
            &HashMap::new(),
            &HashMap::new(),
            &[("a.service", Start), ("b.service", Start)],
            Replace,
        );
        let n = names(&s);
        assert_eq!(n.len(), 3, "should have 3 units: {:?}", n);
        assert!(n.contains(&"shared.service"));
        assert!(n.contains(&"a.service"));
        assert!(n.contains(&"b.service"));
        // shared must come before both a and b
        let shared_pos = n.iter().position(|&x| x == "shared.service").unwrap();
        let a_pos = n.iter().position(|&x| x == "a.service").unwrap();
        let b_pos = n.iter().position(|&x| x == "b.service").unwrap();
        assert!(shared_pos < a_pos, "shared before a: {:?}", n);
        assert!(shared_pos < b_pos, "shared before b: {:?}", n);
    }

    // ------------------------------------------------------------------
    // Sockets= (socket activation)
    // ------------------------------------------------------------------

    /// Sockets= should order socket units before the dependent service,
    /// even when Requires=/BindsTo= does not list them.
    #[test]
    fn sockets_directive_orders_socket_before_service() {
        let units = map(vec![
            make_unit("control.socket"),
            make_unit("kernel.socket"),
            with_sockets(
                make_unit("udevd.service"),
                &["control.socket", "kernel.socket"],
            ),
        ]);
        let s = steps(
            &units,
            &HashMap::new(),
            &HashMap::new(),
            "udevd.service",
            Start,
            Replace,
        );
        let pos = |n: &str| s.iter().position(|x| x.unit == n).unwrap();
        assert!(
            pos("control.socket") < pos("udevd.service"),
            "control.socket must start before udevd.service"
        );
        assert!(
            pos("kernel.socket") < pos("udevd.service"),
            "kernel.socket must start before udevd.service"
        );
    }

    /// Sockets= pulls in socket units via Wants=-like semantics (soft
    /// dependency): the socket units appear in the plan when starting
    /// the service.
    #[test]
    fn sockets_directive_pulls_in_socket_units() {
        let units = map(vec![
            make_unit("my.socket"),
            with_sockets(make_unit("my.service"), &["my.socket"]),
        ]);
        let s = steps(
            &units,
            &HashMap::new(),
            &HashMap::new(),
            "my.service",
            Start,
            Replace,
        );
        let n = names(&s);
        assert!(
            n.contains(&"my.socket"),
            "my.socket should be pulled in: {:?}",
            n
        );
        assert!(
            n.contains(&"my.service"),
            "my.service should be in plan: {:?}",
            n
        );
    }

    /// Sockets= and Requires= for the same socket unit merge into a single
    /// plan step (no duplicates).
    #[test]
    fn sockets_and_requires_merge_to_single_step() {
        let units = map(vec![
            make_unit("svc.socket"),
            with_requires(
                with_sockets(make_unit("svc.service"), &["svc.socket"]),
                &["svc.socket"],
            ),
        ]);
        let s = steps(
            &units,
            &HashMap::new(),
            &HashMap::new(),
            "svc.service",
            Start,
            Replace,
        );
        let socket_count = s.iter().filter(|x| x.unit == "svc.socket").count();
        assert_eq!(
            socket_count, 1,
            "svc.socket should appear once, got {}",
            socket_count
        );
    }

    /// Sockets= with After= ordering: socket units from both Requires= and
    /// Sockets= are ordered before the service.
    #[test]
    fn sockets_directive_respects_after_ordering() {
        let units = map(vec![
            make_unit("control.socket"),
            make_unit("kernel.socket"),
            with_after(
                with_sockets(
                    make_unit("udevd.service"),
                    &["control.socket", "kernel.socket"],
                ),
                &["control.socket", "kernel.socket"],
            ),
            with_after(
                with_requires(make_unit("trigger.service"), &["udevd.service"]),
                &["udevd.service"],
            ),
        ]);
        let s = steps(
            &units,
            &HashMap::new(),
            &HashMap::new(),
            "trigger.service",
            Start,
            Replace,
        );
        let pos = |n: &str| s.iter().position(|x| x.unit == n).unwrap();
        assert!(pos("control.socket") < pos("udevd.service"));
        assert!(pos("kernel.socket") < pos("udevd.service"));
        assert!(pos("udevd.service") < pos("trigger.service"));
    }
}
