//! `[Install]` section parsing for the client-side install fallback.
//!
//! Only the directives systemd's `install.c` consumes are read here:
//! `WantedBy=`, `RequiredBy=`, `UpheldBy=`, `Alias=`, `Also=` and
//! `DefaultInstance=`.  Everything else in the unit file belongs to the
//! Finder; this parser exists because the install fallback has to know the
//! recipe `systemctl enable` would follow.

use std::path::Path;

use sysa::unit_name;

/// The `[Install]` section of one unit file (main file plus drop-ins).
#[derive(Debug, Default, Clone)]
pub struct InstallSection {
    pub wanted_by: Vec<String>,
    pub required_by: Vec<String>,
    pub upheld_by: Vec<String>,
    pub aliases: Vec<String>,
    pub also: Vec<String>,
    pub default_instance: Option<String>,
}

impl InstallSection {
    /// Whether the section carries any enablement rule (systemd's
    /// `install_info_has_rules()`; `Also=` and `DefaultInstance=` alone do
    /// not count).
    pub fn has_rules(&self) -> bool {
        !self.wanted_by.is_empty()
            || !self.required_by.is_empty()
            || !self.upheld_by.is_empty()
            || !self.aliases.is_empty()
    }

    /// Whether the section has any installation configuration at all, which
    /// is what `systemctl disable` reports as `carries_install_info`.
    pub fn has_install_info(&self) -> bool {
        self.has_rules() || !self.also.is_empty()
    }

    fn merge(&mut self, other: InstallSection) {
        append_unique(&mut self.wanted_by, other.wanted_by);
        append_unique(&mut self.required_by, other.required_by);
        append_unique(&mut self.upheld_by, other.upheld_by);
        append_unique(&mut self.aliases, other.aliases);
        append_unique(&mut self.also, other.also);
        if other.default_instance.is_some() {
            self.default_instance = other.default_instance;
        }
    }
}

fn append_unique(dst: &mut Vec<String>, src: Vec<String>) {
    for item in src {
        if !dst.contains(&item) {
            dst.push(item);
        }
    }
}

/// Parse the `[Install]` section of a unit file and every drop-in in
/// `dropins`, in the order given (later files override scalars and append to
/// lists, matching systemd's drop-in semantics).
pub fn parse_install(
    path: &Path,
    dropins: &[std::path::PathBuf],
) -> std::io::Result<InstallSection> {
    let mut section = InstallSection::default();
    section.merge(parse_file(path)?);
    for dropin in dropins {
        // A missing drop-in is not an error: the list is built from directory
        // scans and a file may disappear in between.
        if let Ok(parsed) = parse_file(dropin) {
            section.merge(parsed);
        }
    }
    Ok(section)
}

fn parse_file(path: &Path) -> std::io::Result<InstallSection> {
    let text = std::fs::read_to_string(path)?;
    Ok(parse_text(&text))
}

fn parse_text(text: &str) -> InstallSection {
    let mut section = InstallSection::default();
    let mut in_install = false;
    for logical in logical_lines(text) {
        let line = logical.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_install = name.trim().eq_ignore_ascii_case("install");
            continue;
        }
        if !in_install {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key.to_ascii_lowercase().as_str() {
            "wantedby" => section.wanted_by.extend(split_words(value)),
            "requiredby" => section.required_by.extend(split_words(value)),
            "upheldby" => section.upheld_by.extend(split_words(value)),
            "alias" => section.aliases.extend(split_words(value)),
            "also" => section.also.extend(split_words(value)),
            "defaultinstance" => {
                let words = split_words(value);
                section.default_instance = words.into_iter().next();
            }
            _ => {}
        }
    }
    section
}

/// Join continuation lines (`\` at the end) and strip inline comments the way
/// systemd's config parser does for unit files.
fn logical_lines(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for raw in text.lines() {
        let line = raw.trim_end();
        if let Some(prefix) = line.strip_suffix('\\') {
            current.push_str(prefix);
            current.push(' ');
            continue;
        }
        current.push_str(line);
        out.push(std::mem::take(&mut current));
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Split a directive value into whitespace-separated words, honouring single
/// and double quotes (systemd's `extract_first_word`-style lists).
pub fn split_words(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for c in value.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => current.push(c),
            None if c == '\'' || c == '"' => quote = Some(c),
            None if c.is_whitespace() => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            None => current.push(c),
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Expand the specifiers systemd accepts in `[Install]` values.  The
/// instance-related ones (`%i`, `%I`, `%n`, `%N`, `%p`, `%j`) are the only
/// ones install directives realistically use; unknown specifiers are kept
/// verbatim.
pub fn expand_specifiers(value: &str, name: &str, default_instance: Option<&str>) -> String {
    let prefix = unit_prefix(name);
    let instance = unit_name::instance_of(name)
        .or_else(|| default_instance.map(str::to_string))
        .unwrap_or_default();
    let last_component = prefix.rsplit('-').next().unwrap_or(&prefix).to_string();
    let name_with_instance = if unit_name::is_template(name) {
        default_instance
            .and_then(|inst| replace_instance(name, inst))
            .unwrap_or_else(|| name.to_string())
    } else {
        name.to_string()
    };
    let prefix_and_instance = if prefix.is_empty() {
        instance.clone()
    } else if unit_name::is_template(name) && !instance.is_empty() {
        format!("{prefix}@{instance}")
    } else {
        prefix.clone()
    };

    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('%') => out.push('%'),
            Some('n') => out.push_str(&name_with_instance),
            Some('N') => out.push_str(&prefix_and_instance),
            Some('p') => out.push_str(&prefix),
            Some('i') => out.push_str(&instance),
            Some('I') => out.push_str(&unit_name::unescape(&instance)),
            Some('j') => out.push_str(&last_component),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    out
}

/// Prefix of a unit name: everything before the first `@`, or before the last
/// `.` for plain units.
pub fn unit_prefix(name: &str) -> String {
    match name.find('@') {
        Some(at) => name[..at].to_string(),
        None => match name.rfind('.') {
            Some(dot) => name[..dot].to_string(),
            None => name.to_string(),
        },
    }
}

/// Replace the instance of a template name (`getty@.service` +
/// `tty1` → `getty@tty1.service`).
pub fn replace_instance(template: &str, instance: &str) -> Option<String> {
    if !unit_name::is_template(template) {
        return None;
    }
    let at = template.find('@')?;
    let dot = template.rfind('.')?;
    Some(format!(
        "{}@{}{}",
        &template[..at],
        instance,
        &template[dot..]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_install_directives_and_continuations() {
        let text = "\
[Unit]
Description=x

[Install]
WantedBy=multi-user.target graphical.target
RequiredBy=a.service
Also=other.service \\
     third.service
Alias=alias.service
DefaultInstance=tty1
";
        let section = parse_text(text);
        assert_eq!(section.wanted_by, ["multi-user.target", "graphical.target"]);
        assert_eq!(section.required_by, ["a.service"]);
        assert_eq!(section.also, ["other.service", "third.service"]);
        assert_eq!(section.aliases, ["alias.service"]);
        assert_eq!(section.default_instance.as_deref(), Some("tty1"));
        assert!(section.has_rules());
        assert!(section.has_install_info());
    }

    #[test]
    fn expands_instance_specifiers() {
        assert_eq!(
            expand_specifiers("multi-user.target", "foo.service", None),
            "multi-user.target"
        );
        assert_eq!(
            expand_specifiers("%n.wants", "getty@tty1.service", None),
            "getty@tty1.service.wants"
        );
        assert_eq!(
            expand_specifiers("%i.target", "getty@tty1.service", None),
            "tty1.target"
        );
        assert_eq!(
            expand_specifiers("%n", "getty@.service", Some("tty1")),
            "getty@tty1.service"
        );
        assert_eq!(expand_specifiers("100%%", "a.service", None), "100%");
    }

    #[test]
    fn quoted_words_are_split() {
        assert_eq!(
            split_words("a.service \"b c.service\" d.service"),
            ["a.service", "b c.service", "d.service"]
        );
    }
}
