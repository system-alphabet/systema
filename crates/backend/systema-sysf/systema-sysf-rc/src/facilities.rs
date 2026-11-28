//! Translation of LSB facility and service names into systemd unit names.
//!
//! LSB headers reference *facilities*: `$remote_fs` and friends are
//! standardized names, `$foo` names a distribution facility, and a bare name
//! is either another init script (optionally one it advertises through
//! `Provides:`) or a plain service name.  The mapping mirrors
//! systemd-sysv-generator, with one addition: a bare name is resolved through
//! the `Provides:` index first, so `Required-Start: ssh` finds the script
//! whose `Provides:` line names `ssh` even when its file is named `sshd`.

use std::collections::HashMap;

use crate::types::InitScript;

/// A unit named by one LSB word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    /// The unit to order against.
    pub unit: String,
    /// Whether the reference must also pull the unit in.  Only
    /// `$network` does (`network-online.target` is not otherwise part of a
    /// boot), exactly like systemd's SysV generator.
    pub pull_in: bool,
}

/// The names init scripts provide, mapped to the script owning them.
///
/// The first declaration in discovery order wins, mirroring the search-path
/// priority the loader applies to the scripts themselves.
#[derive(Debug, Default)]
pub struct ProvidesIndex {
    owners: HashMap<String, String>,
}

impl ProvidesIndex {
    pub fn build(scripts: &[InitScript]) -> Self {
        let mut owners = HashMap::new();
        for script in scripts {
            for word in &script.provides {
                let Some(provided) = provided_name(word) else {
                    continue;
                };
                if provided == script.name {
                    continue;
                }
                owners
                    .entry(provided.to_string())
                    .or_insert_with(|| script.name.clone());
            }
        }
        ProvidesIndex { owners }
    }

    /// The script that provides `name`.
    pub fn owner(&self, name: &str) -> Option<&str> {
        self.owners.get(name).map(String::as_str)
    }
}

/// Resolve one LSB dependency word into a unit reference.
///
/// `own` is the referring script's name stem; a dependency on itself is
/// dropped, as systemd does.
pub fn resolve(word: &str, own: &str, provides: &ProvidesIndex) -> Option<Reference> {
    if let Some(facility) = word.strip_prefix('$') {
        return facility_reference(facility);
    }
    let name = without_script_suffix(word);
    if name.is_empty() || name == own || name.strip_suffix(".service") == Some(own) {
        return None;
    }
    let unit = match provides.owner(name) {
        Some(owner) => format!("{owner}.service"),
        None => unit_name(name),
    };
    sysa::unit_name::looks_like_unit_name(&unit).then_some(Reference {
        unit,
        pull_in: false,
    })
}

/// The targets a `Provides:` word advertises.
///
/// A `$`-prefixed facility is a target; `$network` advertises both
/// `network-online.target` and `network.target`, because a service that
/// provides networking is what makes both available.
pub fn provided_targets(word: &str) -> Vec<String> {
    let Some(facility) = word.strip_prefix('$') else {
        return Vec::new();
    };
    match facility {
        // Part of the base system (or replaced by it): nothing to advertise.
        "local_fs" | "syslog" => Vec::new(),
        "network" => vec![
            "network-online.target".to_string(),
            "network.target".to_string(),
        ],
        "named" => vec!["nss-lookup.target".to_string()],
        "portmap" => vec!["rpcbind.target".to_string()],
        "remote_fs" => vec!["remote-fs.target".to_string()],
        "time" => vec!["time-sync.target".to_string()],
        other => vec![format!("{other}.target")],
    }
}

/// The alias a plain `Provides:` word declares for `own` (`ssh` →
/// `ssh.service`).
pub fn provided_alias(word: &str, own: &str) -> Option<String> {
    let provided = provided_name(word)?;
    if provided == own {
        return None;
    }
    let alias = unit_name(provided);
    sysa::unit_name::looks_like_unit_name(&alias).then_some(alias)
}

/// The service name a `Provides:` word names, or `None` when it is a
/// `$`-prefixed facility (a target).
fn provided_name(word: &str) -> Option<&str> {
    if word.starts_with('$') {
        return None;
    }
    let name = without_script_suffix(word);
    (!name.is_empty()).then_some(name)
}

/// The well-known LSB facilities, plus the `$name` → `name.target`
/// convention for distribution-specific ones.
fn facility_reference(facility: &str) -> Option<Reference> {
    let unit = match facility {
        // Pulled in by the base system; no separate ordering edge.
        "local_fs" | "syslog" => return None,
        "network" => {
            return Some(Reference {
                unit: "network-online.target".to_string(),
                pull_in: true,
            })
        }
        "named" => "nss-lookup.target",
        "portmap" => "rpcbind.target",
        "remote_fs" => "remote-fs.target",
        "time" => "time-sync.target",
        other => {
            return Some(Reference {
                unit: format!("{other}.target"),
                pull_in: false,
            })
        }
    };
    Some(Reference {
        unit: unit.to_string(),
        pull_in: false,
    })
}

/// Strip the `.sh` suffix init script file names may carry.
fn without_script_suffix(word: &str) -> &str {
    word.strip_suffix(".sh").unwrap_or(word)
}

/// Give a bare name a unit suffix unless it already carries one.
fn unit_name(name: &str) -> String {
    if has_unit_suffix(name) {
        name.to_string()
    } else {
        format!("{name}.service")
    }
}

fn has_unit_suffix(name: &str) -> bool {
    matches!(
        name.rsplit('.').next(),
        Some(
            "service"
                | "target"
                | "socket"
                | "timer"
                | "mount"
                | "automount"
                | "path"
                | "slice"
                | "scope"
                | "swap"
                | "device"
                | "power"
        )
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script(name: &str, provides: &[&str]) -> InitScript {
        let mut script = InitScript::new(name, format!("/etc/init.d/{name}"));
        script.provides = provides.iter().map(|word| word.to_string()).collect();
        script
    }

    #[test]
    fn provides_resolves_bare_names() {
        let scripts = [script("sshd", &["ssh"]), script("network-manager", &[])];
        let index = ProvidesIndex::build(&scripts);

        assert_eq!(
            resolve("ssh", "monitor", &index),
            Some(Reference {
                unit: "sshd.service".to_string(),
                pull_in: false,
            })
        );
        // An unknown bare name is a service name by convention.
        assert_eq!(
            resolve("network-manager", "monitor", &index),
            Some(Reference {
                unit: "network-manager.service".to_string(),
                pull_in: false,
            })
        );
        // A reference to itself is dropped.
        assert_eq!(resolve("sshd", "sshd", &index), None);
    }

    #[test]
    fn facilities_map_to_targets() {
        let index = ProvidesIndex::default();
        assert_eq!(
            resolve("$network", "foo", &index),
            Some(Reference {
                unit: "network-online.target".to_string(),
                pull_in: true,
            })
        );
        assert_eq!(
            resolve("$remote_fs", "foo", &index),
            Some(Reference {
                unit: "remote-fs.target".to_string(),
                pull_in: false,
            })
        );
        assert_eq!(resolve("$local_fs", "foo", &index), None);
        assert_eq!(
            resolve("$portmap", "foo", &index).map(|r| r.unit),
            Some("rpcbind.target".to_string())
        );
        assert_eq!(
            resolve("$acme", "foo", &index).map(|r| r.unit),
            Some("acme.target".to_string())
        );
    }

    #[test]
    fn provides_words_split_into_targets_and_aliases() {
        assert_eq!(
            provided_targets("$network"),
            ["network-online.target", "network.target"]
        );
        assert_eq!(
            provided_alias("ssh", "sshd").as_deref(),
            Some("ssh.service")
        );
        assert_eq!(provided_alias("$network", "network"), None);
        assert_eq!(provided_alias("sshd", "sshd"), None);
    }
}
