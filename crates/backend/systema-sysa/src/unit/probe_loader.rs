#[cfg(test)]
mod probe_loader {
    #[test]
    fn probe_load_multi_user() {
        let r = crate::unit::loader::load_unit_flexible_in(
            &["/tmp/smoke2/units".to_string()], "multi-user.target");
        match r {
            Ok(u) => eprintln!("PROBE_OK name={} requires={:?} wants={:?}", u.name, u.unit.requires, u.unit.wants),
            Err(e) => eprintln!("PROBE_ERR {e:#}"),
        }
    }
}
