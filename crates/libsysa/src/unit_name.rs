//! Unit name helpers (mirrors systemd's `unit-name.c` basics).
//!
//! A systemd unit name is `prefix@instance.suffix` for instance units,
//! `prefix@.suffix` for template units, and `prefix.suffix` for plain
//! units.  These helpers classify a name and derive the template name
//! an instance should be loaded from.

/// The characters valid in the prefix / instance part of a unit name
/// (systemd `VALID_CHARS_WITH_AT`).
fn is_valid_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, ':' | '-' | '_' | '.' | '\\' | '@')
}

/// The characters valid in the prefix part only (systemd `VALID_CHARS`).
fn is_valid_prefix_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, ':' | '-' | '_' | '.' | '\\')
}

/// Whether `name` looks like a valid unit name (has a known-looking
/// `prefix[@instance].suffix` shape).  This is intentionally permissive:
/// it does not validate the suffix against the known unit types.
pub fn looks_like_unit_name(name: &str) -> bool {
    if name.is_empty() || name.len() >= 255 {
        return false;
    }
    let Some(dot) = name.rfind('.') else {
        return false;
    };
    if dot == 0 || dot == name.len() - 1 {
        return false;
    }
    let prefix = &name[..dot];
    prefix.chars().all(is_valid_name_char)
}

/// Returns `Some(instance)` if `name` is an instance unit
/// (`foo@bar.service` → `bar`).  Returns `None` for plain and template
/// units (`foo.service`, `foo@.service`) and invalid names.
pub fn instance_of(name: &str) -> Option<String> {
    let at = name.find('@')?;
    let dot = name.rfind('.')?;
    if dot <= at + 1 {
        return None; // template (`foo@.service`) or misplaced `@`
    }
    let instance = &name[at + 1..dot];
    if instance.is_empty() {
        return None;
    }
    if !instance.chars().all(is_valid_name_char) {
        return None;
    }
    Some(instance.to_string())
}

/// Returns `Some(template)` if `name` is an instance unit
/// (`foo@bar.service` → `foo@.service`).  Returns `None` otherwise.
pub fn template_of(name: &str) -> Option<String> {
    let at = name.find('@')?;
    let dot = name.rfind('.')?;
    if dot <= at + 1 {
        return None; // template or plain name
    }
    if !name[..at].chars().all(is_valid_prefix_char) {
        return None;
    }
    Some(format!("{}@{}", &name[..at], &name[dot..]))
}

/// Whether `name` is a template unit (`foo@.service`).
pub fn is_template(name: &str) -> bool {
    let Some(at) = name.find('@') else {
        return false;
    };
    let Some(dot) = name.rfind('.') else {
        return false;
    };
    dot == at + 1
}

/// Whether `name` is an instance unit (`foo@bar.service`).
pub fn is_instance(name: &str) -> bool {
    instance_of(name).is_some()
}

/// Unescape the `\xNN` sequences used in escaped unit names
/// (mirrors systemd's `unit_name_unescape`).  Used to expand the `%I`
/// specifier, which is the instance name with escapes removed.
pub fn unescape(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut chars = name.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('x') => {
                    let hi = chars.next();
                    let lo = chars.next();
                    if let (Some(hi), Some(lo)) = (hi, lo) {
                        if let Ok(byte) = u8::from_str_radix(&format!("{hi}{lo}"), 16) {
                            out.push(byte as char);
                            continue;
                        }
                    }
                    // Malformed escape: emit what we consumed and stop.
                    out.push('\\');
                    out.push('x');
                    if let Some(c) = hi {
                        out.push(c);
                    }
                    if let Some(c) = lo {
                        out.push(c);
                    }
                }
                other => {
                    out.push('\\');
                    if let Some(o) = other {
                        out.push(o);
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Parse the UID out of a per-user slice unit name (`user-<UID>.slice`).
///
/// Session scopes are created under `user-<UID>.slice` (via `Slice=`), which
/// is how logind-style callers tie a login session to its user.  Returns
/// `None` for anything that is not a `user-<digits>.slice` name.
pub fn parse_user_slice_uid(slice_name: &str) -> Option<u32> {
    let stem = slice_name.strip_suffix(".slice")?;
    let uid_part = stem.strip_prefix("user-")?;
    if uid_part.is_empty() {
        return None;
    }
    uid_part.parse::<u32>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_of_classifies() {
        assert_eq!(instance_of("getty@tty3.service").as_deref(), Some("tty3"));
        assert_eq!(instance_of("sshd@1.service").as_deref(), Some("1"));
        assert_eq!(instance_of("sshd@prod.service").as_deref(), Some("prod"));
        assert_eq!(instance_of("plain.service"), None);
        assert_eq!(instance_of("getty@.service"), None);
        assert_eq!(instance_of("no-dot"), None);
    }

    #[test]
    fn template_of_classifies() {
        assert_eq!(
            template_of("getty@tty3.service").as_deref(),
            Some("getty@.service")
        );
        assert_eq!(
            template_of("sshd@prod.service").as_deref(),
            Some("sshd@.service")
        );
        assert_eq!(template_of("plain.service"), None);
        assert_eq!(template_of("getty@.service"), None);
    }

    #[test]
    fn template_and_instance_predicates() {
        assert!(is_template("getty@.service"));
        assert!(!is_template("getty@tty3.service"));
        assert!(!is_template("getty.service"));
        assert!(is_instance("getty@tty3.service"));
        assert!(!is_instance("getty@.service"));
        assert!(!is_instance("getty.service"));
    }

    #[test]
    fn looks_like_unit_name_basics() {
        assert!(looks_like_unit_name("getty@tty3.service"));
        assert!(looks_like_unit_name("sshd.service"));
        assert!(!looks_like_unit_name(""));
        assert!(!looks_like_unit_name(".service"));
        assert!(!looks_like_unit_name("foo."));
        assert!(!looks_like_unit_name("no-suffix"));
    }

    #[test]
    fn unescape_hex_sequences() {
        assert_eq!(unescape("tty\\x2d1"), "tty-1");
        assert_eq!(unescape("dev-sda1"), "dev-sda1");
        assert_eq!(unescape("a\\x2fb"), "a/b");
        assert_eq!(unescape("no escapes"), "no escapes");
        assert_eq!(unescape("\\x20"), " ");
        assert_eq!(unescape("a\\x"), "a\\x");
        assert_eq!(unescape("a\\\\b"), "a\\\\b");
    }

    #[test]
    fn user_slice_uid_parsing() {
        assert_eq!(parse_user_slice_uid("user-1000.slice"), Some(1000));
        assert_eq!(parse_user_slice_uid("user-0.slice"), Some(0));
        assert_eq!(
            parse_user_slice_uid("user-2147483647.slice"),
            Some(2147483647)
        );
        // Not a user slice: plain slices, templates, malformed names.
        assert_eq!(parse_user_slice_uid("system.slice"), None);
        assert_eq!(parse_user_slice_uid("user.slice"), None);
        assert_eq!(parse_user_slice_uid("user-.slice"), None);
        assert_eq!(parse_user_slice_uid("user-abc.slice"), None);
        assert_eq!(parse_user_slice_uid("user--1.slice"), None);
        assert_eq!(
            parse_user_slice_uid("user-4294967295.slice"),
            Some(u32::MAX)
        );
        assert_eq!(parse_user_slice_uid("user-4294967296.slice"), None); // overflows u32
        assert_eq!(parse_user_slice_uid("user-1000.service"), None);
    }
}
