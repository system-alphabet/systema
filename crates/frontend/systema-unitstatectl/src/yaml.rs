//! Minimal block-style YAML emitter with optional ANSI highlighting, tuned
//! for the unit-state snapshots produced by the System Allocator.
//!
//! The output is deterministic: object keys are always emitted in sorted
//! order.  Highlighting is applied only when the caller passes `color: true`
//! (the CLI resolves it via `--color={auto,never,always}`, where `auto`
//! detects whether stdout is a terminal).  The escape sequences themselves
//! are produced by the `colored` crate — nothing here hardcodes ANSI bytes.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use colored::Colorize;
use serde_json::Value;

/// Render one unit as a YAML document:
///
/// ```yaml
/// ---
/// foo.service:
///   active_state: active
///   ...
/// ```
///
/// When `color` is false the output is plain ASCII; when true it is styled
/// for a terminal.
pub fn render_doc(
    w: &mut String,
    name: &str,
    value: &Value,
    color: bool,
) -> std::fmt::Result {
    writeln!(w, "{}", style(color, "---", Style::Dim))?;
    writeln!(
        w,
        "{}",
        style(color, &format!("{}:", name.trim()), Style::UnitName)
    )?;
    match value {
        Value::Object(map) => emit_map(w, map, 2, color),
        _ => {
            let mut line = String::new();
            emit_scalar_inline(&mut line, value, color);
            writeln!(w, "  {line}")
        }
    }
}

fn emit_map(
    w: &mut String,
    map: &serde_json::Map<String, Value>,
    indent: usize,
    color: bool,
) -> std::fmt::Result {
    let keys: BTreeSet<&String> = map.keys().collect();
    for key in keys {
        let value = &map[key];
        match value {
            Value::Object(child) if !child.is_empty() => {
                writeln!(w, "{}{}:", pad(indent), style(color, key, Style::Key))?;
                emit_map(w, child, indent + 2, color)?;
            }
            Value::Array(child) if !child.is_empty() => {
                writeln!(w, "{}{}:", pad(indent), style(color, key, Style::Key))?;
                emit_array(w, child, indent + 2, color)?;
            }
            Value::Object(_) => {
                writeln!(
                    w,
                    "{}{}: {}",
                    pad(indent),
                    style(color, key, Style::Key),
                    style(color, "{}", Style::Dim)
                )?;
            }
            Value::Array(_) => {
                writeln!(
                    w,
                    "{}{}: {}",
                    pad(indent),
                    style(color, key, Style::Key),
                    style(color, "[]", Style::Dim)
                )?;
            }
            other => {
                let mut v = String::new();
                emit_scalar_inline(&mut v, other, color);
                writeln!(w, "{}{}: {v}", pad(indent), style(color, key, Style::Key))?;
            }
        }
    }
    Ok(())
}

fn emit_array(w: &mut String, items: &[Value], indent: usize, color: bool) -> std::fmt::Result {
    for item in items {
        match item {
            Value::Object(child) => {
                if child.is_empty() {
                    writeln!(
                        w,
                        "{}- {}",
                        pad(indent),
                        style(color, "{}", Style::Dim)
                    )?;
                } else {
                    writeln!(w, "{}- ", pad(indent))?;
                    emit_map(w, child, indent + 2, color)?;
                }
            }
            Value::Array(child) if !child.is_empty() => {
                emit_array(w, child, indent + 2, color)?;
            }
            other => {
                let mut v = String::new();
                emit_scalar_inline(&mut v, other, color);
                writeln!(w, "{}- {v}", pad(indent))?;
            }
        }
    }
    Ok(())
}

fn emit_scalar_inline(out: &mut String, value: &Value, color: bool) {
    match value {
        Value::Null => out.push_str(&style(color, "null", Style::Dim)),
        Value::Bool(b) => out.push_str(&style(color, if *b { "true" } else { "false" }, Style::Bool)),
        Value::Number(n) => out.push_str(&style(color, &n.to_string(), Style::Number)),
        Value::String(s) if plain_safe(s) => out.push_str(&style(color, s, Style::Scalar)),
        Value::String(s) => out.push_str(&style(color, &quote(s), Style::Scalar)),
        _ => out.push_str(&value.to_string()),
    }
}

/// Double-quote a string using JSON escaping, which is a valid YAML
/// double-quoted scalar (YAML's escape set is a superset of JSON's).
fn quote(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

/// True if the string can be emitted as a plain (unquoted) YAML scalar.
fn plain_safe(s: &str) -> bool {
    if s.is_empty() || s.trim() != s {
        return false;
    }
    // Multi-line values are always quoted (escapes), never block literals.
    if s.contains('\n') || s.contains('\t') {
        return false;
    }
    // YAML indicators at the start of a plain scalar must be quoted.
    let first = s.chars().next().unwrap();
    if "-?:,[]{}#&*!|>'\"%@`".contains(first) {
        return false;
    }
    // Things that would be re-interpreted as other types.
    if ["null", "true", "false", "yes", "no", "on", "off", "~"]
        .contains(&s.to_ascii_lowercase().as_str())
    {
        return false;
    }
    if s.chars().next().unwrap().is_ascii_digit()
        || ((s.starts_with('-') || s.starts_with('+'))
            && s.chars().nth(1).is_some_and(|c| c.is_ascii_digit()))
    {
        return false;
    }
    // "key: value" or end-of-line "# comment" markers must be quoted.
    if s.contains(": ") || s.contains(" #") || s.ends_with(':') {
        return false;
    }
    // Control characters are never plain.
    !s.chars().any(|c| c.is_control())
}

fn pad(indent: usize) -> String {
    " ".repeat(indent)
}

/// Semantic styles used by the emitter.
enum Style {
    /// Faint/dim text (`---`, empty `{}`/`[]`, `null`).
    Dim,
    /// Section keys in bold cyan.
    Key,
    /// The unit name heading in bold yellow.
    UnitName,
    /// Numbers in yellow.
    Number,
    /// Plain/quoted string values in green.
    Scalar,
    /// Boolean values in magenta.
    Bool,
}

/// Apply `s` when color output is enabled; return `text` unchanged
/// otherwise.  Escape sequences are delegated to the `colored` crate, so no
/// ANSI bytes are hardcoded here.
fn style(color: bool, text: &str, s: Style) -> String {
    if !color {
        return text.to_string();
    }
    match s {
        Style::Dim => text.dimmed().to_string(),
        Style::Key => text.bold().cyan().to_string(),
        Style::UnitName => text.bold().yellow().to_string(),
        Style::Number => text.yellow().to_string(),
        Style::Scalar => text.green().to_string(),
        Style::Bool => text.magenta().to_string(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_nested_yaml_block() {
        let doc = json!({
            "active_state": "active",
            "unit": { "after": ["a.service", "b.service"], "description": "Foo" },
            "jobs": [],
        });
        let mut out = String::new();
        render_doc(&mut out, "foo.service", &doc, false).unwrap();
        let expected = "---
foo.service:
  active_state: active
  jobs: []
  unit:
    after:
      - a.service
      - b.service
    description: Foo
";
        assert_eq!(out, expected);
    }

    #[test]
    fn quotes_ambiguous_scalars() {
        let doc = json!({ "v": "123", "s": "leave me alone", "n": "null", "c": "a#b" });
        let mut out = String::new();
        render_doc(&mut out, "x.service", &doc, false).unwrap();
        assert!(out.contains("v: \"123\""));
        assert!(out.contains("s: leave me alone"));
        assert!(out.contains("n: \"null\""));
        // "a#b" is a valid plain scalar (`#` only starts a comment after
        // whitespace), so it must NOT be quoted.
        assert!(out.contains("c: a#b"));
    }

    #[test]
    fn color_flag_switches_highlighting() {
        let doc = json!({ "active_state": "active" });
        let mut plain = String::new();
        render_doc(&mut plain, "foo.service", &doc, false).unwrap();

        // Exercise the styled path deterministically by forcing the global
        // `colored` override on for the duration of this test.
        colored::control::set_override(true);
        let mut colored_out = String::new();
        let colored_result = render_doc(&mut colored_out, "foo.service", &doc, true);
        colored::control::unset_override();

        assert!(colored_result.is_ok());
        assert!(colored_out.starts_with("\u{1b}["));
        assert!(colored_out.contains("\u{1b}[1;36m"));
        // Plain output must never contain escape sequences.
        assert!(!plain.contains("\u{1b}["));
        // Stripping the escape sequences must yield the plain document.
        let mut stripped = String::new();
        let mut in_escape = false;
        for c in colored_out.chars() {
            if in_escape {
                if c == 'm' {
                    in_escape = false;
                }
            } else if c == '\u{1b}' {
                in_escape = true;
            } else {
                stripped.push(c);
            }
        }
        assert_eq!(stripped, plain);
    }

    #[test]
    fn plain_scalar_survives_round_trip() {
        // Every scalar must be emitted in a form that never gets
        // re-interpreted by a YAML parser (i.e. as a string, not a number,
        // bool, comment or mapping).
        for s in [
            "a:b",
            "  x",
            "- dash",
            "1.5s",
            "yes",
            "true",
            "#comment",
            "with\\slash",
            "a b c",
        ] {
            let doc = json!({ "k": s });
            let mut out = String::new();
            render_doc(&mut out, "u.service", &doc, false).unwrap();
            let line = out.lines().skip(2).next().unwrap();
            let val = line.split_once(": ").map(|(_, v)| v).unwrap_or("");
            if val.starts_with('"') {
                let parsed: Value = serde_json::from_str(val).unwrap();
                assert_eq!(parsed.as_str().unwrap(), s, "{s:?} => {line}");
            } else if matches!(val, "yes" | "true" | "false" | "null" | "~") {
                panic!("{s:?} emitted ambiguously as plain scalar: {line}");
            }
        }
    }
}
