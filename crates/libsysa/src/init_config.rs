//! `/etc/systema/init.conf` — the System Init module selection.
//!
//! The file uses the systemd configuration syntax: `[Init]` holds the
//! allocator, finder and splash selection, `[Workers]` holds the supervised
//! modules, each with `Key=Value` assignments, `#`/`;` comment lines and
//! blank lines.  A key is only accepted in the section it belongs to.
//! The drop-in directory (`init.conf.d`, next to the main file) is read
//! afterwards: every `*.conf` file is applied in lexicographic order, so a
//! later file overrides an earlier assignment of the same key.
//!
//! Both `systema-sysi` (the module table) and `systema-sysa` (the `Finder=`
//! selection) read the same files; a missing file or directory simply
//! yields an empty configuration, and each key falls back to its built-in
//! default.  An *empty* value disables the module (`Service=`).
//!
//! Unknown sections, unknown keys and malformed lines are ignored with a
//! warning, mirroring systemd's `config_parse()`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tracing::warn;

/// The sections this file understands.  The allocator, the finder and the
/// splash console are not supervised workers, so they live in `[Init]`; the
/// supervised modules live in `[Workers]`.
pub const SECTION_INIT: &str = "Init";
pub const SECTION_WORKERS: &str = "Workers";
pub const SECTIONS: &[&str] = &[SECTION_INIT, SECTION_WORKERS];

pub const KEY_ALLOCATOR: &str = "Allocator";
pub const KEY_SERVICE: &str = "Service";
pub const KEY_EXTERNAL: &str = "External";
pub const KEY_TARGET: &str = "Target";
pub const KEY_CRON: &str = "Cron";
pub const KEY_SOCKET: &str = "Socket";
pub const KEY_PATH: &str = "Path";
pub const KEY_DEVICE: &str = "Device";
pub const KEY_RESOURCE: &str = "Resource";
pub const KEY_MOUNT: &str = "Mount";
pub const KEY_WRAPPER: &str = "Wrapper";
/// The format-specific finder executable run by System F on a
/// daemon-reload (e.g. `systema-sysf.systemd`), not the `systema-sysf`
/// runner itself.
pub const KEY_FINDER: &str = "Finder";
/// The boot-log console frontend, started once System A is ready and
/// before any worker.
pub const KEY_SPLASH: &str = "Splash";

/// The section a key belongs to.
fn section_of(key: &str) -> &'static str {
    match key {
        KEY_ALLOCATOR | KEY_FINDER | KEY_SPLASH => SECTION_INIT,
        _ => SECTION_WORKERS,
    }
}

/// Every key this file understands.  Used to reject typos, and shared so
/// System A does not warn about keys only System Init consumes.
pub const KEYS: &[&str] = &[
    KEY_ALLOCATOR,
    KEY_SERVICE,
    KEY_EXTERNAL,
    KEY_TARGET,
    KEY_CRON,
    KEY_SOCKET,
    KEY_PATH,
    KEY_DEVICE,
    KEY_RESOURCE,
    KEY_MOUNT,
    KEY_WRAPPER,
    KEY_FINDER,
    KEY_SPLASH,
];

/// The finder run when `Finder=` is not configured at all.
pub const DEFAULT_FINDER: &str = "systema-sysf.systemd";

/// Environment variable per module, in the `SYSTEMA_SYS*` style used by the
/// `*_FLAGS` overrides.  Environment variables outrank the files and are
/// themselves outranked by command-line options.
pub const ENV: &[(&str, &str)] = &[
    (KEY_ALLOCATOR, "SYSTEMA_SYSA"),
    (KEY_SERVICE, "SYSTEMA_SYSS"),
    (KEY_EXTERNAL, "SYSTEMA_SYSE"),
    (KEY_TARGET, "SYSTEMA_SYST"),
    (KEY_CRON, "SYSTEMA_SYSC"),
    (KEY_SOCKET, "SYSTEMA_SYSK"),
    (KEY_PATH, "SYSTEMA_SYSN"),
    (KEY_DEVICE, "SYSTEMA_SYSD"),
    (KEY_RESOURCE, "SYSTEMA_SYSR"),
    (KEY_MOUNT, "SYSTEMA_SYSM"),
    (KEY_WRAPPER, "SYSTEMA_SYSW"),
    (KEY_FINDER, "SYSTEMA_SYSF"),
    (KEY_SPLASH, "SYSTEMA_SYSH"),
];

/// The merged contents of the main file and its drop-ins, with optional
/// environment and command-line layers applied on top.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InitConfig {
    values: HashMap<String, String>,
}

impl InitConfig {
    /// Build a configuration from explicit pairs (used by tests).
    pub fn from_pairs<I, K, V>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let mut config = Self::default();
        config.apply_pairs(pairs);
        config
    }

    /// Overlay explicit assignments, overriding earlier values.  A later
    /// `Key=Value` wins over an earlier one, so this is the command-line
    /// layer (`--worker`, `--finder`).
    pub fn apply_pairs<I, K, V>(&mut self, pairs: I)
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        for (key, value) in pairs {
            self.values.insert(key.into(), value.into());
        }
    }

    /// Overlay the `SYSTEMA_SYS*` environment variables.  An empty value is
    /// preserved and disables the module, exactly like an empty file value.
    pub fn apply_env(&mut self) {
        self.apply_env_from(|name| std::env::var(name).ok());
    }

    /// [`Self::apply_env`] with an injectable lookup, so tests do not touch
    /// the process-global environment.
    pub fn apply_env_from<F>(&mut self, lookup: F)
    where
        F: Fn(&str) -> Option<String>,
    {
        for (key, name) in ENV {
            if let Some(value) = lookup(name) {
                self.values
                    .insert((*key).to_string(), value.trim().to_string());
            }
        }
    }

    /// Load the configuration from the built-in paths (see
    /// [`crate::paths::Paths::init_conf_file`]).
    pub fn load() -> Self {
        let paths = crate::paths::instance();
        Self::load_from(
            Path::new(paths.init_conf_file),
            Path::new(paths.init_conf_dir),
        )
    }

    /// [`Self::load`] with the environment layer applied.
    pub fn load_with_env() -> Self {
        let mut config = Self::load();
        config.apply_env();
        config
    }

    /// Load `main` and then every `*.conf` in `dropin_dir`, in
    /// lexicographic order.  Missing files are not an error.
    pub fn load_from(main: &Path, dropin_dir: &Path) -> Self {
        let mut config = Self::default();
        config.read_file(main);
        for path in dropin_files(dropin_dir) {
            config.read_file(&path);
        }
        config
    }

    /// The configured value of `key`, or `None` when the key is absent.
    ///
    /// `Some("")` means the module was explicitly disabled.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    fn read_file(&mut self, path: &Path) {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                warn!("Failed to read init configuration {}: {e}", path.display());
                return;
            }
        };

        let mut section: Option<String> = None;
        for (index, line) in text.lines().enumerate() {
            self.apply_line(path, index + 1, &mut section, line);
        }
    }

    fn apply_line(
        &mut self,
        path: &Path,
        line_no: usize,
        section: &mut Option<String>,
        line: &str,
    ) {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            return;
        }

        if let Some(header) = line.strip_prefix('[') {
            let Some(name) = header.strip_suffix(']') else {
                warn!(
                    "{}:{line_no}: malformed section header, ignoring",
                    path.display()
                );
                return;
            };
            let name = name.trim();
            *section = Some(name.to_string());
            if !SECTIONS.contains(&name) {
                warn!(
                    "{}:{line_no}: unknown section '[{name}]', ignoring its assignments",
                    path.display()
                );
            }
            return;
        }

        let Some((key, value)) = line.split_once('=') else {
            warn!(
                "{}:{line_no}: expected 'Key=Value', ignoring line",
                path.display()
            );
            return;
        };

        let Some(section_name) = section.as_deref() else {
            warn!(
                "{}:{line_no}: assignment outside of a section, ignoring",
                path.display()
            );
            return;
        };
        // Assignments inside an unknown section were already reported when
        // the section header was parsed.
        if !SECTIONS.contains(&section_name) {
            return;
        }

        let key = key.trim();
        if !KEYS.contains(&key) {
            warn!(
                "{}:{line_no}: unknown key '{key}', ignoring",
                path.display()
            );
            return;
        }
        let owner = section_of(key);
        if owner != section_name {
            warn!(
                "{}:{line_no}: key '{key}' belongs in section '[{owner}]', ignoring",
                path.display()
            );
            return;
        }
        self.values
            .insert(key.to_string(), value.trim().to_string());
    }
}

/// Every `*.conf` regular file in `dir`, sorted by file name.
fn dropin_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "conf") && path.is_file())
        .collect();
    files.sort();
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("systema-init-config-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_files_yield_an_empty_configuration() {
        let dir = temp_dir("missing");
        let config = InitConfig::load_from(&dir.join("init.conf"), &dir.join("init.conf.d"));
        assert_eq!(config, InitConfig::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn main_file_is_parsed_and_empty_value_is_kept() {
        let dir = temp_dir("main");
        std::fs::write(
            dir.join("init.conf"),
            "[Init]\n\
             # a comment\n\
             ; another comment\n\
             Allocator=systema-sysa\n\
             [Workers]\n\
             Service=systema-syss\n\
             Device=\n",
        )
        .unwrap();

        let config = InitConfig::load_from(&dir.join("init.conf"), &dir.join("init.conf.d"));
        assert_eq!(config.get(KEY_ALLOCATOR), Some("systema-sysa"));
        assert_eq!(config.get(KEY_SERVICE), Some("systema-syss"));
        assert_eq!(config.get(KEY_DEVICE), Some(""));
        assert_eq!(config.get(KEY_MOUNT), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn drop_ins_override_in_lexicographic_order() {
        let dir = temp_dir("dropins");
        let dropins = dir.join("init.conf.d");
        std::fs::create_dir_all(&dropins).unwrap();
        std::fs::write(
            dir.join("init.conf"),
            "[Workers]\nService=systema-syss\nMount=systema-sysm.linux\n",
        )
        .unwrap();
        std::fs::write(dropins.join("20-mount.conf"), "[Workers]\nMount=none\n").unwrap();
        std::fs::write(
            dropins.join("10-service.conf"),
            "[Workers]\nService=systema-syss.debug\n",
        )
        .unwrap();
        // Not a drop-in: wrong extension.
        std::fs::write(dropins.join("30-ignored.conf.bak"), "[Init]\nFinder=\n").unwrap();

        let config = InitConfig::load_from(&dir.join("init.conf"), &dropins);
        assert_eq!(config.get(KEY_SERVICE), Some("systema-syss.debug"));
        assert_eq!(config.get(KEY_MOUNT), Some("none"));
        assert_eq!(config.get(KEY_FINDER), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn later_drop_in_wins_over_earlier_one() {
        let dir = temp_dir("order");
        let dropins = dir.join("init.conf.d");
        std::fs::create_dir_all(&dropins).unwrap();
        std::fs::write(dropins.join("10-a.conf"), "[Init]\nFinder=first\n").unwrap();
        std::fs::write(dropins.join("20-b.conf"), "[Init]\nFinder=second\n").unwrap();

        let config = InitConfig::load_from(&dir.join("absent.conf"), &dropins);
        assert_eq!(config.get(KEY_FINDER), Some("second"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_keys_sections_and_stray_lines_are_ignored() {
        let dir = temp_dir("invalid");
        std::fs::write(
            dir.join("init.conf"),
            "[Manager]\nService=ignored\n\
             [Init]\n\
             Service=wrong-section\n\
             Bogus=1\n\
             no assignment here\n\
             [broken\n\
             Finder=systema-sysf.systemd\n",
        )
        .unwrap();

        let config = InitConfig::load_from(&dir.join("init.conf"), &dir.join("init.conf.d"));
        assert_eq!(config.get(KEY_SERVICE), None);
        assert_eq!(config.get("Bogus"), None);
        assert_eq!(config.get(KEY_FINDER), Some("systema-sysf.systemd"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn assignments_before_any_section_are_ignored() {
        let dir = temp_dir("nosection");
        std::fs::write(dir.join("init.conf"), "Service=systema-syss\n").unwrap();

        let config = InitConfig::load_from(&dir.join("init.conf"), &dir.join("init.conf.d"));
        assert_eq!(config.get(KEY_SERVICE), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn environment_outranks_files_and_empty_values_disable() {
        let mut config = InitConfig::from_pairs([(KEY_SERVICE, "from-file")]);
        config.apply_env_from(|name| match name {
            "SYSTEMA_SYSS" => Some("from-env".to_string()),
            "SYSTEMA_SYSD" => Some(String::new()),
            _ => None,
        });

        assert_eq!(config.get(KEY_SERVICE), Some("from-env"));
        assert_eq!(config.get(KEY_DEVICE), Some(""));
        assert_eq!(config.get(KEY_MOUNT), None);
    }

    #[test]
    fn command_line_pairs_outrank_the_environment() {
        let mut config = InitConfig::from_pairs([(KEY_SERVICE, "from-file")]);
        config.apply_env_from(|_| Some("from-env".to_string()));
        config.apply_pairs([(KEY_SERVICE, "from-cli"), (KEY_DEVICE, "")]);

        assert_eq!(config.get(KEY_SERVICE), Some("from-cli"));
        assert_eq!(config.get(KEY_DEVICE), Some(""));
        assert_eq!(config.get(KEY_MOUNT), Some("from-env"));
    }
}
