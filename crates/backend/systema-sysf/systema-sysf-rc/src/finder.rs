use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::json;

use systema_sysf::ir::{ConfigMap, DependencySet, UnitIR, UnitType};
use systema_sysf::Finder;

use crate::facilities::{self, ProvidesIndex};
use crate::loader;
use crate::types::{runlevel_target, InitScript, SOURCE_FORMAT};

/// SysV init (`rc`) implementation of the [`Finder`] trait.
///
/// Parses the executable scripts in the init.d search paths and converts
/// them into service units.  Each script becomes a `Type=forking` service
/// whose start/stop/reload actions run the script with the matching LSB
/// verb.  Runlevel membership — `Snn` links in `rcN.d` plus the
/// `Default-Start:` recipe — becomes `WantedBy=` metadata and ordering
/// against the equivalent systemd targets (`multi-user.target`,
/// `graphical.target`, ...).  LSB dependencies become `After=`/`Before=`
/// ordering edges, and `Provides:` names become aliases.
pub struct RcFinder;

impl RcFinder {
    pub fn new() -> Arc<Self> {
        Arc::new(RcFinder)
    }
}

#[async_trait]
impl Finder for RcFinder {
    fn name(&self) -> &str {
        "sysv"
    }

    async fn find_all(&self) -> Result<HashMap<String, UnitIR>> {
        let scripts = loader::discover_all();
        Ok(convert_all(&scripts)
            .into_iter()
            .map(|unit| (unit.id.clone(), unit))
            .collect())
    }

    async fn find_one(&self, id: &str) -> Result<Option<UnitIR>> {
        let scripts = loader::discover_all();
        let units = convert_all(&scripts);
        if let Some(unit) = units.iter().find(|unit| unit.id == id) {
            return Ok(Some(unit.clone()));
        }
        // A `Provides:` name is another name for the script's unit.
        Ok(units
            .iter()
            .find(|unit| unit.aliases.iter().any(|alias| alias == id))
            .cloned())
    }
}

/// Convert every discovered script, then apply the runlevel boot ordering.
pub(crate) fn convert_all(scripts: &[InitScript]) -> Vec<UnitIR> {
    let provides = ProvidesIndex::build(scripts);
    let mut units: Vec<UnitIR> = scripts
        .iter()
        .map(|script| convert_script(script, &provides))
        .collect();
    apply_start_priority_order(&mut units, scripts);
    units
}

/// Convert one script into its service unit.
fn convert_script(script: &InitScript, provides: &ProvidesIndex) -> UnitIR {
    let mut dependencies = DependencySet {
        // SysV services get the normal base/shutdown ordering, exactly like
        // the units systemd-sysv-generator emits.
        default_dependencies: true,
        ..Default::default()
    };

    // Required-Start/Should-Start order the unit after their facilities.
    // Required-Stop/Should-Stop name facilities that must stay up while the
    // service runs, which implies the same start ordering (and therefore
    // stopping before them), so they are folded into the same edge.
    for word in script
        .required_start
        .iter()
        .chain(&script.should_start)
        .chain(&script.required_stop)
        .chain(&script.should_stop)
        .chain(&script.x_start_after)
    {
        add_ordering(&mut dependencies, word, script, provides, false);
    }
    for word in &script.x_start_before {
        add_ordering(&mut dependencies, word, script, provides, true);
    }

    // A service that provides a target makes it available: it is pulled in
    // by the target's users and ordered before it, like systemd does.
    for word in &script.provides {
        for target in facilities::provided_targets(word) {
            dependencies.wants.insert(target.clone());
            dependencies.before.insert(target);
        }
    }

    let mut aliases = Vec::new();
    for word in &script.provides {
        if let Some(alias) = facilities::provided_alias(word, &script.name) {
            if !aliases.contains(&alias) {
                aliases.push(alias);
            }
        }
    }

    let wanted_by = wanted_by(script);
    for target in &wanted_by {
        dependencies.before.insert(target.clone());
    }

    UnitIR {
        id: script.unit_name(),
        unit_type: Some(UnitType::Service),
        description: description(script),
        source_format: Some(SOURCE_FORMAT.to_string()),
        source_path: Some(script.path.clone()),
        drop_in_paths: None,
        owner: None,
        slice: None,
        dependencies: Some(dependencies),
        config: Some(service_config(script)),
        conditions: Some(Vec::new()),
        asserts: Some(Vec::new()),
        wanted_by: Some(wanted_by),
        required_by: Some(Vec::new()),
        enabled: Some(script.enabled),
        unit_file_state: Some(unit_file_state(script).to_string()),
        aliases,
        alias_of: None,
        template: None,
    }
}

/// Add one ordering edge, translating the LSB word first.
fn add_ordering(
    dependencies: &mut DependencySet,
    word: &str,
    script: &InitScript,
    provides: &ProvidesIndex,
    before: bool,
) {
    let Some(reference) = facilities::resolve(word, &script.name, provides) else {
        return;
    };
    // `$network` is the one facility that must be pulled in actively; an
    // `X-Start-Before:` reference only orders the units.
    if reference.pull_in && !before {
        dependencies.wants.insert(reference.unit.clone());
    }
    if before {
        dependencies.before.insert(reference.unit);
    } else {
        dependencies.after.insert(reference.unit);
    }
}

/// `LSB: …` / `SYSV: …`, matching the descriptions systemd's SysV generator
/// gives its wrapper units.
fn description(script: &InitScript) -> Option<String> {
    if script.description.is_empty() {
        return None;
    }
    let prefix = if script.has_lsb { "LSB" } else { "SYSV" };
    Some(format!("{prefix}: {}", script.description))
}

/// The `[Service]` directives that run the script through the LSB verbs.
fn service_config(script: &InitScript) -> ConfigMap {
    let mut config = ConfigMap::new();
    config.insert("Type".to_string(), json!("forking"));
    config.insert(
        "ExecStart".to_string(),
        json!([format!("{} start", script.path)]),
    );
    config.insert(
        "ExecStop".to_string(),
        json!([format!("{} stop", script.path)]),
    );
    if script.supports_reload {
        config.insert(
            "ExecReload".to_string(),
            json!([format!("{} reload", script.path)]),
        );
    }
    if !script.pid_file.is_empty() {
        config.insert("PIDFile".to_string(), json!(script.pid_file));
        config.insert("RemainAfterExit".to_string(), json!(false));
    } else {
        // Without a PID file the forking daemon cannot be tracked, so the
        // unit stays active until it is explicitly stopped (systemd's rule).
        config.insert("RemainAfterExit".to_string(), json!(true));
    }
    config.insert("Restart".to_string(), json!("no"));
    config.insert("KillMode".to_string(), json!("process"));
    // systemd's SysV generator uses TimeoutSec=5min.
    config.insert("TimeoutStartSec".to_string(), json!(300));
    config.insert("TimeoutStopSec".to_string(), json!(300));
    config
}

/// The targets the script belongs to, sorted and deduplicated.
///
/// `rcN.d` links are enablement evidence; `Default-Start:` is the install
/// recipe (the runlevels `update-rc.d` would link it into).  Both describe
/// the same membership, so their targets are unioned.
fn wanted_by(script: &InitScript) -> Vec<String> {
    let mut targets = BTreeSet::new();
    for runlevel in &script.link_runlevels {
        if let Some(target) = runlevel_target(*runlevel) {
            targets.insert(target.to_string());
        }
    }
    for value in &script.default_start {
        let mut chars = value.chars();
        let Some(runlevel) = chars.next() else {
            continue;
        };
        if chars.next().is_some() {
            continue;
        }
        if let Some(target) = runlevel_target(runlevel) {
            targets.insert(target.to_string());
        }
    }
    targets.into_iter().collect()
}

/// `systemctl is-enabled` state: links are enablement, `Default-Start:` is
/// only an install recipe, and a script with neither is static.
fn unit_file_state(script: &InitScript) -> &'static str {
    if script.enabled {
        "enabled"
    } else if script.default_start.is_empty() {
        "static"
    } else {
        "disabled"
    }
}

/// Order scripts that lack LSB headers by their `Snn` priority.
///
/// Two scripts that both carry an LSB header already declare their order
/// explicitly; for every other pair, the higher `Snn` number starts later
/// (systemd-sysv-generator's `fix_order`).
fn apply_start_priority_order(units: &mut [UnitIR], scripts: &[InitScript]) {
    for left in 0..scripts.len() {
        for right in (left + 1)..scripts.len() {
            if scripts[left].has_lsb && scripts[right].has_lsb {
                continue;
            }
            let (Some(left_priority), Some(right_priority)) =
                (scripts[left].start_priority, scripts[right].start_priority)
            else {
                continue;
            };
            let (before, after) = if left_priority < right_priority {
                (left, right)
            } else if right_priority < left_priority {
                (right, left)
            } else {
                continue;
            };
            let earlier = units[before].id.clone();
            let later = units[after].id.clone();
            if let Some(dependencies) = units[after].dependencies.as_mut() {
                dependencies.after.insert(earlier);
            }
            if let Some(dependencies) = units[before].dependencies.as_mut() {
                dependencies.before.insert(later);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runlevel_membership_and_lsb_dependencies_are_converted() {
        let mut sshd = InitScript::new("sshd", "/etc/init.d/sshd");
        sshd.has_lsb = true;
        sshd.provides = vec!["ssh".to_string()];
        sshd.required_start = vec!["$remote_fs".to_string()];
        sshd.default_start = vec![
            "2".to_string(),
            "3".to_string(),
            "4".to_string(),
            "5".to_string(),
        ];
        sshd.link_runlevels = vec!['3'];
        sshd.enabled = true;

        let mut monitor = InitScript::new("monitor", "/etc/init.d/monitor");
        monitor.required_start = vec!["ssh".to_string(), "$network".to_string()];

        let units = convert_all(&[sshd, monitor]);
        let sshd = units
            .iter()
            .find(|unit| unit.id == "sshd.service")
            .expect("sshd unit");
        assert_eq!(sshd.aliases, ["ssh.service"]);
        assert_eq!(
            sshd.wanted_by,
            Some(vec![
                "graphical.target".to_string(),
                "multi-user.target".to_string()
            ])
        );
        assert_eq!(sshd.unit_file_state.as_deref(), Some("enabled"));
        let dependencies = sshd.dependencies.as_ref().expect("dependencies");
        assert!(dependencies.after.contains("remote-fs.target"));
        assert!(dependencies.before.contains("multi-user.target"));
        assert_eq!(
            sshd.config
                .as_ref()
                .and_then(|config| config.get("ExecStart")),
            Some(&json!(["/etc/init.d/sshd start"]))
        );

        let monitor = units
            .iter()
            .find(|unit| unit.id == "monitor.service")
            .expect("monitor unit");
        let dependencies = monitor.dependencies.as_ref().expect("dependencies");
        assert!(dependencies.after.contains("sshd.service"));
        assert!(dependencies.after.contains("network-online.target"));
        assert!(dependencies.wants.contains("network-online.target"));
        assert_eq!(monitor.unit_file_state.as_deref(), Some("static"));
    }

    #[test]
    fn scripts_without_lsb_headers_are_ordered_by_rc_priority() {
        let mut early = InitScript::new("early", "/etc/init.d/early");
        early.start_priority = Some(10);
        let mut late = InitScript::new("late", "/etc/init.d/late");
        late.start_priority = Some(20);

        let units = convert_all(&[late, early]);
        let early = units
            .iter()
            .find(|unit| unit.id == "early.service")
            .expect("early unit");
        let late = units
            .iter()
            .find(|unit| unit.id == "late.service")
            .expect("late unit");
        assert!(early
            .dependencies
            .as_ref()
            .expect("dependencies")
            .before
            .contains("late.service"));
        assert!(late
            .dependencies
            .as_ref()
            .expect("dependencies")
            .after
            .contains("early.service"));
    }
}
