//! On-demand instantiation of IR template definitions.
//!
//! A template (`getty@.service`) is a definition: it is stored apart from the
//! unit table and never run.  When a job names an instance that no producer
//! reported, this module instantiates the template into a [`UnitIR`] and
//! merges it through the normal commit path, so the instance is
//! indistinguishable from one a finder or worker reported.
//!
//! # Precedence
//!
//! A reported definition always wins.  A producer that reported `xxx@yyy.*`
//! has already merged that instance's own drop-ins into it, so the existing
//! table entry is kept and the template is not consulted.  Only a missing
//! instance is materialized; a plain name, or an instance whose template was
//! never reported, resolves nowhere here and is left to the caller's other
//! sources (the on-disk definition and `unit.define`).

use std::collections::HashMap;

use anyhow::Result;
use tracing::info;

use crate::state::AllocatorHandle;

/// Materialize `name` from its template definition when the unit table has no
/// entry for it.
///
/// Returns `true` when the table holds a definition for `name` afterwards:
/// either the entry that was already there (a reported instance wins over the
/// template) or one instantiated from `xxx@.*`.  Returns `false` when there is
/// nothing to instantiate from — a plain unit name, or an instance whose
/// template was never reported.
pub fn materialize(allocator: &AllocatorHandle, name: &str) -> Result<bool> {
    let canonical = allocator.read().resolve_unit_name(name);
    let template = {
        let state = allocator.read();
        if state.units.contains_key(&canonical) {
            return Ok(true);
        }
        let Some(template_name) = sysa::unit_name::template_of(&canonical) else {
            return Ok(false);
        };
        match state.templates.get(&template_name) {
            Some(template) => template.clone(),
            None => return Ok(false),
        }
    };

    let parameters: Vec<String> = sysa::unit_name::instance_of(&canonical)
        .into_iter()
        .collect();
    let ir = systema_sysf::template::instantiate(&template, &canonical, &parameters);
    info!(
        "Instantiated '{}' from template '{}' (parameters: {:?})",
        canonical,
        template.id,
        ir.template_parameters()
    );

    let mut state = allocator.write();
    state
        .merge_units(&HashMap::from([(canonical, ir)]))
        .map_err(anyhow::Error::msg)?;
    drop(state);
    // Default dependencies are a System A pass over the unit table, not part
    // of any definition: the commit pipeline and the on-disk loader both rely
    // on it, so a materialized instance gets the same edges as a committed
    // one.
    crate::unit::loader::inject_default_dependencies(allocator.clone());
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use systema_sysf::ir::{ExecCommand, ServiceConfig, TemplateInfo, UnitIR, UnitType};

    fn template_ir() -> UnitIR {
        UnitIR {
            id: "getty@.service".to_string(),
            unit_type: Some(UnitType::Service),
            description: Some("Getty on %I".to_string()),
            source_format: Some("systemd".to_string()),
            service: Some(ServiceConfig {
                exec_start: vec![ExecCommand {
                    raw: "/sbin/agetty %i".to_string(),
                    program: "/sbin/agetty".to_string(),
                    args: vec!["%i".to_string()],
                    ignore_failure: false,
                    privileged: false,
                }],
                tty_path: "/dev/%i".to_string(),
                ..Default::default()
            }),
            template: Some(TemplateInfo {
                definition: true,
                of: None,
                parameters: Vec::new(),
            }),
            ..Default::default()
        }
    }

    fn allocator_with_template() -> AllocatorHandle {
        let allocator = crate::state::Allocator::handle();
        allocator
            .write()
            .merge_units(&HashMap::from([(
                "getty@.service".to_string(),
                template_ir(),
            )]))
            .unwrap();
        allocator
    }

    #[test]
    fn materialize_instantiates_from_the_template() {
        let allocator = allocator_with_template();
        assert!(materialize(&allocator, "getty@tty3.service").unwrap());

        let state = allocator.read();
        let unit = state
            .units
            .get("getty@tty3.service")
            .expect("instance materialized");
        assert_eq!(unit.unit.description, "Getty on tty3");
        assert_eq!(unit.service.as_ref().unwrap().tty_path, "/dev/tty3");
        // A template is a definition, never a unit of its own, and cannot be
        // materialized as one.
        assert!(!state.units.contains_key("getty@.service"));
        drop(state);
        assert!(!materialize(&allocator, "getty@.service").unwrap());
    }

    #[test]
    fn materialize_keeps_a_reported_instance() {
        let allocator = allocator_with_template();
        let reported = UnitIR {
            id: "getty@tty3.service".to_string(),
            unit_type: Some(UnitType::Service),
            description: Some("Reported instance".to_string()),
            service: Some(ServiceConfig {
                tty_path: "/dev/custom".to_string(),
                ..Default::default()
            }),
            template: Some(TemplateInfo {
                definition: false,
                of: Some("getty@.service".to_string()),
                parameters: vec!["tty3".to_string()],
            }),
            ..Default::default()
        };
        allocator
            .write()
            .merge_units(&HashMap::from([(
                "getty@tty3.service".to_string(),
                reported,
            )]))
            .unwrap();

        assert!(materialize(&allocator, "getty@tty3.service").unwrap());

        let state = allocator.read();
        let unit = state.units.get("getty@tty3.service").unwrap();
        // The reported definition (with its own drop-in result) wins.
        assert_eq!(unit.unit.description, "Reported instance");
        assert_eq!(unit.service.as_ref().unwrap().tty_path, "/dev/custom");
    }

    #[test]
    fn materialize_without_a_template_does_nothing() {
        let allocator = crate::state::Allocator::handle();
        assert!(!materialize(&allocator, "getty@tty3.service").unwrap());
        assert!(!materialize(&allocator, "plain.service").unwrap());
        let state = allocator.read();
        assert!(!state.units.contains_key("getty@tty3.service"));
        assert!(state.templates.is_empty());
    }
}
