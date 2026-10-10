//! Evaluation of `.device` unit match rules against discovered devices.
//!
//! Mirrors systemd's `[Device]` section semantics:
//! - All specified rules must match (AND).
//! - `DeviceName=`/`DevicePath=` are shell-style globs over the node
//!   basename / `/dev` path.
//! - `SysfsPath=` is a glob over the canonical sysfs device path.
//! - `Property=` clauses require a sysfs property (`KEY` or `KEY=VALUE`).
//!
//! When every rule is unset the rule set is not satisfiable on its own; the
//! caller treats those units as implicit — matched purely by unit name.

use sysa::proto::DeviceConfig;

use crate::discovery::DeviceMeta;

/// Does this device satisfy every rule in `cfg`?
pub fn device_matches(cfg: &DeviceConfig, dev: &DeviceMeta) -> bool {
    if !cfg.device_name.is_empty()
        && !glob_match(&cfg.device_name, &dev.node) {
            return false;
        }
    if !cfg.device_path.is_empty()
        && !glob_match(&cfg.device_path, &dev.dev_file) {
            return false;
        }
    if !cfg.sysfs_path.is_empty() {
        let Some(sp) = &dev.sysfs_path else {
            return false;
        };
        if !glob_match(&cfg.sysfs_path, sp) {
            return false;
        }
    }
    for prop in &cfg.property {
        if !property_matches(prop, &dev.properties) {
            return false;
        }
    }
    true
}

/// Whether any rule is specified at all (an all-empty config is "match all").
pub fn has_rules(cfg: &DeviceConfig) -> bool {
    !cfg.device_name.is_empty()
        || !cfg.device_path.is_empty()
        || !cfg.sysfs_path.is_empty()
        || !cfg.property.is_empty()
}

/// `KEY` requires the property to exist; `KEY=VALUE` also requires the value.
fn property_matches(spec: &str, props: &std::collections::HashMap<String, String>) -> bool {
    match spec.split_once('=') {
        Some((key, want)) => props
            .get(&norm_key(key))
            .map(|v| v == want || glob_match(want, v))
            .unwrap_or(false),
        None => props.contains_key(&norm_key(spec)),
    }
}

/// Property keys are uppercase in `/sys` `uevent` files but admins often
/// write them lowercase in unit files.
fn norm_key(key: &str) -> String {
    key.trim().to_ascii_uppercase()
}

/// Small shell-style glob supporting `*`, `?` and `[...]` character sets.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let txt: Vec<char> = text.chars().collect();
    let (m, n) = (pat.len(), txt.len());
    let (mut i, mut j) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut mark = 0usize;
    while j < n {
        if i < m {
            let c = pat[i];
            if c == '*' {
                star = Some(i);
                mark = j;
                i += 1;
                continue;
            }
            if c == '?' {
                i += 1;
                j += 1;
                continue;
            }
            if c == '[' {
                if let Some((matched, next)) = char_class(&pat, i, txt[j]) {
                    i = next;
                    j += 1;
                    if !matched {
                        return false;
                    }
                    continue;
                }
            }
            if c == txt[j] {
                i += 1;
                j += 1;
                continue;
            }
        }
        if let Some(s) = star {
            i = s + 1;
            mark += 1;
            j = mark;
            continue;
        }
        return false;
    }
    while i < m && pat[i] == '*' {
        i += 1;
    }
    i == m
}

/// Evaluate a `[a-z0-9]` character set at `pos` against `c`.
/// Returns `(matched, position_after_set)` or None if malformed.
fn char_class(pat: &[char], pos: usize, c: char) -> Option<(bool, usize)> {
    if pos + 1 >= pat.len() || pat[pos + 1] == ']' {
        return None;
    }
    let mut k = pos + 1;
    let mut matched = false;
    let mut negate = false;
    if pat[k] == '^' || pat[k] == '!' {
        negate = true;
        k += 1;
    }
    let mut first = true;
    loop {
        if k >= pat.len() {
            return None;
        }
        if pat[k] == ']' && !first {
            break;
        }
        first = false;
        let lo = pat[k];
        if k + 2 < pat.len() && pat[k + 1] == '-' && pat[k + 2] != ']' {
            let hi = pat[k + 2];
            if lo <= c && c <= hi {
                matched = true;
            }
            k += 3;
        } else {
            if lo == c {
                matched = true;
            }
            k += 1;
        }
    }
    Some((matched != negate, k + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_basics() {
        assert!(glob_match("sda", "sda"));
        assert!(glob_match("sd?1", "sda1"));
        assert!(!glob_match("sd?1", "sda2"));
        assert!(glob_match("sd*", "sdb3"));
        assert!(!glob_match("sd*2", "sd1"));
        assert!(glob_match("/dev/sd*", "/dev/sda"));
        assert!(glob_match("hd[ab]1", "hda1"));
        assert!(!glob_match("hd[ab]1", "hdc1"));
        assert!(glob_match("hd[!ab]1", "hdc1"));
    }

    #[test]
    fn property_keys_normalised() {
        let props = [("ID_MODEL".to_string(), "Foo".to_string())]
            .into_iter()
            .collect();
        assert!(property_matches("ID_MODEL=Foo", &props));
        assert!(property_matches("id_model=*", &props));
        assert!(property_matches("ID_MODEL", &props));
        assert!(!property_matches("ID_MODEL=Bar", &props));
        assert!(!property_matches("ID_SERIAL", &props));
    }
}