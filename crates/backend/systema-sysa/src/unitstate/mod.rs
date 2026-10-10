//! Snapshots of a unit's *full cached state*, built from the System
//! Allocator's in-memory caches only (never from any worker).
//!
//! The output of [`entry_json`] is a JSON document containing every cached
//! property of a unit: the complete parsed unit file (`[Unit]`, `[Install]`,
//! and the type-specific section), the runtime state cache (active/sub
//! state, main PID, invocation id, …), cgroup metrics, live jobs, the
//! desired state, and the owning worker.  It is consumed by the
//! `systema-unitstatectl` front-end, which renders it as YAML.

use serde_json::{json, Map, Value};

use crate::state::{AllocatorState, DesiredState, Job, JobStatus};

/// All loaded unit names, sorted for deterministic streaming.
pub fn unit_names(state: &AllocatorState) -> Vec<String> {
    let mut names: Vec<String> = state.units.keys().cloned().collect();
    names.sort();
    names
}

/// Build the JSON snapshot for one unit; `None` if the unit is unknown.
///
/// All map keys are sorted on output so that the stream is deterministic
/// across runs.
pub fn entry_json(state: &AllocatorState, name: &str) -> Option<Value> {
    let uf = state.units.get(name)?;
    let mut map = Map::new();

    map.insert("kind".into(), json!(uf.kind.worker_type()));

    if let Some(cs) = state.unit_states.get(name) {
        map.insert("active_state".into(), json!(cs.active_state));
        map.insert("sub_state".into(), json!(cs.sub_state));
        map.insert("main_pid".into(), json!(cs.main_pid));
        map.insert("invocation_id".into(), json!(cs.invocation_id));
        map.insert(
            "active_enter_timestamp".into(),
            json!(cs.active_enter_timestamp),
        );
        map.insert(
            "inactive_enter_timestamp".into(),
            json!(cs.inactive_enter_timestamp),
        );
        map.insert("extensions".into(), sorted_str_map(&cs.extensions));
        map.insert("pids".into(), json!(cs.pids));
        map.insert("controller".into(), json!(cs.controller));
    }

    if let Some(cm) = state.cgroup_metrics.get(name) {
        map.insert("control_group".into(), json!(cm.control_group));
        map.insert("control_group_id".into(), json!(cm.control_group_id));
        map.insert("cgroup_metrics".into(), sorted_u64_map(&cm.metrics));
        map.insert("processes".into(), cgroup_processes(cm));
    }

    if let Some(desired) = state.desired.get(name) {
        let s = match desired {
            DesiredState::Active => "active",
            DesiredState::Inactive => "inactive",
        };
        map.insert("desired".into(), json!(s));
    }

    if let Some(owner) = state.unit_owners.get(name) {
        map.insert("owner".into(), json!(owner));
    }

    let mut aliases: Vec<String> = state
        .aliases
        .iter()
        .filter(|(_, canonical)| canonical.as_str() == name)
        .map(|(alias, _)| alias.clone())
        .collect();
    aliases.sort();
    map.insert("aliases".into(), json!(aliases));

    let mut jobs: Vec<&Job> = state
        .jobs
        .values()
        .filter(|j| j.unit_name == name)
        .collect();
    jobs.sort_by_key(|j| j.id);
    map.insert(
        "jobs".into(),
        Value::Array(jobs.iter().map(|j| job_json(j)).collect()),
    );

    map.insert("unit".into(), serde_json::to_value(&uf.unit).ok()?);
    map.insert("install".into(), serde_json::to_value(&uf.install).ok()?);
    insert_section(&mut map, "service", uf.service.as_ref())?;
    insert_section(&mut map, "mount", uf.mount.as_ref())?;
    insert_section(&mut map, "automount", uf.automount.as_ref())?;
    insert_section(&mut map, "timer", uf.timer.as_ref())?;
    insert_section(&mut map, "socket", uf.socket.as_ref())?;
    insert_section(&mut map, "swap", uf.swap.as_ref())?;
    insert_section(&mut map, "path", uf.path.as_ref())?;
    insert_section(&mut map, "slice", uf.slice.as_ref())?;
    insert_section(&mut map, "scope", uf.scope.as_ref())?;
    insert_section(&mut map, "device", uf.device.as_ref())?;

    Some(Value::Object(map))
}

fn insert_section<T: serde::Serialize>(
    map: &mut Map<String, Value>,
    key: &str,
    section: Option<&T>,
) -> Option<()> {
    if let Some(section) = section {
        map.insert(key.into(), serde_json::to_value(section).ok()?);
    }
    Some(())
}

fn sorted_str_map(m: &std::collections::HashMap<String, String>) -> Value {
    let ordered: std::collections::BTreeMap<&String, &String> = m.iter().collect();
    json!(ordered)
}

fn sorted_u64_map(m: &std::collections::HashMap<String, u64>) -> Value {
    let ordered: std::collections::BTreeMap<&String, &u64> = m.iter().collect();
    json!(ordered)
}

fn cgroup_processes(cm: &sysa::proto::UnitCgroupMetrics) -> Value {
    let mut processes: Vec<Value> = cm
        .processes
        .iter()
        .map(|p| {
            json!({
                "subpath": p.subpath,
                "pid": p.pid,
                "name": p.name,
            })
        })
        .collect();
    processes.sort_by(|a, b| {
        let ka = (
            a["subpath"].as_str().unwrap_or(""),
            a["pid"].as_u64().unwrap_or(0),
        );
        let kb = (
            b["subpath"].as_str().unwrap_or(""),
            b["pid"].as_u64().unwrap_or(0),
        );
        ka.cmp(&kb)
    });
    Value::Array(processes)
}

fn job_json(j: &Job) -> Value {
    let status = match &j.status {
        JobStatus::Running => json!("running"),
        JobStatus::Done => json!("done"),
        JobStatus::Failed(msg) => Value::String(sysa::l10n::fmt(
            sysa::l10n::t_("failed: {msg}"),
            &[("msg", &msg.to_string())],
        )),
        JobStatus::Cancelled => json!("cancelled"),
    };
    json!({
        "id": j.id,
        "kind": j.kind.as_str(),
        "status": status,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::DesiredState;
    use crate::unit::types::UnitFile;
    use std::collections::HashMap;

    fn state_with_unit() -> AllocatorState {
        let mut units = HashMap::new();
        let mut uf = UnitFile::new("foo.service");
        uf.unit.description = "Foo unit".into();
        uf.unit.requires.insert("bar.service".into());
        uf.unit.after.insert("network.target".into());
        uf.unit.after.insert("local-fs.target".into());
        units.insert("foo.service".to_string(), uf);
        let mut state = AllocatorState::new();
        state.units = units;
        state
            .desired
            .insert("foo.service".into(), DesiredState::Active);
        state
            .aliases
            .insert("alias.service".into(), "foo.service".into());
        state
    }

    #[test]
    fn entry_json_contains_all_cached_fields() {
        let state = state_with_unit();
        let doc = entry_json(&state, "foo.service").expect("unit exists");
        let obj = doc.as_object().unwrap();
        assert_eq!(obj["kind"], "service");
        assert_eq!(obj["unit"]["description"], "Foo unit");
        assert_eq!(
            obj["unit"]["after"],
            json!(["local-fs.target", "network.target"])
        );
        assert_eq!(obj["desired"], "active");
        assert_eq!(obj["aliases"], json!(["alias.service"]));
        assert_eq!(obj["install"]["wanted_by"], json!([]));
    }

    #[test]
    fn entry_json_unknown_unit_is_none() {
        let state = state_with_unit();
        assert!(entry_json(&state, "nope.service").is_none());
    }

    #[test]
    fn unit_names_are_sorted() {
        let state = state_with_unit();
        let mut units = HashMap::new();
        units.insert("zeta.service".to_string(), UnitFile::new("zeta.service"));
        units.insert("alpha.service".to_string(), UnitFile::new("alpha.service"));
        let mut state = state;
        state.units = units;
        assert_eq!(unit_names(&state), vec!["alpha.service", "zeta.service"]);
    }
}
