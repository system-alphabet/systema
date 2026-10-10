//! Temporary reproduction: walk the VM rootfs unit dirs (via explicit
//! search-path list) and inspect graphical.target wants + lightdm aliases
//! after discovery + finder merge.

use std::collections::HashMap;

use systema_sysf::Finder;
use systema_sysf_systemd::loader::discover_all;
use systema_sysf_systemd::finder::SystemdFinder;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let base = std::env::var("VM_ROOT").unwrap_or_else(|_| "/mnt".to_string());
    let dirs = [
        "/etc/systema",
        "/run/systema",
        "/usr/local/lib/systema",
        "/usr/lib/systema",
        "/etc/systemd/system",
        "/usr/lib/systemd/system",
        "/lib/systemd/system",
    ]
    .iter()
    .map(|d| format!("{base}{d}"))
    .collect::<Vec<_>>()
    .join(":");
    std::env::set_var("SYSTEMA_UNIT_PATH", &dirs);

    let files = discover_all().expect("discover_all");
    eprintln!("discovered {} units", files.len());

    let finder = SystemdFinder::new();
    let map: HashMap<String, systema_sysf::ir::UnitIR> =
        finder.find_all().await.expect("find_all");

    for key in [
        "graphical.target",
        "lightdm.service",
        "default.target",
        "display-manager.service",
    ] {
        match map.get(key) {
            Some(ir) => eprintln!("== {key} exists: aliases={:?}", ir.aliases),
            None => eprintln!("== {key}: ABSENT"),
        }
    }
    if let Some(g) = map.get("graphical.target") {
        eprintln!(
            "graphical.target deps: wants={:?} requires={:?} after={:?}",
            g.dependencies.as_ref().map(|d| &d.wants),
            g.dependencies.as_ref().map(|d| &d.requires),
            g.dependencies.as_ref().map(|d| &d.after),
        );
    }
}