//! Polling fallback backend for platforms without inotify or kqueue.
//!
//! Simply wakes up every `interval` for every armed unit and marks each as
//! `written`, forcing the engine to re-evaluate level-triggered conditions
//! from the filesystem and edge-triggered conditions via signature
//! comparison.  This is also useful as a deterministic backend in tests.

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;

use crate::spec::PathSpec;

use super::{FsEvent, PathBackend, PathChange};

pub struct PollingBackend {
    armed: Mutex<HashSet<String>>,
    interval: Duration,
}

impl PollingBackend {
    pub fn new() -> Self {
        PollingBackend {
            armed: Mutex::new(HashSet::new()),
            interval: Duration::from_secs(1),
        }
    }
}

#[async_trait]
impl PathBackend for PollingBackend {
    fn arm(&self, unit: &str, _specs: &[PathSpec]) -> anyhow::Result<()> {
        self.armed.lock().unwrap().insert(unit.to_string());
        Ok(())
    }

    fn disarm(&self, unit: &str) {
        self.armed.lock().unwrap().remove(unit);
    }

    fn armed_units(&self) -> Vec<String> {
        self.armed
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect()
    }

    async fn changes(&self) -> Vec<PathChange> {
        tokio::time::sleep(self.interval).await;
        self.armed_units()
            .into_iter()
            .map(|unit| PathChange {
                unit,
                event: FsEvent {
                    written: true,
                    ..Default::default()
                },
            })
            .collect()
    }
}
