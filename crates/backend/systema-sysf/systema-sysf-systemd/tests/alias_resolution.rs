//! Alias resolution through the real discovery path.
//!
//! The finder scans the configured search paths in priority order and must
//! report exactly one alias record per name, so System A never has to choose
//! between competing claims.

use std::fs;
use std::path::PathBuf;

use systema_sysf::Finder;
use systema_sysf_systemd::finder::SystemdFinder;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("systema-sysf-alias-{}-{tag}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn service(exec: &str) -> String {
    format!("[Unit]\nDescription=test\n[Service]\nExecStart={exec}\n")
}

#[tokio::test]
async fn alias_records_resolve_the_highest_priority_link() {
    let etc = temp_dir("etc");
    let usr = temp_dir("usr");

    // /usr: the two real definitions plus a link that loses to /etc.
    fs::write(usr.join("lightdm.service"), service("/usr/bin/lightdm")).unwrap();
    fs::write(usr.join("ddm.service"), service("/usr/bin/ddm")).unwrap();
    std::os::unix::fs::symlink("ddm.service", usr.join("display-manager.service")).unwrap();
    // A second name for lightdm: every alias of one unit must survive.
    std::os::unix::fs::symlink("lightdm.service", usr.join("x-display-manager.service")).unwrap();

    // /etc: the higher-priority link points at lightdm.
    std::os::unix::fs::symlink(
        usr.join("lightdm.service"),
        etc.join("display-manager.service"),
    )
    .unwrap();

    std::env::set_var(
        "SYSTEMA_UNIT_PATH",
        format!("{}:{}", etc.display(), usr.display()),
    );

    let map = SystemdFinder::new().find_all().await.unwrap();

    let record = map
        .get("display-manager.service")
        .expect("one alias record for the contested name");
    assert_eq!(record.alias_of.as_deref(), Some("lightdm.service"));
    assert!(record.unit_type.is_none(), "an alias is not a definition");
    assert!(record.service.is_none());

    assert_eq!(
        map["x-display-manager.service"].alias_of.as_deref(),
        Some("lightdm.service"),
        "a second alias of the same unit must not be lost to deduplication"
    );

    // The aliases live only in the records: no unit carries a rival claim.
    assert!(map["lightdm.service"].aliases.is_empty());
    assert!(map["ddm.service"].aliases.is_empty());

    let _ = fs::remove_dir_all(&etc);
    let _ = fs::remove_dir_all(&usr);
}
