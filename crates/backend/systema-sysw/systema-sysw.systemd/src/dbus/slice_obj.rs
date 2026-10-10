//! Per-unit `Slice` interface (bridge).

use zbus::interface;

pub struct SliceObject {
    pub unit_name: String,
}

#[interface(name = "org.freedesktop.systemd1.Slice")]
impl SliceObject {
    #[zbus(property)]
    fn c_p_u_set_partition(&self) -> String {
        if self.unit_name == crate::mirror::ROOT_SLICE_NAME
            || self.unit_name == "system.slice"
            || self.unit_name == "user.slice"
        {
            "root".to_string()
        } else {
            "member".to_string()
        }
    }

    #[zbus(property)]
    fn o_o_m_rules(&self) -> Vec<String> {
        Vec::new()
    }
}