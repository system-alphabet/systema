//! Unit-name generation and escaping for device units.
//!
//! A real device `/dev/sda` becomes the unit `dev-sda.device`.  Names must
//! be stable and reversible so the engine can both enumerate the injected
//! units and match a `.device` file unit back to a concrete device node.

/// Escape a `/dev` node basename into the "dev-" segment of a unit name.
///
/// Non-alphanumeric characters (including `-` which would be ambiguous with
/// the "dev-" prefix) are replaced with `-`.  The result is lowercase.
pub fn sanitize_node(basename: &str) -> String {
    let mut out = String::with_capacity(basename.len() + 4);
    for c in basename.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c.to_ascii_lowercase());
        } else {
            out.push('-');
        }
    }
    out
}

/// The canonical unit id for a device node, e.g. "sda" -> "dev-sda.device".
pub fn unit_name_for_node(basename: &str) -> String {
    format!("dev-{}.device", sanitize_node(basename))
}

/// Translate a fully generic `.device` unit name back into the expected
/// `/dev` basename, if the name follows the "dev-<node>.device" convention.
pub fn node_from_unit_name(unit_name: &str) -> Option<String> {
    let rest = unit_name.strip_prefix("dev-")?.strip_suffix(".device")?;
    if rest.is_empty() || !rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return None;
    }
    Some(rest.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_roundtrip() {
        assert_eq!(unit_name_for_node("sda"), "dev-sda.device");
        assert_eq!(unit_name_for_node("nvme0n1"), "dev-nvme0n1.device");
        assert_eq!(unit_name_for_node("ttyS0"), "dev-ttys0.device");
        assert_eq!(node_from_unit_name("dev-sda.device"), Some("sda".to_string()));
        assert_eq!(node_from_unit_name("dev-sda.service"), None);
        assert_eq!(node_from_unit_name("sda.device"), None);
    }

    #[test]
    fn escapes_unsafe_chars() {
        assert_eq!(unit_name_for_node("a b"), "dev-a-b.device");
        assert_eq!(unit_name_for_node("a=b/c"), "dev-a-b-c.device");
    }
}