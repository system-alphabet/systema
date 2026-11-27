//! System-state queries whose results may change while System A is running.

pub(super) fn evaluate(kind: &str, value: &str) -> Option<bool> {
    match kind {
        "PathExists" => Some(std::path::Path::new(value).exists()),
        "PathExistsGlob" => Some(path_glob_matches(value)),
        "FileNotEmpty" => Some(
            std::fs::metadata(value)
                .map(|metadata| metadata.is_file() && metadata.len() != 0)
                .unwrap_or(false),
        ),
        "DirectoryNotEmpty" => Some(
            std::fs::read_dir(value)
                .map(|mut entries| entries.next().is_some())
                .unwrap_or(false),
        ),
        "KernelModuleLoaded" => Some(std::path::Path::new("/sys/module").join(value).exists()),
        "ACPower" => Some(is_on_ac_power() == yes_value(value)),
        "FirstBoot" => Some(sysa::firstboot::is_first_boot() == yes_value(value)),
        "Host" => Some(host_matches(value)),
        "Environment" => Some(environment_matches(value)),
        _ => None,
    }
}

/// Interpret a systemd boolean-ish condition value.
fn yes_value(value: &str) -> bool {
    matches!(value.to_ascii_lowercase().as_str(), "yes" | "true" | "1")
}

/// `ConditionHost=`: matches the hostname or the machine ID.
fn host_matches(value: &str) -> bool {
    let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| std::fs::read_to_string("/etc/hostname"))
        .map(|hostname| hostname.trim().to_string())
        .unwrap_or_default();
    let machine_id = std::fs::read_to_string(sysa::paths::instance().machine_id_file)
        .map(|machine_id| machine_id.trim().to_string())
        .unwrap_or_default();
    value == hostname || value == machine_id
}

/// `ConditionEnvironment=`: variable set, or set to the given value.
fn environment_matches(value: &str) -> bool {
    match value.split_once('=') {
        Some((key, expected)) => std::env::var(key)
            .map(|actual| actual == expected)
            .unwrap_or(false),
        None => std::env::var_os(value).is_some(),
    }
}

/// Check if any filesystem path matches a simple glob pattern.
///
/// Uses the same glob logic as the rest of systema (no external crate).
fn path_glob_matches(pattern: &str) -> bool {
    let (directory, file_pattern) = match pattern.rfind('/') {
        Some(position) => (&pattern[..position], &pattern[position + 1..]),
        None => (".", pattern),
    };
    std::fs::read_dir(directory)
        .map(|entries| {
            entries.filter_map(|entry| entry.ok()).any(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .map(|name| simple_glob_match(file_pattern, name))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// Minimal shell-style glob: `*` matches any sequence, `?` matches any char.
fn simple_glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    glob_match_impl(&pattern, &text)
}

fn glob_match_impl(pattern: &[char], text: &[char]) -> bool {
    match (pattern.first(), text.first()) {
        (None, None) => true,
        (Some(&'*'), _) => {
            (0..=text.len()).any(|offset| glob_match_impl(&pattern[1..], &text[offset..]))
        }
        (Some(&'?'), Some(_)) => glob_match_impl(&pattern[1..], &text[1..]),
        (Some(pattern_char), Some(text_char)) if pattern_char == text_char => {
            glob_match_impl(&pattern[1..], &text[1..])
        }
        _ => false,
    }
}

/// Returns `true` if the system appears to be running on AC power.
///
/// Best-effort: returns `true` (assume AC) if the check cannot be performed.
fn is_on_ac_power() -> bool {
    let path = std::path::Path::new("/sys/class/power_supply");
    if !path.exists() {
        return true;
    }
    std::fs::read_dir(path)
        .map(|entries| {
            entries.filter_map(|entry| entry.ok()).any(|entry| {
                std::fs::read_to_string(entry.path().join("online"))
                    .map(|contents| contents.trim() == "1")
                    .unwrap_or(false)
            })
        })
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::simple_glob_match;

    #[test]
    fn test_simple_glob_match_exact() {
        assert!(simple_glob_match("foo.service", "foo.service"));
        assert!(!simple_glob_match("foo.service", "bar.service"));
    }

    #[test]
    fn test_simple_glob_match_wildcard() {
        assert!(simple_glob_match("*.service", "foo.service"));
        assert!(simple_glob_match("foo.*", "foo.service"));
        assert!(!simple_glob_match("*.service", "foo.txt"));
    }

    #[test]
    fn test_simple_glob_match_question_mark() {
        assert!(simple_glob_match("foo.???????", "foo.service"));
        assert!(!simple_glob_match("foo.??????", "foo.service"));
    }

    #[test]
    fn test_simple_glob_match_empty() {
        assert!(simple_glob_match("", ""));
        assert!(!simple_glob_match("", "foo"));
        assert!(!simple_glob_match("foo", ""));
    }
}
