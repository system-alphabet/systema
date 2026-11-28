//! Discovery of SysV init scripts and their runlevel links.
//!
//! Scripts are the regular, executable files in the init.d search paths; a
//! script's unit name is its file name without a `.sh` suffix.  Runlevel
//! membership is filesystem evidence: an `Snn` link in `rc1.d` … `rc5.d`
//! (or `rcS.d`) marks the script enabled for that runlevel and carries the
//! boot ordering priority.  The shutdown runlevel links (`rc0.d`, `rc6.d`)
//! are not read: every service already gets the default ordering before
//! `shutdown.target`, so they add nothing.

use std::path::{Path, PathBuf};

use tracing::{debug, info, warn};

use crate::parser;
use crate::types::{is_startup_runlevel, rc_dir_runlevel, InitScript};

/// Discover and parse every init script in the configured search paths.
pub fn discover_all() -> Vec<InitScript> {
    let paths = sysa::paths::instance();
    discover_all_in(&paths.sysv_initd_paths, &paths.sysv_rc_paths)
}

/// [`discover_all`] over explicit search paths (testable without touching the
/// global path configuration).
pub fn discover_all_in(initd_dirs: &[String], rc_dirs: &[String]) -> Vec<InitScript> {
    let mut scripts = discover_scripts(initd_dirs);
    apply_runlevel_links(&mut scripts, rc_dirs);
    info!("SysV finder discovered {} init script(s)", scripts.len());
    scripts
}

/// Parse every executable script, honoring search-path priority.
fn discover_scripts(dirs: &[String]) -> Vec<InitScript> {
    let mut scripts: Vec<InitScript> = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut names: Vec<String> = entries
            .flatten()
            .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
            .collect();
        names.sort();
        for file_name in names {
            if file_name.starts_with('.') {
                continue;
            }
            let Some(name) = script_stem(&file_name) else {
                debug!("Ignoring init script name that is not a valid unit name: {file_name}");
                continue;
            };
            // The first search path wins, exactly like systemd's unit
            // load-path shadowing.
            if scripts.iter().any(|script| script.name == name) {
                continue;
            }
            let path = Path::new(dir).join(&file_name);
            if !is_executable_script(&path) {
                continue;
            }
            match parser::parse_script_from_path(&name, &path) {
                Ok(script) => scripts.push(script),
                Err(e) => warn!("Skipping init script {}: {e}", path.display()),
            }
        }
    }
    scripts
}

/// Mark scripts enabled by the `Snn` links in the start-up runlevel
/// directories and record their boot priority.
fn apply_runlevel_links(scripts: &mut [InitScript], dirs: &[String]) {
    for base in dirs {
        let Ok(entries) = std::fs::read_dir(base) else {
            continue;
        };
        let mut runlevel_dirs: Vec<(char, PathBuf)> = entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name();
                let runlevel = rc_dir_runlevel(name.to_str()?)?;
                Some((runlevel, Path::new(base).join(name)))
            })
            .collect();
        runlevel_dirs.sort_by_key(|(runlevel, _)| *runlevel);
        for (runlevel, dir) in runlevel_dirs {
            if !is_startup_runlevel(runlevel) {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let Some(link_name) = entry.file_name().to_str().map(str::to_string) else {
                    continue;
                };
                let Some((priority, name)) = start_link(&link_name) else {
                    continue;
                };
                let Some(script) = scripts.iter_mut().find(|script| script.name == name) else {
                    continue;
                };
                script.enabled = true;
                script.start_priority = Some(
                    script
                        .start_priority
                        .map_or(priority, |current| current.max(priority)),
                );
                if !script.link_runlevels.contains(&runlevel) {
                    script.link_runlevels.push(runlevel);
                }
            }
        }
    }
}

/// The `Snn` priority and script name of a runlevel link (`S20sshd` →
/// `(20, "sshd")`).
fn start_link(link_name: &str) -> Option<(u32, String)> {
    let rest = link_name.strip_prefix('S')?;
    let mut chars = rest.chars();
    let tens = chars.next()?.to_digit(10)?;
    let ones = chars.next()?.to_digit(10)?;
    Some((tens * 10 + ones, script_stem(chars.as_str())?))
}

/// The unit-name stem of an init script file (`sshd` for `sshd`, `foo` for
/// `foo.sh`).  `None` when the name cannot form a valid unit name.
fn script_stem(file_name: &str) -> Option<String> {
    let stem = file_name.strip_suffix(".sh").unwrap_or(file_name);
    let unit = format!("{stem}.service");
    sysa::unit_name::looks_like_unit_name(&unit).then(|| stem.to_string())
}

fn is_executable_script(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    meta.is_file() && is_executable(&meta)
}

#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o100 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: &std::fs::Metadata) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runlevel_links_carry_priority_and_enablement() {
        let root = std::env::temp_dir().join(format!("systema-sysf-rc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let initd = root.join("init.d");
        let rc3 = root.join("rc3.d");
        std::fs::create_dir_all(&initd).unwrap();
        std::fs::create_dir_all(&rc3).unwrap();
        std::fs::write(initd.join("foo"), "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(initd.join("foo"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        std::fs::write(rc3.join("S20foo"), "").unwrap();

        let scripts = discover_all_in(
            &[initd.to_string_lossy().into_owned()],
            &[root.to_string_lossy().into_owned()],
        );

        assert_eq!(scripts.len(), 1);
        assert_eq!(scripts[0].name, "foo");
        assert!(scripts[0].enabled);
        assert_eq!(scripts[0].start_priority, Some(20));
        assert_eq!(scripts[0].link_runlevels, ['3']);

        let _ = std::fs::remove_dir_all(&root);
    }
}
