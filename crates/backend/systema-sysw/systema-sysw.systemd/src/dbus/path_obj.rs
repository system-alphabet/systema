//! Per-unit `Path` interface (bridge).
//!
//! `systemctl list-paths` reads `Paths` with a fatally-checked
//! `Properties.Get`, so the member must exist even when SysF reports no path
//! watch data.

use zbus::interface;

pub struct PathObject;

#[interface(name = "org.freedesktop.systemd1.Path")]
impl PathObject {
    #[zbus(property)]
    fn paths(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    #[zbus(property)]
    fn result(&self) -> String {
        "success".to_string()
    }
}
