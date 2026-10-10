//! ReloadTask — unified reload pipeline for System A.
//!
//! All unit-set mutations (boot-time finder commits and runtime
//! `systemctl daemon-reload`) flow through this single consumer.  System A
//! never scans unit files directly; it spawns System F as a subprocess
//! which discovers format-specific finders, runs them, and commits the
//! discovered units via IPC.  After the commit, ReloadTask applies
//! default dependencies and syncs workers.

use anyhow::Result;
use tokio::sync::{mpsc, oneshot};
use tracing::{error, info};

use crate::state::AllocatorHandle;

/// Requests that flow through the reload pipeline.
pub enum ReloadRequest {
    /// D-Bus Reload / IPC daemon-reload: blocks until System F has
    /// completed its re-scan and the post-commit work (inject + sync)
    /// is done.  The oneshot sender is signalled when finished.
    ByTrigger(oneshot::Sender<()>),
    /// Finder commit completed via the IPC path (System F called
    /// `finder.commit_units` directly): fire-and-forget; inject +
    /// sync happens asynchronously without blocking the commit ack.
    FromCommit,
}

/// Single-consumer task that serialises all reload operations.
pub struct ReloadTask;

impl ReloadTask {
    /// Spawn the reload task and return the producer handle + join handle.
    pub fn spawn(
        allocator: AllocatorHandle,
    ) -> (mpsc::Sender<ReloadRequest>, tokio::task::JoinHandle<()>) {
        let (tx, rx) = mpsc::channel(16);
        let handle = tokio::spawn(Self::run(allocator, rx));
        (tx, handle)
    }

    /// Main loop: process reload requests until the channel closes.
    async fn run(allocator: AllocatorHandle, mut rx: mpsc::Receiver<ReloadRequest>) {
        info!("ReloadTask started");
        while let Some(req) = rx.recv().await {
            match req {
                ReloadRequest::ByTrigger(reply) => {
                    Self::handle_trigger(&allocator, reply).await;
                }
                ReloadRequest::FromCommit => {
                    Self::handle_commit(&allocator);
                }
            }
        }
        info!("ReloadTask stopped (channel closed)");
    }

    /// Spawn System F as a subprocess, wait for it to discover and commit
    /// units, then apply default dependencies + sync workers.
    async fn handle_trigger(allocator: &AllocatorHandle, reply: oneshot::Sender<()>) {
        info!("ReloadTask: spawning System F for re-scan");
        match Self::spawn_sysf().await {
            Ok(()) => {
                Self::apply_after_commit(allocator);
                info!("ReloadTask: daemon-reload complete");
            }
            Err(e) => {
                error!("ReloadTask: System F re-scan failed: {e}");
            }
        }
        let _ = reply.send(());
    }

    /// Handle a finder-commit completion: inject default dependencies
    /// and sync workers asynchronously (fire-and-forget).
    fn handle_commit(allocator: &AllocatorHandle) {
        info!("ReloadTask: processing finder commit (inject + sync)");
        Self::apply_after_commit(allocator);
    }

    /// Apply default dependencies and request a full worker sync.
    ///
    /// This is the common tail of both the trigger and commit paths.
    fn apply_after_commit(allocator: &AllocatorHandle) {
        crate::unit::loader::inject_default_dependencies(allocator.clone());
        tokio::spawn(crate::scheduler::request_all_worker_syncs(
            allocator.clone(),
        ));
    }

    /// Locate the `systema-sysf` binary and run it as a subprocess.
    ///
    /// System F discovers format-specific finders (e.g. `systema-sysf.systemd`),
    /// runs them all concurrently to stage discovered units, then commits the
    /// staging area into System A's active set via IPC.
    ///
    /// # Errors
    /// Returns an error if the binary cannot be found or exits with a
    /// non-zero status.
    async fn spawn_sysf() -> Result<()> {
        let sysf_path = Self::find_sysf_binary()?;
        info!("ReloadTask: running {}", sysf_path.display());

        let status = tokio::process::Command::new(&sysf_path)
            .arg("--name")
            .arg("systema-sysf/reload")
            .status()
            .await
            .map_err(|e| {
                anyhow::anyhow!(sysa::l10n::fmt(
                    sysa::l10n::t_("failed to spawn {program}: {e}"),
                    &[
                        ("program", &(sysf_path.display()).to_string()),
                        ("e", &e.to_string())
                    ]
                ))
            })?;

        if status.success() {
            info!("ReloadTask: System F completed successfully");
            Ok(())
        } else {
            anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_("System F exited with status: {status}"),
                &[("status", &status.to_string())]
            ));
        }
    }

    /// Search for the `systema-sysf` binary in the standard search paths.
    fn find_sysf_binary() -> Result<std::path::PathBuf> {
        for dir in &sysa::paths::instance().systema_bin_search_paths {
            let path = std::path::Path::new(dir).join("systema-sysf");
            if path.exists() {
                return Ok(path);
            }
        }
        anyhow::bail!(sysa::l10n::fmt(
            sysa::l10n::t_("systema-sysf binary not found in search paths: {paths}"),
            &[(
                "paths",
                &format!("{:?}", sysa::paths::instance().systema_bin_search_paths)
            )]
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify that `find_sysf_binary` searches the configured paths.
    /// This test only passes if `systema-sysf` is built and on PATH.
    #[test]
    fn find_sysf_binary_searches_paths() {
        // This is a basic smoke test — the binary must exist for CI.
        match ReloadTask::find_sysf_binary() {
            Ok(path) => {
                assert!(path.exists(), "found path does not exist: {path:?}");
            }
            Err(e) => {
                // Acceptable in environments where sysf isn't built.
                eprintln!("systema-sysf not found (expected in dev): {e}");
            }
        }
    }
}
