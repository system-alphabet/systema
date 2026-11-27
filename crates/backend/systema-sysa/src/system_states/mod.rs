//! System-state queries used by unit conditions and assertions.
//!
//! Queries whose answers are fixed for a boot are isolated from queries that
//! can change while the allocator is running.

mod r#static;
mod volatile;

use sysa::ir::Condition;
use tracing::debug;

use crate::unit::types::UnitSection;

/// Evaluate all `Condition*=` directives of `unit`.
///
/// Returns `true` if the conditions pass (unit should start), `false` if the
/// start must be silently skipped (unit stays inactive).
pub(crate) fn check_conditions(unit: &UnitSection) -> bool {
    eval_condition_list(&unit.conditions)
}

/// Evaluate all `Assert*=` directives of `unit`.
///
/// Returns `true` if the asserts pass, `false` if the start job must fail.
pub(crate) fn check_asserts(unit: &UnitSection) -> bool {
    eval_condition_list(&unit.asserts)
}

/// Evaluate a `Condition*=`/`Assert*=` list with systemd semantics.
///
/// Regular conditions are ANDed. A directive prefixed with `|` is a
/// *triggering* condition: when at least one trigger exists, the unit also
/// needs any one of the triggers to apply. `!` negates the check result.
///
/// Kinds this build cannot evaluate are treated as met, so a missing
/// implementation never silently disables a unit (they are logged at debug).
fn eval_condition_list(conditions: &[Condition]) -> bool {
    let mut has_trigger = false;
    let mut trigger_met = false;
    let mut regular_met = true;

    for condition in conditions {
        let Some(result) = eval_condition(condition) else {
            continue;
        };
        let met = if condition.negate { !result } else { result };
        if condition.trigger {
            has_trigger = true;
            trigger_met |= met;
        } else {
            regular_met &= met;
        }
    }

    regular_met && (!has_trigger || trigger_met)
}

/// Evaluate one directive. `None` means "not evaluated by this build".
fn eval_condition(condition: &Condition) -> Option<bool> {
    let kind = condition.kind.as_str();
    let value = condition.value.as_str();
    let result = match kind {
        "KernelCommandLine" | "Security" => r#static::evaluate(kind, value),
        "PathExists" | "PathExistsGlob" | "FileNotEmpty" | "DirectoryNotEmpty"
        | "KernelModuleLoaded" | "ACPower" | "FirstBoot" | "Host" | "Environment" => {
            volatile::evaluate(kind, value)
        }
        _ => None,
    };
    if result.is_none() && !(kind == "Security") {
        debug!("Condition/Assert kind '{kind}' is not evaluated; treated as met");
    }
    result
}
