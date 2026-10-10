//! Path unit controller: implements [`UnitController`] for every `.path`
//! unit registered with System N.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use sysa::controller::{decode_unit_config, UnitController, UnitStatus};
use sysa::proto::PathConfig;
use tracing::info;

use crate::engine::{
    arm_unit, publish_path, recheck_level, status_of, EngineShared,
};
use crate::spec::{PathSpec, PathSpecKind};
use crate::state::{PathInstance, PathState};

/// Path controller owning every `.path` unit registered with System N.
pub struct PathController {
    shared: Arc<EngineShared>,
}

impl PathController {
    pub fn new(shared: Arc<EngineShared>) -> Self {
        PathController { shared }
    }
}

#[async_trait]
impl UnitController for PathController {
    async fn status(&self, unit_name: &str) -> Result<UnitStatus> {
        let reg = self.shared.registry.read();
        let inst = reg.get(unit_name).ok_or_else(|| {
            anyhow!(sysa::l10n::fmt(
                sysa::l10n::t_("Unknown path unit '{unit_name}'"),
                &[("unit_name", &unit_name.to_string())]
            ))
        })?;
        Ok(status_of(unit_name, inst))
    }

    async fn start(&self, unit_name: &str, config: &[u8], invocation_id: &str) -> Result<()> {
        if !unit_name.ends_with(".path") {
            anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_("'{unit_name}' is not a .path unit"),
                &[("unit_name", &unit_name.to_string())]
            ));
        }
        let cfg = decode_unit_config(config)?;
        let path_cfg = cfg.path.clone().ok_or_else(|| {
            anyhow!(sysa::l10n::fmt(
                sysa::l10n::t_("No PathConfig for '{unit_name}'"),
                &[("unit_name", &unit_name.to_string())]
            ))
        })?;

        let specs = build_specs(&path_cfg);
        let target_unit = if path_cfg.unit.is_empty() {
            default_target_unit(unit_name)
        } else {
            path_cfg.unit.clone()
        };

        // `MakeDirectory=` creates the parent directories of the watched
        // paths (mirroring systemd's mkdir_parents semantics).
        if path_cfg.make_directory {
            let mode = parse_directory_mode(&path_cfg.directory_mode);
            for spec in &specs {
                if let Some(parent) = parent_dir_of(&spec.path) {
                    if !parent.exists() {
                        std::fs::create_dir_all(&parent)?;
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(mode))?;
                    }
                }
            }
        }

        let has_condition = !specs.is_empty();
        let inst = PathInstance {
            state: if has_condition {
                PathState::Dead
            } else {
                PathState::Failed
            },
            config: path_cfg.clone(),
            specs,
            target_unit: target_unit.clone(),
            spec_signatures: Vec::new(),
            last_fired_epoch: None,
            n_fired: 0,
            trigger_times: Default::default(),
            last_error: if has_condition {
                None
            } else {
                Some(
                    sysa::l10n::t_(
                        "Path unit has no PathExists=/PathChanged=/DirectoryNotEmpty= condition",
                    )
                    .to_string(),
                )
            },
            invocation_id: if invocation_id.is_empty() {
                None
            } else {
                Some(invocation_id.to_string())
            },
        };

        // Insert into the registry first: arm_unit reads the specs back from
        // it and records fresh stat signatures there.
        {
            let mut reg = self.shared.registry.write();
            reg.insert(unit_name.to_string(), inst);
        }

        if has_condition {
            let arm_result = arm_unit(&self.shared, unit_name);
            let mut reg = self.shared.registry.write();
            if let Some(inst) = reg.get_mut(unit_name) {
                match arm_result {
                    Ok(()) => {
                        inst.state = PathState::Waiting;
                        inst.last_error = None;
                    }
                    Err(e) => {
                        inst.state = PathState::Failed;
                        inst.last_error = Some(sysa::l10n::fmt(
                            sysa::l10n::t_("Failed to arm watches: {e}"),
                            &[("e", &e.to_string())],
                        ));
                    }
                }
            }
        }

        publish_path(&self.shared, unit_name);

        if has_condition {
            let failed = {
                let reg = self.shared.registry.read();
                reg.get(unit_name)
                    .map(|i| i.state == PathState::Failed)
                    .unwrap_or(false)
            };
            if failed {
                let err = {
                    let reg = self.shared.registry.read();
                    reg.get(unit_name)
                        .and_then(|i| i.last_error.clone())
                        .unwrap_or_default()
                };
                anyhow::bail!(sysa::l10n::fmt(
                    sysa::l10n::t_("Cannot arm path unit '{unit_name}': {err}"),
                    &[
                        ("unit_name", &unit_name.to_string()),
                        ("err", &err.to_string())
                    ]
                ));
            }
            recheck_level(&self.shared, unit_name);
        }
        info!("Path unit '{unit_name}' started (target '{target_unit}')");
        Ok(())
    }

    async fn stop(&self, unit_name: &str) -> Result<()> {
        self.shared.backend.disarm(unit_name);
        {
            let mut reg = self.shared.registry.write();
            let inst = reg.get_mut(unit_name).ok_or_else(|| {
                anyhow!(sysa::l10n::fmt(
                    sysa::l10n::t_("Unknown path unit '{unit_name}'"),
                    &[("unit_name", &unit_name.to_string())]
                ))
            })?;
            inst.state = PathState::Dead;
            inst.last_error = None;
            inst.trigger_times.clear();
        }
        publish_path(&self.shared, unit_name);
        Ok(())
    }

    async fn restart(&self, unit_name: &str, config: &[u8], invocation_id: &str) -> Result<()> {
        self.stop(unit_name).await?;
        self.start(unit_name, config, invocation_id).await
    }

    async fn reload(&self, unit_name: &str, config: &[u8]) -> Result<()> {
        let cfg = decode_unit_config(config)?;
        let path_cfg = cfg.path.clone().ok_or_else(|| {
            anyhow!(sysa::l10n::fmt(
                sysa::l10n::t_("No PathConfig for '{unit_name}'"),
                &[("unit_name", &unit_name.to_string())]
            ))
        })?;

        let specs = build_specs(&path_cfg);
        let specs_empty = specs.is_empty();
        let rearm = {
            let mut reg = self.shared.registry.write();
            let inst = reg.get_mut(unit_name).ok_or_else(|| {
                anyhow!(sysa::l10n::fmt(
                    sysa::l10n::t_("Unknown path unit '{unit_name}'"),
                    &[("unit_name", &unit_name.to_string())]
                ))
            })?;
            inst.config = path_cfg.clone();
            inst.specs = specs;
            if !path_cfg.unit.is_empty() {
                inst.target_unit = path_cfg.unit.clone();
            }
            if specs_empty {
                inst.state = PathState::Failed;
                inst.last_error = Some(
                    sysa::l10n::t_(
                        "Path unit has no PathExists=/PathChanged=/DirectoryNotEmpty= condition",
                    )
                    .to_string(),
                );
                false
            } else {
                inst.state == PathState::Waiting
            }
        };

        if rearm {
            self.shared.backend.disarm(unit_name);
            match arm_unit(&self.shared, unit_name) {
                Ok(()) => recheck_level(&self.shared, unit_name),
                Err(e) => {
                    let mut reg = self.shared.registry.write();
                    if let Some(inst) = reg.get_mut(unit_name) {
                        inst.state = PathState::Failed;
                        inst.last_error = Some(sysa::l10n::fmt(
                            sysa::l10n::t_("Failed to arm watches: {e}"),
                            &[("e", &e.to_string())],
                        ));
                    }
                }
            }
        }
        publish_path(&self.shared, unit_name);
        Ok(())
    }

    async fn sync_state(&self) -> Vec<UnitStatus> {
        let reg = self.shared.registry.read();
        reg.iter()
            .map(|(name, inst)| status_of(name, inst))
            .collect()
    }
}

/// Build the watch specs from a [`PathConfig`] in declaration order.
fn build_specs(cfg: &PathConfig) -> Vec<PathSpec> {
    let mut specs = Vec::new();
    for p in &cfg.path_exists {
        specs.push(PathSpec {
            kind: PathSpecKind::Exists,
            path: p.clone(),
        });
    }
    for p in &cfg.path_exists_glob {
        specs.push(PathSpec {
            kind: PathSpecKind::ExistsGlob,
            path: p.clone(),
        });
    }
    for p in &cfg.path_changed {
        specs.push(PathSpec {
            kind: PathSpecKind::Changed,
            path: p.clone(),
        });
    }
    for p in &cfg.path_modified {
        specs.push(PathSpec {
            kind: PathSpecKind::Modified,
            path: p.clone(),
        });
    }
    for p in &cfg.directory_not_empty {
        specs.push(PathSpec {
            kind: PathSpecKind::DirectoryNotEmpty,
            path: p.clone(),
        });
    }
    specs
}

/// Default target: the sibling `.service` with the same base name.
fn default_target_unit(unit_name: &str) -> String {
    match unit_name.strip_suffix(".path") {
        Some(stem) => format!("{stem}.service"),
        None => unit_name.to_string(),
    }
}

/// Parse an octal directory mode string ("0755") with a 0755 fallback.
fn parse_directory_mode(s: &str) -> u32 {
    u32::from_str_radix(s.trim(), 8).unwrap_or(0o755)
}

/// Parent directory of a watch path (mirrors backend watch targets).
fn parent_dir_of(path: &str) -> Option<PathBuf> {
    match path.rfind('/') {
        Some(0) => Some(std::path::PathBuf::from("/")),
        Some(idx) => Some(std::path::PathBuf::from(&path[..idx])),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_target_replaces_suffix() {
        assert_eq!(default_target_unit("foo.path"), "foo.service");
        assert_eq!(default_target_unit("a/b/c.path"), "a/b/c.service");
        assert_eq!(default_target_unit("foo.service"), "foo.service");
    }

    #[test]
    fn directory_mode_parsing() {
        assert_eq!(parse_directory_mode("0755"), 0o755);
        assert_eq!(parse_directory_mode("0700"), 0o700);
        assert_eq!(parse_directory_mode(""), 0o755);
        assert_eq!(parse_directory_mode("not-a-mode"), 0o755);
    }

    #[test]
    fn parent_dir_splitting() {
        assert_eq!(parent_dir_of("/var/spool/app"), Some(PathBuf::from("/var/spool")));
        assert_eq!(parent_dir_of("/file"), Some(PathBuf::from("/")));
        assert_eq!(parent_dir_of("relative"), None);
    }

    #[test]
    fn build_specs_order_and_kinds() {
        let cfg = PathConfig {
            path_exists: vec!["/a".into()],
            path_changed: vec!["/b".into()],
            path_modified: vec!["/c".into()],
            directory_not_empty: vec!["/d".into()],
            path_exists_glob: vec!["/e/*.txt".into()],
            ..Default::default()
        };
        let specs = build_specs(&cfg);
        assert_eq!(specs.len(), 5);
        assert_eq!(specs[0].kind, PathSpecKind::Exists);
        assert_eq!(specs[1].kind, PathSpecKind::ExistsGlob);
        assert_eq!(specs[2].kind, PathSpecKind::Changed);
        assert_eq!(specs[3].kind, PathSpecKind::Modified);
        assert_eq!(specs[4].kind, PathSpecKind::DirectoryNotEmpty);
    }
}
