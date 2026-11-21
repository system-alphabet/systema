//! Preset policy files (`*.preset`), mirroring systemd's preset handling.

use super::parse::replace_instance;
use super::{instance_of, is_template, is_valid_unit_name, name_template};

/// The policy action a preset rule asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresetAction {
    Enable,
    Disable,
    Ignore,
}

/// Preset execution mode (`--preset-mode=`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresetMode {
    Full,
    EnableOnly,
    DisableOnly,
}

impl PresetMode {
    /// Parse systemd's preset mode strings (the empty string means `full`).
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "" | "full" => Some(Self::Full),
            "enable-only" => Some(Self::EnableOnly),
            "disable-only" => Some(Self::DisableOnly),
            _ => None,
        }
    }
}

/// One parsed preset rule.
#[derive(Debug, Clone)]
pub struct PresetRule {
    pub pattern: String,
    pub action: PresetAction,
    /// Instance names from `enable template@.service a b c`.
    pub instances: Option<Vec<String>>,
}

/// Directories systemd reads preset files from, highest priority first.
pub const SYSTEM_PRESET_DIRS: &[&str] = &[
    "/etc/systemd/system-preset",
    "/run/systemd/system-preset",
    "/usr/local/lib/systemd/system-preset",
    "/usr/lib/systemd/system-preset",
    "/lib/systemd/system-preset",
];

pub fn read_system_presets() -> Vec<PresetRule> {
    read_presets(SYSTEM_PRESET_DIRS)
}

/// Read and parse the preset files in `dirs` (first directory wins for
/// duplicate file names, files are applied in name order).
pub fn read_presets(dirs: &[&str]) -> Vec<PresetRule> {
    let mut files: Vec<(String, std::path::PathBuf)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".preset") || !seen.insert(name.clone()) {
                continue;
            }
            files.push((name, entry.path()));
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let mut rules = Vec::new();
    for (_, path) in files {
        if let Ok(text) = std::fs::read_to_string(&path) {
            rules.extend(parse_presets(&text));
        }
    }
    rules
}

fn parse_presets(text: &str) -> Vec<PresetRule> {
    let mut rules = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let mut words = line.split_whitespace();
        let Some(verb) = words.next() else {
            continue;
        };
        let rest: Vec<&str> = words.collect();
        if rest.is_empty() {
            continue;
        }
        match verb {
            "enable" => {
                let pattern = rest[0].to_string();
                let instances = if rest.len() > 1 {
                    if !is_template(&pattern) {
                        continue;
                    }
                    Some(rest[1..].iter().map(|s| s.to_string()).collect())
                } else {
                    None
                };
                rules.push(PresetRule {
                    pattern,
                    action: PresetAction::Enable,
                    instances,
                });
            }
            "disable" | "ignore" => {
                rules.push(PresetRule {
                    pattern: rest.join(" "),
                    action: if verb == "disable" {
                        PresetAction::Disable
                    } else {
                        PresetAction::Ignore
                    },
                    instances: None,
                });
            }
            _ => {}
        }
    }
    rules
}

/// Query the policy for `name`: the first matching rule wins, and a missing
/// rule means "enable" (systemd's default).
pub fn query(rules: &[PresetRule], name: &str) -> (PresetAction, Option<Vec<String>>) {
    if !is_valid_unit_name(name) {
        return (PresetAction::Enable, None);
    }
    for rule in rules {
        if let Some(instances) = &rule.instances {
            if let Some(template) = name_template(name) {
                if template == rule.pattern {
                    if is_template(name) {
                        let expanded = instances
                            .iter()
                            .filter_map(|instance| replace_instance(name, instance))
                            .collect::<Vec<_>>();
                        return (rule.action, Some(expanded));
                    }
                    if let Some(instance) = instance_of(name) {
                        if instances.contains(&instance) {
                            return (rule.action, None);
                        }
                    }
                }
            }
        }
        if wildcard_match(&rule.pattern, name) {
            return (rule.action, None);
        }
    }
    (PresetAction::Enable, None)
}

/// Minimal `fnmatch(3)` with `FNM_NOESCAPE`: `*` and `?` are wildcards, every
/// other character matches literally.
fn wildcard_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let mut dp = vec![vec![false; text.len() + 1]; pattern.len() + 1];
    dp[0][0] = true;
    for i in 1..=pattern.len() {
        if pattern[i - 1] == '*' {
            dp[i][0] = dp[i - 1][0];
        }
    }
    for i in 1..=pattern.len() {
        for j in 1..=text.len() {
            dp[i][j] = match pattern[i - 1] {
                '*' => dp[i - 1][j] || dp[i][j - 1],
                '?' => dp[i - 1][j - 1],
                c => dp[i - 1][j - 1] && c == text[j - 1],
            };
        }
    }
    dp[pattern.len()][text.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_matching_rule_wins() {
        let rules =
            parse_presets("# comment\nenable foo.service\nignore bar.service\ndisable *.service\n");
        assert_eq!(query(&rules, "foo.service").0, PresetAction::Enable);
        assert_eq!(query(&rules, "baz.service").0, PresetAction::Disable);
        assert_eq!(query(&rules, "bar.service").0, PresetAction::Ignore);
        assert_eq!(query(&rules, "nothing.target").0, PresetAction::Enable);
    }

    #[test]
    fn template_rule_expands_instances() {
        let rules = parse_presets("enable getty@.service tty1 tty2\n");
        let (action, instances) = query(&rules, "getty@.service");
        assert_eq!(action, PresetAction::Enable);
        assert_eq!(
            instances.unwrap(),
            ["getty@tty1.service", "getty@tty2.service"]
        );
        assert_eq!(query(&rules, "getty@tty1.service").0, PresetAction::Enable);
        assert_eq!(query(&rules, "getty@tty9.service").0, PresetAction::Enable);
    }

    #[test]
    fn wildcard_matching() {
        assert!(wildcard_match("*.service", "foo.service"));
        assert!(wildcard_match("foo?service", "foo.service"));
        assert!(!wildcard_match("*.socket", "foo.service"));
    }
}
