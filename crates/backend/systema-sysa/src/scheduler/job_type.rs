//! systemd-compatible job type model.
//!
//! A faithful Rust port of the job type machinery in systemd's
//! `src/core/job.h` and `src/core/job.c`:
//!
//! - [`JobType`] mirrors systemd's `JobType` enum (including transient
//!   types that never enter a transaction).
//! - [`job_type_lookup_merge`] implements the `job_merging_table[]` from
//!   `job.c` (the lower triangle of the commutative merge matrix).
//! - [`job_type_collapse`] resolves the state-dependent transient types
//!   (`JOB_TRY_RESTART`, `JOB_TRY_RELOAD`, `JOB_RELOAD_OR_START`) by
//!   observing the unit's current active state.
//! - [`job_type_is_redundant`] implements the `JOB_*` redundancy rules
//!   (`job.c`), i.e. whether an operation is a no-op given the unit state.
//!
//! systema's runtime state cache (`AllocatorState::unit_states`) may not
//! know a unit's state. [`UnitActiveState::Unknown`] models this: following
//! the project's conservative policy, an unknown state never folds a job
//! away — it always keeps the operation.

use crate::state::JobKind;

/// Classification of a unit's current active state, mirroring systemd's
/// `UnitActiveState` plus an `Unknown` case for state not present in the
/// runtime cache.
///
/// The variant *order* intentionally matches systemd's `JobType` enum
/// ordering constraints (see [`JobType`]) so that `PartialOrd` derived here
/// has no semantic meaning beyond the job model — keep the two in sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum UnitActiveState {
    Active,
    Reloading,
    Inactive,
    Failed,
    Activating,
    Deactivating,
    Maintenance,
    Refreshing,
    /// No state is cached for the unit. systemd always knows the state;
    /// systema treats this as "cannot prove anything" (conservative).
    Unknown,
}

impl UnitActiveState {
    /// Map the `active_state` string reported by workers (systemd
    /// `ActiveState` values, plus `dead` which workers emit as a sub-state)
    /// onto the classification. Anything unrecognised or empty is `Unknown`.
    pub fn from_active_state_str(s: &str) -> UnitActiveState {
        match s {
            "active" => UnitActiveState::Active,
            "reloading" => UnitActiveState::Reloading,
            "refreshing" => UnitActiveState::Refreshing,
            "activating" => UnitActiveState::Activating,
            "deactivating" => UnitActiveState::Deactivating,
            "maintenance" => UnitActiveState::Maintenance,
            "inactive" | "dead" => UnitActiveState::Inactive,
            "failed" => UnitActiveState::Failed,
            _ => UnitActiveState::Unknown,
        }
    }

    /// `UNIT_IS_ACTIVE_OR_RELOADING()` — `active`, `reloading`,
    /// `refreshing`. An `Unknown` state is never active-like.
    pub fn is_active_or_reloading(self) -> bool {
        matches!(
            self,
            UnitActiveState::Active | UnitActiveState::Reloading | UnitActiveState::Refreshing
        )
    }

    /// `UNIT_IS_ACTIVE_OR_ACTIVATING()` — `active`, `activating`,
    /// `reloading`, `refreshing`. An `Unknown` state is never active-like.
    pub fn is_active_or_activating(self) -> bool {
        matches!(
            self,
            UnitActiveState::Active
                | UnitActiveState::Activating
                | UnitActiveState::Reloading
                | UnitActiveState::Refreshing
        )
    }

    /// `UNIT_IS_INACTIVE_OR_FAILED()` — `inactive`, `failed`. An `Unknown`
    /// state is never inactive-like.
    pub fn is_inactive_or_failed(self) -> bool {
        matches!(self, UnitActiveState::Inactive | UnitActiveState::Failed)
    }
}

/// Mirrors systemd's `JobType` enum (`src/core/job.h`).
///
/// The variant order is significant: it reproduces systemd's declaration
/// order, which `job_merging_table[]` is indexed by. Keep the order in sync
/// with the reference implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum JobType {
    /// Start the unit if it is not running; wait for it to become active.
    Start,
    /// Do nothing if the unit is active; otherwise fail the transaction.
    VerifyActive,
    /// Stop the unit.
    Stop,
    /// Reload the unit if it is running.
    Reload,
    /// Stop the unit if it is running, then start it unconditionally.
    Restart,
    /// Do nothing. May enter a transaction but never pulls in dependencies.
    Nop,
    /// If the unit is running, stop and then start it; otherwise do nothing.
    /// Never enters a transaction — collapses to `Restart` or `Nop`.
    TryRestart,
    /// If the unit is running, reload it; otherwise do nothing.
    /// Never enters a transaction — collapses to `Reload` or `Nop`.
    TryReload,
    /// If the unit is running, reload it; otherwise start it.
    /// Never enters a transaction — collapses to `Reload` or `Start`.
    ReloadOrStart,
}

impl JobType {
    /// Convert a public scheduler [`JobKind`] into the internal job model.
    pub fn from_job_kind(kind: JobKind) -> JobType {
        match kind {
            JobKind::Start => JobType::Start,
            JobKind::Stop => JobType::Stop,
            JobKind::Restart => JobType::Restart,
            JobKind::Reload => JobType::Reload,
            JobKind::Nop => JobType::Nop,
        }
    }

    /// Stable string form, used for diagnostics and control-port job properties.
    pub fn as_str(&self) -> &str {
        match self {
            JobType::Start => "start",
            JobType::VerifyActive => "verify-active",
            JobType::Stop => "stop",
            JobType::Reload => "reload",
            JobType::Restart => "restart",
            JobType::Nop => "nop",
            JobType::TryRestart => "try-restart",
            JobType::TryReload => "try-reload",
            JobType::ReloadOrStart => "reload-or-start",
        }
    }
}

/// Look up the result of merging job types `a` and `b`, implementing the
/// `job_merging_table[]` from `job.c:416-424` (the symmetric merge matrix).
///
/// Returns `None` for unmergeable (conflicting) pairs. Only the five
/// transaction job types participate; `Nop` and the transient types
/// (`TryRestart`, `TryReload`, `ReloadOrStart`) must be handled by callers
/// before this function is reached.
pub fn job_type_lookup_merge(a: JobType, b: JobType) -> Option<JobType> {
    use JobType::*;

    if a == b {
        return Some(a);
    }

    // Normalise so `big >= small` (merging is commutative; systemd stores
    // only the lower triangle of the matrix).
    let (big, small) = if a > b { (a, b) } else { (b, a) };

    match (big, small) {
        (VerifyActive, Start) => Some(Start),
        (Reload, Start) => Some(ReloadOrStart),
        (Reload, VerifyActive) => Some(Reload),
        (Restart, Start) | (Restart, VerifyActive) | (Restart, Reload) => Some(Restart),
        // (Stop, Start|VerifyActive|Reload|Restart) and anything involving
        // Nop or the transient types are unmergeable.
        _ => None,
    }
}

/// Resolve a state-dependent job type into a plain transaction type by
/// observing the unit's current active state (`job.c:482-516`).
///
/// systema deviation for [`UnitActiveState::Unknown`]: systemd always knows
/// the state and would collapse `TRY_RESTART`/`TRY_RELOAD` to `Nop` when the
/// unit is not running. Since systema cannot prove the unit is inactive, it
/// keeps the operation (returns the action type) rather than skipping it.
pub fn job_type_collapse(t: JobType, state: UnitActiveState) -> JobType {
    use JobType::*;

    match t {
        TryRestart => match state {
            UnitActiveState::Unknown => Restart,
            s if s.is_active_or_activating() => Restart,
            _ => Nop,
        },
        TryReload => match state {
            UnitActiveState::Unknown => Reload,
            s if s.is_active_or_reloading() => Reload,
            _ => Nop,
        },
        ReloadOrStart => match state {
            UnitActiveState::Unknown => Start,
            s if s.is_active_or_reloading() => Reload,
            _ => Start,
        },
        other => other,
    }
}

/// Whether a job of type `t` is redundant (a no-op) given the unit's active
/// state (`job.c:443-480`).
///
/// `Reload` and `Restart` are never redundant (restarting an `Activating`
/// unit is still required so it picks up fresh state; reload is meaningful
/// whenever the unit exists). `Nop` is always redundant. `Start`/`VerifyActive`
/// are redundant when the unit is active-like; `Stop` when it is inactive-like.
/// An [`UnitActiveState::Unknown`] state never marks a job redundant.
pub fn job_type_is_redundant(t: JobType, state: UnitActiveState) -> bool {
    use JobType::*;

    match t {
        Start | VerifyActive => state.is_active_or_reloading(),
        Stop => state.is_inactive_or_failed(),
        Reload | Restart => false,
        Nop => true,
        // Transient types must be collapsed before entering a transaction;
        // defensively treat them as not redundant.
        TryRestart | TryReload | ReloadOrStart => false,
    }
}

/// Merge two job types that apply to the same unit within one transaction
/// (`job_type_merge_and_collapse`, `job.c:518-529`).
///
/// Returns the merged (and state-collapsed) type, or `None` when the pair
/// conflicts and the transaction must be resolved by deleting a job. A `Nop`
/// job simply gives way to the other type (systemd stores nop jobs in a
/// dedicated slot and any real job replaces them).
pub fn job_type_merge_and_collapse(
    a: JobType,
    b: JobType,
    state: UnitActiveState,
) -> Option<JobType> {
    use JobType::*;

    let t = match (a, b) {
        (x, y) if x == y => x,
        (Nop, x) | (x, Nop) => x,
        _ => job_type_lookup_merge(a, b)?,
    };
    Some(job_type_collapse(t, state))
}

#[cfg(test)]
mod tests {
    use super::*;

    use JobType::*;
    use UnitActiveState::*;

    // ------------------------------------------------------------------
    // Enum ordering parity with systemd's JobType
    // ------------------------------------------------------------------

    #[test]
    fn enum_order_matches_systemd_job_h() {
        // systemd: JOB_START < JOB_VERIFY_ACTIVE < JOB_STOP < JOB_RELOAD
        //          < JOB_RESTART < JOB_NOP < JOB_TRY_RESTART < JOB_TRY_RELOAD
        //          < JOB_RELOAD_OR_START
        let order = [
            Start,
            VerifyActive,
            Stop,
            Reload,
            Restart,
            Nop,
            TryRestart,
            TryReload,
            ReloadOrStart,
        ];
        for (i, a) in order.iter().enumerate() {
            for (j, b) in order.iter().enumerate() {
                assert_eq!(
                    a.cmp(b),
                    i.cmp(&j),
                    "variant order diverges from systemd's JobType enum"
                );
            }
        }
    }

    // ------------------------------------------------------------------
    // Active state classification
    // ------------------------------------------------------------------

    #[test]
    fn active_state_from_str_mapping() {
        assert_eq!(UnitActiveState::from_active_state_str("active"), Active);
        assert_eq!(
            UnitActiveState::from_active_state_str("reloading"),
            Reloading
        );
        assert_eq!(
            UnitActiveState::from_active_state_str("refreshing"),
            Refreshing
        );
        assert_eq!(
            UnitActiveState::from_active_state_str("activating"),
            Activating
        );
        assert_eq!(
            UnitActiveState::from_active_state_str("deactivating"),
            Deactivating
        );
        assert_eq!(
            UnitActiveState::from_active_state_str("maintenance"),
            Maintenance
        );
        assert_eq!(UnitActiveState::from_active_state_str("inactive"), Inactive);
        // Workers emit "dead" as a sub-state; map it to inactive as well.
        assert_eq!(UnitActiveState::from_active_state_str("dead"), Inactive);
        assert_eq!(UnitActiveState::from_active_state_str("failed"), Failed);
    }

    #[test]
    fn active_state_unknown_for_unrecognised_strings() {
        assert_eq!(UnitActiveState::from_active_state_str(""), Unknown);
        assert_eq!(UnitActiveState::from_active_state_str("garbage"), Unknown);
        assert_eq!(UnitActiveState::from_active_state_str("STARTING"), Unknown);
    }

    #[test]
    fn active_state_classification_sets() {
        for s in [Active, Reloading, Refreshing] {
            assert!(s.is_active_or_reloading());
            assert!(s.is_active_or_activating());
            assert!(!s.is_inactive_or_failed());
        }
        {
            let s = Activating;
            assert!(!s.is_active_or_reloading());
            assert!(s.is_active_or_activating());
        }
        for s in [Inactive, Failed] {
            assert!(!s.is_active_or_reloading());
            assert!(!s.is_active_or_activating());
            assert!(s.is_inactive_or_failed());
        }
        for s in [Deactivating, Maintenance, Unknown] {
            assert!(!s.is_active_or_reloading());
            assert!(!s.is_active_or_activating());
            assert!(!s.is_inactive_or_failed());
        }
    }

    // ------------------------------------------------------------------
    // Merge table (job_merging_table[])
    // ------------------------------------------------------------------

    #[test]
    fn merge_identical_types_are_unchanged() {
        for t in [Start, VerifyActive, Stop, Reload, Restart] {
            assert_eq!(job_type_lookup_merge(t, t), Some(t));
        }
    }

    #[test]
    fn merge_verify_active_into_start() {
        assert_eq!(job_type_lookup_merge(Start, VerifyActive), Some(Start));
        assert_eq!(job_type_lookup_merge(VerifyActive, Start), Some(Start));
    }

    #[test]
    fn merge_start_with_reload_yields_reload_or_start() {
        assert_eq!(job_type_lookup_merge(Start, Reload), Some(ReloadOrStart));
        assert_eq!(job_type_lookup_merge(Reload, Start), Some(ReloadOrStart));
    }

    #[test]
    fn merge_reload_with_verify_active_yields_reload() {
        assert_eq!(job_type_lookup_merge(Reload, VerifyActive), Some(Reload));
        assert_eq!(job_type_lookup_merge(VerifyActive, Reload), Some(Reload));
    }

    #[test]
    fn merge_restart_absorbs_start_verify_and_reload() {
        for other in [Start, VerifyActive, Reload] {
            assert_eq!(job_type_lookup_merge(Restart, other), Some(Restart));
            assert_eq!(job_type_lookup_merge(other, Restart), Some(Restart));
        }
    }

    #[test]
    fn merge_stop_conflicts_with_everything_but_stop() {
        for other in [Start, VerifyActive, Reload, Restart] {
            assert_eq!(job_type_lookup_merge(Stop, other), None);
            assert_eq!(job_type_lookup_merge(other, Stop), None);
        }
    }

    #[test]
    fn merge_table_is_symmetric() {
        let types = [Start, VerifyActive, Stop, Reload, Restart];
        for a in types {
            for b in types {
                assert_eq!(
                    job_type_lookup_merge(a, b),
                    job_type_lookup_merge(b, a),
                    "merge({a:?}, {b:?}) must be commutative"
                );
            }
        }
    }

    #[test]
    fn merge_rejects_nop_and_transient_types() {
        for a in [Start, VerifyActive, Stop, Reload, Restart] {
            for b in [Nop, TryRestart, TryReload, ReloadOrStart] {
                assert_eq!(job_type_lookup_merge(a, b), None);
            }
        }
        for a in [
            Start,
            VerifyActive,
            Stop,
            Reload,
            Restart,
            TryRestart,
            TryReload,
            ReloadOrStart,
        ] {
            for b in [TryRestart, TryReload, ReloadOrStart] {
                if a != b {
                    assert_eq!(job_type_lookup_merge(a, b), None);
                }
            }
        }
        // Nop pairs with itself only through the equality shortcut; the
        // table itself never accepts Nop.
        assert_eq!(job_type_lookup_merge(Nop, Nop), Some(Nop));
    }

    // ------------------------------------------------------------------
    // State-dependent collapse (job_type_collapse)
    // ------------------------------------------------------------------

    #[test]
    fn collapse_try_restart_depends_on_state() {
        assert_eq!(job_type_collapse(TryRestart, Active), Restart);
        assert_eq!(job_type_collapse(TryRestart, Activating), Restart);
        assert_eq!(job_type_collapse(TryRestart, Reloading), Restart);
        assert_eq!(job_type_collapse(TryRestart, Refreshing), Restart);
        assert_eq!(job_type_collapse(TryRestart, Inactive), Nop);
        assert_eq!(job_type_collapse(TryRestart, Failed), Nop);
        assert_eq!(job_type_collapse(TryRestart, Deactivating), Nop);
        assert_eq!(job_type_collapse(TryRestart, Maintenance), Nop);
        // Unknown state keeps the operation (conservative).
        assert_eq!(job_type_collapse(TryRestart, Unknown), Restart);
    }

    #[test]
    fn collapse_try_reload_depends_on_state() {
        assert_eq!(job_type_collapse(TryReload, Active), Reload);
        assert_eq!(job_type_collapse(TryReload, Reloading), Reload);
        assert_eq!(job_type_collapse(TryReload, Refreshing), Reload);
        assert_eq!(job_type_collapse(TryReload, Inactive), Nop);
        assert_eq!(job_type_collapse(TryReload, Failed), Nop);
        assert_eq!(job_type_collapse(TryReload, Activating), Nop);
        assert_eq!(job_type_collapse(TryReload, Deactivating), Nop);
        assert_eq!(job_type_collapse(TryReload, Maintenance), Nop);
        assert_eq!(job_type_collapse(TryReload, Unknown), Reload);
    }

    #[test]
    fn collapse_reload_or_start_depends_on_state() {
        assert_eq!(job_type_collapse(ReloadOrStart, Active), Reload);
        assert_eq!(job_type_collapse(ReloadOrStart, Reloading), Reload);
        assert_eq!(job_type_collapse(ReloadOrStart, Refreshing), Reload);
        assert_eq!(job_type_collapse(ReloadOrStart, Inactive), Start);
        assert_eq!(job_type_collapse(ReloadOrStart, Failed), Start);
        assert_eq!(job_type_collapse(ReloadOrStart, Activating), Start);
        assert_eq!(job_type_collapse(ReloadOrStart, Deactivating), Start);
        assert_eq!(job_type_collapse(ReloadOrStart, Maintenance), Start);
        assert_eq!(job_type_collapse(ReloadOrStart, Unknown), Start);
    }

    #[test]
    fn collapse_leaves_transaction_types_untouched() {
        for t in [Start, VerifyActive, Stop, Reload, Restart, Nop] {
            for s in [
                Active,
                Inactive,
                Failed,
                Activating,
                Deactivating,
                Maintenance,
                Unknown,
            ] {
                assert_eq!(job_type_collapse(t, s), t);
            }
        }
    }

    // ------------------------------------------------------------------
    // Redundancy (job_type_is_redundant)
    // ------------------------------------------------------------------

    #[test]
    fn start_and_verify_are_redundant_when_active() {
        for t in [Start, VerifyActive] {
            for s in [Active, Reloading, Refreshing] {
                assert!(job_type_is_redundant(t, s), "{t:?} redundant in {s:?}");
            }
            for s in [
                Inactive,
                Failed,
                Activating,
                Deactivating,
                Maintenance,
                Unknown,
            ] {
                assert!(!job_type_is_redundant(t, s), "{t:?} not redundant in {s:?}");
            }
        }
    }

    #[test]
    fn stop_is_redundant_when_inactive() {
        assert!(job_type_is_redundant(Stop, Inactive));
        assert!(job_type_is_redundant(Stop, Failed));
        for s in [
            Active,
            Reloading,
            Refreshing,
            Activating,
            Deactivating,
            Maintenance,
            Unknown,
        ] {
            assert!(
                !job_type_is_redundant(Stop, s),
                "Stop not redundant in {s:?}"
            );
        }
    }

    #[test]
    fn reload_and_restart_are_never_redundant() {
        for t in [Reload, Restart] {
            for s in [
                Active,
                Reloading,
                Refreshing,
                Inactive,
                Failed,
                Activating,
                Deactivating,
                Maintenance,
                Unknown,
            ] {
                assert!(!job_type_is_redundant(t, s), "{t:?} redundant in {s:?}");
            }
        }
    }

    #[test]
    fn nop_is_always_redundant() {
        for s in [
            Active,
            Reloading,
            Refreshing,
            Inactive,
            Failed,
            Activating,
            Deactivating,
            Maintenance,
            Unknown,
        ] {
            assert!(job_type_is_redundant(Nop, s), "Nop redundant in {s:?}");
        }
    }

    #[test]
    fn transient_types_are_not_redundant() {
        for t in [TryRestart, TryReload, ReloadOrStart] {
            for s in [Active, Inactive, Unknown] {
                assert!(!job_type_is_redundant(t, s));
            }
        }
    }

    // ------------------------------------------------------------------
    // Merge + collapse (job_type_merge_and_collapse)
    // ------------------------------------------------------------------

    #[test]
    fn merge_and_collapse_identical_types() {
        assert_eq!(
            job_type_merge_and_collapse(Start, Start, Unknown),
            Some(Start)
        );
        assert_eq!(job_type_merge_and_collapse(Stop, Stop, Unknown), Some(Stop));
        assert_eq!(job_type_merge_and_collapse(Nop, Nop, Unknown), Some(Nop));
    }

    #[test]
    fn merge_and_collapse_conflicts_return_none() {
        assert_eq!(job_type_merge_and_collapse(Stop, Start, Active), None);
        assert_eq!(job_type_merge_and_collapse(Start, Stop, Inactive), None);
        assert_eq!(job_type_merge_and_collapse(Stop, Reload, Unknown), None);
        assert_eq!(job_type_merge_and_collapse(Restart, Stop, Active), None);
    }

    #[test]
    fn merge_and_collapse_resolves_reload_or_start_by_state() {
        // Start ⊕ Reload → ReloadOrStart → collapse by state.
        assert_eq!(
            job_type_merge_and_collapse(Start, Reload, Active),
            Some(Reload)
        );
        assert_eq!(
            job_type_merge_and_collapse(Start, Reload, Reloading),
            Some(Reload)
        );
        assert_eq!(
            job_type_merge_and_collapse(Start, Reload, Inactive),
            Some(Start)
        );
        assert_eq!(
            job_type_merge_and_collapse(Start, Reload, Failed),
            Some(Start)
        );
        assert_eq!(
            job_type_merge_and_collapse(Start, Reload, Unknown),
            Some(Start)
        );
    }

    #[test]
    fn merge_and_collapse_restart_absorbs_and_verify_active_merges() {
        assert_eq!(
            job_type_merge_and_collapse(Restart, VerifyActive, Active),
            Some(Restart)
        );
        assert_eq!(
            job_type_merge_and_collapse(Reload, VerifyActive, Active),
            Some(Reload)
        );
        assert_eq!(
            job_type_merge_and_collapse(Start, VerifyActive, Inactive),
            Some(Start)
        );
    }

    #[test]
    fn merge_and_collapse_nop_yields_to_the_other_job() {
        assert_eq!(job_type_merge_and_collapse(Nop, Stop, Active), Some(Stop));
        assert_eq!(job_type_merge_and_collapse(Stop, Nop, Unknown), Some(Stop));
        assert_eq!(job_type_merge_and_collapse(Nop, Start, Active), Some(Start));
    }

    // ------------------------------------------------------------------
    // JobKind bridge
    // ------------------------------------------------------------------

    #[test]
    fn job_kind_maps_to_job_type() {
        assert_eq!(JobType::from_job_kind(JobKind::Start), Start);
        assert_eq!(JobType::from_job_kind(JobKind::Stop), Stop);
        assert_eq!(JobType::from_job_kind(JobKind::Restart), Restart);
        assert_eq!(JobType::from_job_kind(JobKind::Reload), Reload);
        assert_eq!(JobType::from_job_kind(JobKind::Nop), Nop);
    }

    #[test]
    fn job_type_as_str_matches_systemd_names() {
        assert_eq!(JobType::from_job_kind(JobKind::Start).as_str(), "start");
        assert_eq!(JobType::from_job_kind(JobKind::Stop).as_str(), "stop");
        assert_eq!(JobType::from_job_kind(JobKind::Restart).as_str(), "restart");
        assert_eq!(JobType::from_job_kind(JobKind::Reload).as_str(), "reload");
        assert_eq!(Nop.as_str(), "nop");
        assert_eq!(TryRestart.as_str(), "try-restart");
        assert_eq!(TryReload.as_str(), "try-reload");
        assert_eq!(ReloadOrStart.as_str(), "reload-or-start");
        assert_eq!(VerifyActive.as_str(), "verify-active");
    }
}
