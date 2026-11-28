//! Parser for the metadata headers of a SysV init script.
//!
//! The LSB block (`### BEGIN INIT INFO` … `### END INIT INFO`) carries the
//! dependency and runlevel declarations; older Red Hat style scripts only
//! carry `# description:` / `# pidfile:` comments.  Both forms are read here;
//! interpreting the values into unit dependencies belongs to
//! [`crate::facilities`] and [`crate::finder`].

use std::path::Path;

use anyhow::{Context, Result};

use crate::types::InitScript;

/// Read and parse one init script from `path`.
pub fn parse_script_from_path(name: &str, path: &Path) -> Result<InitScript> {
    let text = std::fs::read_to_string(path).with_context(|| {
        sysa::l10n::fmt(
            sysa::l10n::t_("Failed to read init script {path}."),
            &[("path", &path.display().to_string())],
        )
    })?;
    Ok(parse_script(name, &path.to_string_lossy(), &text))
}

/// Parse an init script's text into its metadata.
pub fn parse_script(name: &str, path: &str, text: &str) -> InitScript {
    let mut script = InitScript::new(name, path);
    let mut in_lsb = false;
    let mut lsb_short = String::new();
    let mut lsb_long = String::new();
    let mut lsb_description_open = false;
    let mut redhat_description = String::new();
    let mut redhat_continuation = false;

    for raw in text.lines() {
        let line = raw.trim_end();
        let lower = line.to_ascii_lowercase();
        if lower.contains("usage") && lower.contains("reload") {
            script.supports_reload = true;
        }

        if line.trim() == "### BEGIN INIT INFO" {
            in_lsb = true;
            script.has_lsb = true;
            lsb_description_open = false;
            continue;
        }
        if in_lsb && line.trim() == "### END INIT INFO" {
            in_lsb = false;
            lsb_description_open = false;
            continue;
        }

        let Some(content) = commented(line) else {
            continue;
        };
        if in_lsb {
            if let Some((key, value)) = content.split_once(':') {
                lsb_description_open = false;
                let value = value.trim();
                match key.trim().to_ascii_lowercase().as_str() {
                    "provides" => script.provides = words(value),
                    "required-start" => script.required_start = words(value),
                    "required-stop" => script.required_stop = words(value),
                    "should-start" => script.should_start = words(value),
                    "should-stop" => script.should_stop = words(value),
                    "x-start-before" => script.x_start_before = words(value),
                    "x-start-after" => script.x_start_after = words(value),
                    "default-start" => script.default_start = words(value),
                    "default-stop" => script.default_stop = words(value),
                    "short-description" => lsb_short = value.to_string(),
                    "description" => {
                        lsb_long = value.to_string();
                        lsb_description_open = true;
                    }
                    _ => {}
                }
            } else if lsb_description_open && !content.is_empty() {
                append_word(&mut lsb_long, content);
            }
            continue;
        }

        if redhat_continuation {
            let (text, continues) = without_continuation(content);
            append_word(&mut redhat_description, text);
            redhat_continuation = continues;
        } else if let Some(value) = header_value(content, "description") {
            let (text, continues) = without_continuation(value);
            redhat_description = text.to_string();
            redhat_continuation = continues;
        } else if let Some(value) = header_value(content, "pidfile") {
            let value = value.trim();
            if value.starts_with('/') {
                script.pid_file = value.to_string();
            }
        }
    }

    // A short description wins over the long one; the Red Hat comment is the
    // fallback for scripts without an LSB block.
    script.description = if !lsb_short.is_empty() {
        lsb_short
    } else if !redhat_description.is_empty() {
        redhat_description
    } else {
        lsb_long
    };
    script.description = script.description.trim().to_string();
    script
}

/// The text of a comment line, without the leading `#`.
fn commented(line: &str) -> Option<&str> {
    line.trim_start().strip_prefix('#').map(str::trim_start)
}

/// `value` of a `Key: value` comment header, with a case-insensitive key.
fn header_value<'a>(content: &'a str, key: &str) -> Option<&'a str> {
    let (head, value) = content.split_once(':')?;
    head.trim().eq_ignore_ascii_case(key).then_some(value)
}

/// Strip a trailing backslash continuation marker.
fn without_continuation(value: &str) -> (&str, bool) {
    let value = value.trim();
    match value.strip_suffix('\\') {
        Some(text) => (text.trim_end(), true),
        None => (value, false),
    }
}

/// Append a space-separated word list to `target`.
fn append_word(target: &mut String, word: &str) {
    let word = word.trim();
    if word.is_empty() {
        return;
    }
    if !target.is_empty() {
        target.push(' ');
    }
    target.push_str(word);
}

/// Split a header value into words (whitespace or comma separated).
fn words(value: &str) -> Vec<String> {
    value
        .split(|c: char| c.is_ascii_whitespace() || c == ',')
        .filter(|word| !word.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsb_header_yields_dependencies_runlevels_and_description() {
        let text = "#!/bin/sh\n\
### BEGIN INIT INFO\n\
# Provides:          ssh\n\
# Required-Start:    $remote_fs $syslog\n\
# Required-Stop:     $remote_fs\n\
# Default-Start:     2 3 4 5\n\
# Default-Stop:      0 1 6\n\
# Short-Description: OpenBSD Secure Shell server\n\
# Description:       Control the ssh daemon\n\
### END INIT INFO\n\
case \"$1\" in reload) ;; esac\n";
        let script = parse_script("sshd", "/etc/init.d/sshd", text);

        assert!(script.has_lsb);
        assert_eq!(script.provides, ["ssh"]);
        assert_eq!(script.required_start, ["$remote_fs", "$syslog"]);
        assert_eq!(script.required_stop, ["$remote_fs"]);
        assert_eq!(script.default_start, ["2", "3", "4", "5"]);
        assert_eq!(script.default_stop, ["0", "1", "6"]);
        assert_eq!(script.description, "OpenBSD Secure Shell server");
        // No usage line advertises reload, so no ExecReload is generated.
        assert!(!script.supports_reload);
    }

    #[test]
    fn redhat_header_is_the_description_fallback() {
        let text = "# description: start the foo daemon \\\n\
#   (and keep it running)\n\
# pidfile: /run/foo.pid\n\
start() { :; }\n";
        let script = parse_script("foo", "/etc/init.d/foo", text);

        assert!(!script.has_lsb);
        assert_eq!(
            script.description,
            "start the foo daemon (and keep it running)"
        );
        assert_eq!(script.pid_file, "/run/foo.pid");
    }

    #[test]
    fn usage_text_advertising_reload_enables_exec_reload() {
        let text = "#!/bin/sh\n# Usage: /etc/init.d/foo {start|stop|reload}\n";
        let script = parse_script("foo", "/etc/init.d/foo", text);
        assert!(script.supports_reload);
    }
}
