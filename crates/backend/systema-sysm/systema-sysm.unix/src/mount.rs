use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use sysa::proto::MountConfig;
use tokio::process::Command;
use tracing::{info, warn};

use crate::state::{MountInstance, MountRegistry, MountState};

// ---------------------------------------------------------------------------
// Platform-specific mount(8) flag helpers
// ---------------------------------------------------------------------------

/// Flag to specify filesystem type: `-t` on Linux/BSD, `-F` on Solaris.
fn mount_type_flag() -> &'static str {
    #[cfg(any(target_os = "solaris", target_os = "illumos"))]
    {
        "-F"
    }
    #[cfg(not(any(target_os = "solaris", target_os = "illumos")))]
    {
        "-t"
    }
}

/// Whether the platform's `mount` supports the `-s` (sloppy) flag.
fn sloppy_supported() -> bool {
    cfg!(any(target_os = "linux", target_os = "android"))
}

/// Whether the platform's `umount` supports the `-l` (lazy detach) flag.
fn lazy_unmount_supported() -> bool {
    cfg!(any(target_os = "linux", target_os = "android"))
}

// ---------------------------------------------------------------------------
// Core operations
// ---------------------------------------------------------------------------

pub async fn do_mount(
    registry: MountRegistry,
    unit_name: &str,
    config: &MountConfig,
) -> Result<()> {
    let mount_point = config.r#where.clone();
    let mount_point_path = Path::new(&mount_point);

    // Create mount point directory if it doesn't exist.
    let dir_mode = if config.directory_mode.is_empty() {
        "0755"
    } else {
        &config.directory_mode
    };
    if !mount_point_path.exists() {
        fs::create_dir_all(&mount_point)
            .context(sysa::l10n::t_("create_dir_all for mount point failed"))?;
        let mode = u32::from_str_radix(dir_mode.trim_start_matches('0'), 8).unwrap_or(0o755);
        fs::set_permissions(&mount_point, fs::Permissions::from_mode(mode))
            .context(sysa::l10n::t_("set_permissions for mount point failed"))?;
    }

    // Idempotency: skip if already mounted (matches systemd behavior).
    if crate::mounttable::mount_point_is_mounted(&mount_point) {
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

    // -s (sloppy) — Linux util-linux only
    if config.sloppy_options {
        if sloppy_supported() {
            cmd.arg("-s");
        } else {
            warn!("sloppy_options set but not supported on this platform — ignoring");
        }
    }

    // Type flag: -t (Linux/BSD) or -F (Solaris)
    if !config.r#type.is_empty() && config.r#type != "auto" {
        cmd.arg(mount_type_flag());
        cmd.arg(&config.r#type);
    }

    // Options
    if !config.options.is_empty() {
        cmd.arg("-o");
        cmd.arg(&config.options);
    }
    cmd.arg(&config.what);
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
        let err = anyhow::anyhow!(sysa::l10n::fmt(
            sysa::l10n::t_("mount failed: {stderr}"),
            &[("stderr", &(stderr.trim()).to_string())]
        ));
        {
            let mut reg = registry.lock();
            if let Some(inst) = reg.get_mut(unit_name) {
                inst.state = MountState::Failed;
            }
        }
        return Err(err);
    }

    {
        let mut reg = registry.lock();
        if let Some(inst) = reg.get_mut(unit_name) {
            inst.state = MountState::Mounted;
            inst.main_pid = None;
        } else {
            reg.insert(
                unit_name.to_string(),
                MountInstance::new(
                    unit_name.to_string(),
                    mount_point.clone(),
                    config.what.clone(),
                ),
            );
            if let Some(inst) = reg.get_mut(unit_name) {
                inst.state = MountState::Mounted;
            }
        }
    }

    info!("Mount succeeded: {}", mount_point);
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
                warn!(
                    "No mount point found for {}; using default cleanup",
                    unit_name
                );
                String::new()
            })
    };

    if mount_point.is_empty() {
        anyhow::bail!(sysa::l10n::fmt(
            sysa::l10n::t_("No mount point recorded for unit {unit_name}"),
            &[("unit_name", &unit_name.to_string())]
        ));
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
        if lazy_unmount_supported() {
            cmd.arg("-l");
        } else {
            warn!("lazy_unmount set but not supported on this platform — ignoring");
        }
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

    let output = tokio::time::timeout(timeout, cmd.output())
        .await
        .context(sysa::l10n::t_("umount timed out"))?
        .context(sysa::l10n::t_("umount command failed to start"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let err = anyhow::anyhow!(sysa::l10n::fmt(
            sysa::l10n::t_("umount failed: {stderr}"),
            &[("stderr", &(stderr.trim()).to_string())]
        ));
        {
            let mut reg = registry.lock();
            if let Some(inst) = reg.get_mut(unit_name) {
                inst.state = MountState::Failed;
            }
        }
        return Err(err);
    }

    {
        let mut reg = registry.lock();
        if let Some(inst) = reg.get_mut(unit_name) {
            inst.state = MountState::Dead;
            inst.main_pid = None;
        }
    }

    info!("Unmount succeeded: {}", mount_point);
    Ok(())
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
        None => {
            anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_("Cannot remount {unit_name}: not currently mounted"),
                &[("unit_name", &unit_name.to_string())]
            ));
        }
    };

    let mut cmd = Command::new("mount");

    #[cfg(any(target_os = "linux", target_os = "android"))]
    cmd.arg("-o").arg(format!("remount,{}", config.options));

    #[cfg(target_os = "freebsd")]
    cmd.arg("-u").arg("-o").arg(&config.options);

    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "freebsd")))]
    {
        cmd.arg("-o").arg(format!("remount,{}", config.options));
        warn!("remount semantics may differ on this platform — falling back to -o remount");
    }

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
        anyhow::bail!(sysa::l10n::fmt(
            sysa::l10n::t_("remount failed: {stderr}"),
            &[("stderr", &(stderr.trim()).to_string())]
        ));
    }

    info!("Remount succeeded: {}", mount_point);
    Ok(())
}
