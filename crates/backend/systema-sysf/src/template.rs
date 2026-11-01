//! Template instantiation over the unified IR.
//!
//! A template definition (`getty@.service`) is reported as a [`UnitIR`] whose
//! string values keep the instance specifiers (`%i`, `%I`) unexpanded.  A
//! consumer that wants a concrete instance calls [`instantiate`] with the
//! instance name and its parameters; the result is an ordinary, runnable IR
//! that can be applied like any other definition.
//!
//! The expansion is deliberately format-neutral: it understands the
//! name-derived specifiers every producer can supply (`%n`, `%N`, `%p`, `%P`,
//! `%i`, `%I`, `%%`) and leaves anything else untouched, so a producer that
//! pre-expanded its own specifiers keeps those values.

use serde_json::Value;

use crate::ir::{TemplateInfo, UnitIR};

/// Materialize an instance of `template` as `name`.
///
/// Every string value in the template is expanded with the instance
/// parameters: the first parameter is the `%i` instance string (systemd's
/// single-parameter syntax), and `%I` is its unescaped form.  The parameters
/// are recorded on the result so a later consumer can re-derive them without
/// re-parsing the name.
///
/// A template that is already an instance — or a plain unit — is turned into
/// an instance of itself; the function never fails, so a producer cannot wedge
/// a lookup with a malformed template.
pub fn instantiate(template: &UnitIR, name: &str, parameters: &[String]) -> UnitIR {
    let mut ir = template.clone();
    ir.id = name.to_string();
    ir.template = Some(TemplateInfo {
        definition: false,
        of: Some(template.id.clone()),
        parameters: parameters.to_vec(),
    });

    let specifiers = Specifiers::for_unit(name, parameters);
    expand_strings(&mut ir, &specifiers);

    // Identity fields are not subject to expansion: a `%` in a unit name is
    // not a specifier, and the template's own name is the source of `%p`/`%n`.
    ir.id = name.to_string();
    if let Some(info) = &mut ir.template {
        info.definition = false;
        info.of = Some(template.id.clone());
        info.parameters = parameters.to_vec();
    }
    ir
}

/// Expand every string value reachable from `ir`.
///
/// The IR is a plain serde model, so a `serde_json` round-trip is the one
/// place that covers all of its (many) string fields without maintaining a
/// field-by-field list that silently misses new directives.
fn expand_strings(ir: &mut UnitIR, specifiers: &Specifiers<'_>) {
    let Ok(mut value) = serde_json::to_value(&*ir) else {
        return;
    };
    expand_value(&mut value, specifiers);
    if let Ok(expanded) = serde_json::from_value::<UnitIR>(value) {
        *ir = expanded;
    }
}

fn expand_value(value: &mut Value, specifiers: &Specifiers<'_>) {
    match value {
        Value::String(text) => {
            if text.contains('%') {
                *text = specifiers.expand(text);
            }
        }
        Value::Array(items) => {
            for item in items {
                expand_value(item, specifiers);
            }
        }
        Value::Object(map) => {
            for item in map.values_mut() {
                expand_value(item, specifiers);
            }
        }
        _ => {}
    }
}

/// The name-derived values a template's specifiers expand to.
struct Specifiers<'a> {
    /// Full unit name (`getty@tty3.service`).
    name: &'a str,
    /// Name without the type suffix (`getty@tty3`).
    stem: &'a str,
    /// Prefix before `@` (`getty`).
    prefix: &'a str,
    /// Instance string (`tty3`).
    instance: &'a str,
}

impl<'a> Specifiers<'a> {
    fn for_unit(name: &'a str, parameters: &'a [String]) -> Self {
        let stem = name.rsplit_once('.').map(|(stem, _)| stem).unwrap_or(name);
        let (prefix, name_instance) = stem
            .split_once('@')
            .map(|(prefix, instance)| (prefix, instance))
            .unwrap_or((stem, ""));
        let instance = parameters
            .first()
            .map(String::as_str)
            .unwrap_or(name_instance);
        Specifiers {
            name,
            stem,
            prefix,
            instance,
        }
    }

    fn expand(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch != '%' {
                out.push(ch);
                continue;
            }
            match chars.next() {
                None => out.push('%'),
                Some('n') => out.push_str(self.name),
                Some('N') => out.push_str(self.stem),
                Some('p') => out.push_str(self.prefix),
                Some('P') => out.push_str(&sysa::unit_name::unescape(self.prefix)),
                Some('i') => out.push_str(self.instance),
                Some('I') => out.push_str(&sysa::unit_name::unescape(self.instance)),
                Some('%') => out.push('%'),
                Some(other) => {
                    // Not a name-derived specifier: the producer either
                    // expanded it already or owns its meaning.
                    out.push('%');
                    out.push(other);
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{DependencySet, ExecCommand, ServiceConfig};

    fn template() -> UnitIR {
        UnitIR {
            id: "getty@.service".to_string(),
            unit_type: Some(crate::ir::UnitType::Service),
            description: Some("Getty on %I".to_string()),
            source_format: Some("systemd".to_string()),
            source_path: None,
            aliases: Vec::new(),
            alias_of: None,
            slice: None,
            dependencies: Some(DependencySet {
                after: ["system-getty.slice".to_string()].into_iter().collect(),
                wants: ["dev-%i.device".to_string()].into_iter().collect(),
                ..Default::default()
            }),
            service: Some(ServiceConfig {
                exec_start: vec![ExecCommand {
                    raw: "/sbin/agetty %i".to_string(),
                    program: "/sbin/agetty".to_string(),
                    args: vec!["%i".to_string()],
                    ignore_failure: false,
                    privileged: false,
                }],
                tty_path: "/dev/%i".to_string(),
                environment: vec!["GETTY=%I".to_string()],
                ..Default::default()
            }),
            mount: None,
            automount: None,
            timer: None,
            socket: None,
            path: None,
            resource_control: None,
            conditions: None,
            asserts: None,
            wanted_by: None,
            required_by: None,
            template: Some(TemplateInfo {
                definition: true,
                of: None,
                parameters: Vec::new(),
            }),
        }
    }

    #[test]
    fn instantiate_expands_every_string_field() {
        let params = vec!["tty3".to_string()];
        let ir = instantiate(&template(), "getty@tty3.service", &params);

        assert_eq!(ir.id, "getty@tty3.service");
        assert!(!ir.is_template());
        assert!(ir.is_template_instance());
        assert_eq!(ir.template_of(), Some("getty@.service"));
        assert_eq!(ir.template_parameters(), ["tty3"]);

        assert_eq!(ir.description.as_deref(), Some("Getty on tty3"));
        let svc = ir.service.as_ref().expect("service config");
        assert_eq!(svc.tty_path, "/dev/tty3");
        assert_eq!(svc.exec_start[0].args, ["tty3"]);
        assert_eq!(svc.environment, ["GETTY=tty3"]);

        let deps = ir.dependencies.as_ref().expect("dependencies");
        assert_eq!(
            deps.wants.iter().next().map(String::as_str),
            Some("dev-tty3.device")
        );
        assert_eq!(
            deps.after.iter().next().map(String::as_str),
            Some("system-getty.slice")
        );
    }

    #[test]
    fn instantiate_unescapes_the_instance_for_wide_specifiers() {
        let params = vec!["dev-sda\\x2d1".to_string()];
        let ir = instantiate(&template(), "getty@dev-sda\\x2d1.service", &params);
        let svc = ir.service.as_ref().unwrap();
        // %i keeps the escaped form, %I is unescaped (mirrors systemd).
        assert_eq!(svc.tty_path, "/dev/dev-sda\\x2d1");
        assert_eq!(svc.environment, ["GETTY=dev-sda-1"]);
    }

    #[test]
    fn instantiate_keeps_multiple_parameters() {
        // Reserved shape: a backend may template on more than one value.
        // systemd only ever uses the first for `%i`.
        let params = vec!["a".to_string(), "b".to_string()];
        let ir = instantiate(&template(), "getty@a.service", &params);
        assert_eq!(ir.template_parameters(), ["a", "b"]);
        assert_eq!(ir.service.as_ref().unwrap().tty_path, "/dev/a");
    }

    #[test]
    fn instantiate_expands_name_specifiers_and_escapes() {
        let tpl = UnitIR {
            description: Some("%n %N %p %P %i %I %%".to_string()),
            ..template()
        };
        let params = vec!["a-b".to_string()];
        let ir = instantiate(&tpl, "foo@a-b.service", &params);
        assert_eq!(
            ir.description.as_deref(),
            Some("foo@a-b.service foo@a-b foo foo a-b a-b %")
        );
    }

    #[test]
    fn instantiate_leaves_unknown_specifiers_alone() {
        let tpl = UnitIR {
            description: Some("%H stays %z".to_string()),
            ..template()
        };
        let ir = instantiate(&tpl, "foo@x.service", &["x".to_string()]);
        assert_eq!(ir.description.as_deref(), Some("%H stays %z"));
    }

    #[test]
    fn instantiate_of_a_plain_unit_still_yields_an_instance() {
        let mut plain = template();
        plain.id = "plain.service".to_string();
        plain.template = None;
        let ir = instantiate(&plain, "plain@x.service", &["x".to_string()]);
        assert_eq!(ir.template_of(), Some("plain.service"));
        assert_eq!(ir.template_parameters(), ["x"]);
    }
}
