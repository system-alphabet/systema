//! The bridge's memory-mirror of System A's unit registry.
//!
//! The System Wrapper keeps its own projection of unit state, primed from
//! `list_snapshots` and kept fresh by the `unit.new` / `unit.changed` /
//! `unit.removed` / `job.new` / `job.completed` control-port events.  All
//! D-Bus property getters read from here — the bridge never touches the
//! allocator, so D-Bus traffic stays completely off the workload plane.

use std::collections::HashMap;
use std::sync::Arc;

use sysa::proto::{JobInfo, UnitSnapshot};

pub const ROOT_SLICE_NAME: &str = "-.slice";

#[derive(Default)]
pub struct UnitMirror {
    units: HashMap<String, UnitSnapshot>,
    /// Live jobs, mirroring `manager.list_jobs` (running only).
    running_jobs: Vec<JobInfo>,
}

pub type MirrorHandle = Arc<parking_lot::RwLock<UnitMirror>>;

impl UnitMirror {
    pub fn new() -> Self {
        Self::default()
    }

    /// Prime the mirror from the initial snapshot/job listing.
    pub fn seed(&mut self, units: Vec<UnitSnapshot>, jobs: Vec<JobInfo>) {
        self.units = units.into_iter().map(|s| (s.name.clone(), s)).collect();
        self.running_jobs = jobs;
    }

    /// Apply a `unit.new` / `unit.changed` snapshot.
    pub fn upsert(&mut self, snap: UnitSnapshot) {
        let name = snap.name.clone();
        self.units.insert(name, snap);
    }

    /// Apply a `unit.removed` event.
    pub fn remove(&mut self, name: &str) {
        self.units.remove(name);
    }

    /// Replace the live-jobs projection wholesale.
    #[cfg(test)]
    pub fn set_jobs(&mut self, jobs: Vec<JobInfo>) {
        self.running_jobs = jobs;
    }

    /// Add a freshly started job (`job.new`).
    pub fn add_job(&mut self, job: JobInfo) {
        if !self.running_jobs.iter().any(|j| j.job_id == job.job_id) {
            self.running_jobs.push(job);
            self.running_jobs.sort_by_key(|j| j.job_id);
        }
    }

    /// Remove a finished job (`job.completed`).
    pub fn remove_job(&mut self, job_id: u64) {
        self.running_jobs.retain(|j| j.job_id != job_id);
    }

    pub fn get(&self, name: &str) -> Option<&UnitSnapshot> {
        self.units.get(name)
    }

    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.units.keys().cloned().collect();
        names.sort();
        names
    }

    pub fn len(&self) -> usize {
        self.units.len()
    }

    /// The unit whose snapshot is currently `failed`.
    pub fn failed_count(&self) -> u32 {
        self.units
            .values()
            .filter(|s| s.active_state == "failed")
            .count() as u32
    }

    pub fn running_jobs(&self) -> &[JobInfo] {
        &self.running_jobs
    }

    pub fn running_job_for(&self, unit_name: &str) -> Option<&JobInfo> {
        self.running_jobs
            .iter()
            .find(|j| j.unit_name == unit_name && j.status == "running")
    }

    pub fn find_by_pid(&self, pid: u32) -> Option<String> {
        self.units.values().find_map(|s| {
            if s.main_pid == pid || s.pids.contains(&pid) {
                Some(s.name.clone())
            } else {
                None
            }
        })
    }

    pub fn find_by_invocation(&self, invocation_id: &str) -> Option<String> {
        self.units.values().find_map(|s| {
            if s.invocation_id == invocation_id {
                Some(s.name.clone())
            } else {
                None
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(name: &str) -> UnitSnapshot {
        UnitSnapshot {
            name: name.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn seed_and_lookup() {
        let mut m = UnitMirror::new();
        m.seed(vec![snap("a.service"), snap("b.service")], vec![]);
        assert_eq!(m.len(), 2);
        assert!(m.get("a.service").is_some());
        assert_eq!(m.names(), vec!["a.service", "b.service"]);
    }

    #[test]
    fn upsert_remove_and_removal() {
        let mut m = UnitMirror::new();
        m.upsert(snap("x.service"));
        assert!(m.get("x.service").is_some());
        m.remove("x.service");
        assert!(m.get("x.service").is_none());
    }

    #[test]
    fn job_tracking_roundtrip() {
        let mut m = UnitMirror::new();
        m.set_jobs(vec![JobInfo {
            job_id: 1,
            unit_name: "a.service".to_string(),
            job_type: "start".to_string(),
            status: "running".to_string(),
        }]);
        assert_eq!(m.running_jobs().len(), 1);
        assert!(m.running_job_for("a.service").is_some());
        m.remove_job(1);
        assert!(m.running_job_for("a.service").is_none());
        m.add_job(JobInfo {
            job_id: 2,
            unit_name: "a.service".to_string(),
            job_type: "start".to_string(),
            status: "running".to_string(),
        });
        assert!(m.running_job_for("a.service").is_some());
    }
}