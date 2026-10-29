use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use sysa::proto::MountConfig;
use tokio::process::Command;
use tracing::{error, info, warn};

use crate::linux::mountinfo;
use crate::linux::state::{MountInstance, MountRegistry, MountState};

pub async fn do_mount(
    registry: MountRegistry,
    unit_name: &str,
    config: &MountConfig,
) -> Result<()> {
    let mount_point = config.r#where.clone();
    let mount_point_path = Path::new(&mount_point);

    // Create mount point directory if needed.
    let dir_mode = if config.directory_mode.is_empty() {
        "0755"
    } else {
        &config.directory_mode
    };
    if !mount_point_path.exists() {
        Command::new("mkdir")
            .arg("-p")
            .arg(&mount_point)
            .status()
            .await
            .context(sysa::l10n::t_("mkdir -p for mount point failed"))?;
        Command::new("chmod")
            .arg(dir_mode)
            .arg(&mount_point)
            .status()
            .await
            .context(sysa::l10n::t_("chmod for mount point failed"))?;
    }

    {
        let mut reg = registry.lock();
        if let Some(inst) = reg.get_mut(unit_name) {
            inst.from_fragment = true;
        } else {
            let mut inst = MountInstance::new(
                unit_name.to_string(),
                mount_point.clone(),
                config.r#what.clone(),
            );
            inst.from_fragment = true;
            inst.fstype = config.r#type.clone();
            inst.options = config.options.clone();
            reg.insert(unit_name.to_string(), inst);
        }
    }

    // Idempotency: skip if already mounted (matches systemd behavior).
    if mountinfo::mount_point_is_mounted(&mount_point) {
        info!("Already mounted, skipping: {}", mount_point);
        {
            let mut reg = registry.lock();
            if let Some(inst) = reg.get_mut(unit_name) {
                inst.state = MountState::Mounted;
            }
        }
        return Ok(());
    }

    // Build mount command.
    let mut cmd = Command::new("mount");
    if config.sloppy_options {
        cmd.arg("-s");
    }
    if !config.r#type.is_empty() && config.r#type != "auto" {
        cmd.arg("-t");
        cmd.arg(&config.r#type);
    }
    if !config.options.is_empty() {
        cmd.arg("-o");
        cmd.arg(&config.options);
    }
    cmd.arg(&config.r#what);
    cmd.arg(&mount_point);

    info!("Mounting: {:?}", cmd.as_std());

    let timeout = if config.timeout_sec > 0 {
        Duration::from_secs(config.timeout_sec as u64)
    } else {
        Duration::from_secs(30)
    };

    let output = tokio::time::timeout(timeout, cmd.output())
        .await
        .context(sysa::l10n::t_("mount timed out"))?
        .context(sysa::l10n::t_("mount command failed to start"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        error!("mount {} failed: {}", mount_point, stderr.trim());
        let err = anyhow::anyhow!(sysa::l10n::fmt(
            sysa::l10n::t_("mount failed: {stderr}"),
            &[("stderr", &(stderr.trim()).to_string())]
        ));
        return Err(err);
    }

    // Verify against kernel: re-check /proc/self/mountinfo.
    // Trust the kernel, not the exit code (systemd mount_enter_dead_or_mounted pattern).
    let actually_mounted = mountinfo::mount_point_is_mounted(&mount_point);
    let mut reg = registry.lock();
    if let Some(inst) = reg.get_mut(unit_name) {
        if actually_mounted {
            inst.state = MountState::Mounted;
        } else {
            warn!(
                "mount command exited OK but {} is NOT in /proc/self/mountinfo; staying Dead",
                mount_point
            );
        }
    }
    drop(reg);

    if actually_mounted {
        info!("Mount succeeded: {}", mount_point);
    } else {
        warn!(
            "Mount command reported success but kernel disagrees: {}",
            mount_point
        );
    }

    Ok(())
}

pub async fn do_umount(
    registry: MountRegistry,
    unit_name: &str,
    config: Option<&MountConfig>,
) -> Result<()> {
    let mount_point = {
        let reg = registry.lock();
        reg.get(unit_name)
            .map(|inst| inst.mount_point.clone())
            .unwrap_or_else(|| {
                warn!("No mount point found for {}", unit_name);
                String::new()
            })
    };

    if mount_point.is_empty() {
        anyhow::bail!(sysa::l10n::fmt(
            sysa::l10n::t_("No mount point recorded for unit {unit_name}"),
            &[("unit_name", &unit_name.to_string())]
        ));
    }

    if !mountinfo::mount_point_is_mounted(&mount_point) {
        reconcile_umount_state(&registry, unit_name, false);
        info!(
            "Mount point is already unmounted, skipping: {}",
            mount_point
        );
        return Ok(());
    }

    // Mark as unmounting.
    {
        let mut reg = registry.lock();
        if let Some(inst) = reg.get_mut(unit_name) {
            inst.state = MountState::Unmounting;
        }
    }

    let mut cmd = Command::new("umount");
    cmd.arg(&mount_point);

    let lazy = config.map(|c| c.lazy_unmount).unwrap_or(false);
    let force = config.map(|c| c.force_unmount).unwrap_or(false);

    if force {
        cmd.arg("-f");
    }
    if lazy {
        cmd.arg("-l");
    }

    info!("Unmounting: {:?}", cmd.as_std());

    let timeout = Duration::from_secs(
        config
            .map(|c| {
                if c.timeout_sec > 0 {
                    c.timeout_sec as u64
                } else {
                    30
                }
            })
            .unwrap_or(30),
    );

    let output = match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            let still_mounted = mountinfo::mount_point_is_mounted(&mount_point);
            reconcile_umount_state(&registry, unit_name, still_mounted);
            if !still_mounted {
                info!(
                    "Mount point disappeared while starting umount, treating stop as complete: {}",
                    mount_point
                );
                return Ok(());
            }
            return Err(error).context(sysa::l10n::t_("umount command failed to start"));
        }
        Err(_) => {
            let still_mounted = mountinfo::mount_point_is_mounted(&mount_point);
            reconcile_umount_state(&registry, unit_name, still_mounted);
            if !still_mounted {
                info!(
                        "Mount point disappeared while waiting for umount, treating stop as complete: {}",
                        mount_point
                    );
                return Ok(());
            }
            anyhow::bail!(sysa::l10n::t_("umount timed out"));
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let still_mounted = mountinfo::mount_point_is_mounted(&mount_point);
        reconcile_umount_state(&registry, unit_name, still_mounted);
        if !still_mounted {
            info!(
                "Mount point disappeared during failed umount; treating stop as complete: {}",
                mount_point
            );
            return Ok(());
        }
        error!("umount {} failed: {}", mount_point, stderr.trim());
        anyhow::bail!(sysa::l10n::fmt(
            sysa::l10n::t_("umount failed: {stderr}"),
            &[("stderr", &(stderr.trim()).to_string())]
        ));
    }

    // Verify against kernel: re-check /proc/self/mountinfo.
    // Trust the kernel, not the exit code.
    let still_mounted = mountinfo::mount_point_is_mounted(&mount_point);

    let mut reg = registry.lock();
    if let Some(inst) = reg.get_mut(unit_name) {
        if still_mounted {
            // The umount command may remove only one layer of a stacked mount.
            // Keep the worker state aligned with the kernel and fail this stop
            // rather than claiming the unit is dead.
            inst.n_retry_umount += 1;
            inst.state = MountState::Mounted;
            inst.from_mountinfo = true;
            warn!(
                "Mount point {} remains mounted after umount attempt {}",
                mount_point, inst.n_retry_umount
            );
        } else {
            inst.state = MountState::Dead;
            inst.from_mountinfo = false;
            inst.n_retry_umount = 0;
            info!("Unmount succeeded: {}", mount_point);
        }
    } else {
        // Registry entry vanished under us; nothing to update
        info!("Unmount succeeded: {} (no registry entry)", mount_point);
    }
    drop(reg);

    if still_mounted {
        anyhow::bail!(sysa::l10n::fmt(
            sysa::l10n::t_("Mount point {mount_point} remained mounted after umount."),
            &[("mount_point", &mount_point)]
        ));
    }

    Ok(())
}

/// Reconcile the worker's mount state with the kernel after an umount failure.
/// Returns whether the mount point is still mounted.
fn reconcile_umount_state(registry: &MountRegistry, unit_name: &str, still_mounted: bool) {
    let mut reg = registry.lock();
    if let Some(inst) = reg.get_mut(unit_name) {
        if still_mounted {
            inst.state = MountState::Mounted;
            inst.from_mountinfo = true;
        } else {
            inst.state = MountState::Dead;
            inst.from_mountinfo = false;
            inst.n_retry_umount = 0;
        }
    }
}

pub async fn do_remount(
    registry: MountRegistry,
    unit_name: &str,
    config: &MountConfig,
) -> Result<()> {
    let mount_point = {
        let reg = registry.lock();
        reg.get(unit_name).map(|inst| inst.mount_point.clone())
    };

    let mount_point = match mount_point {
        Some(p) => p,
        None => anyhow::bail!(sysa::l10n::fmt(
            sysa::l10n::t_("Cannot remount {unit_name}: not currently mounted"),
            &[("unit_name", &unit_name.to_string())]
        )),
    };

    let mut cmd = Command::new("mount");
    cmd.arg("-o");
    cmd.arg(format!("remount,{}", config.options));
    cmd.arg(&mount_point);

    info!("Remounting: {:?}", cmd.as_std());

    let timeout = Duration::from_secs(if config.timeout_sec > 0 {
        config.timeout_sec as u64
    } else {
        30
    });

    let output = tokio::time::timeout(timeout, cmd.output())
        .await
        .context(sysa::l10n::t_("remount timed out"))?
        .context(sysa::l10n::t_("remount command failed to start"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        error!("remount {} failed: {}", mount_point, stderr.trim());
        anyhow::bail!(sysa::l10n::fmt(
            sysa::l10n::t_("remount failed: {stderr}"),
            &[("stderr", &(stderr.trim()).to_string())]
        ));
    }

    // Verify the mount point still exists after remount.
    if !mountinfo::mount_point_is_mounted(&mount_point) {
        warn!(
            "remount exited OK but {} is no longer in /proc/self/mountinfo",
            mount_point
        );
    }

    info!("Remount succeeded: {}", mount_point);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stopping_an_already_unmounted_mount_is_idempotent() {
        let registry = crate::linux::state::new_mount_registry();
        let unit_name = "test-stale-mount.mount";
        let mount_point = std::env::temp_dir()
            .join(format!("systema-not-mounted-{}", std::process::id()))
            .to_string_lossy()
            .into_owned();
        let mut instance = MountInstance::new(
            unit_name.to_string(),
            mount_point,
            "test-device".to_string(),
        );
        instance.state = MountState::Mounted;
        instance.from_mountinfo = true;
        registry.lock().insert(unit_name.to_string(), instance);

        do_umount(registry.clone(), unit_name, None)
            .await
            .expect("stopping an absent mount should succeed");

        let registry = registry.lock();
        let instance = registry.get(unit_name).expect("mount remains registered");
        assert_eq!(instance.state, MountState::Dead);
        assert!(!instance.from_mountinfo);
        assert_eq!(instance.n_retry_umount, 0);
    }
}
