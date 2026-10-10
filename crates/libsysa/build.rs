fn main() {
    for proto in ["common.proto", "workload.proto", "control.proto"] {
        println!("cargo:rerun-if-changed=../../proto/{proto}");
    }
    prost_build::compile_protos(
        &[
            "../../proto/common.proto",
            "../../proto/workload.proto",
            "../../proto/control.proto",
        ],
        &["../../proto/"],
    )
    .expect("Failed to compile proto files");

    generate_paths();
    emit_build_info();
    compile_mo_files();
}

fn compile_mo_files() {
    if std::env::var("CARGO_FEATURE_L10N_DEBUG").is_err() {
        return;
    }

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let po_dir = std::path::Path::new(&manifest_dir)
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("po");
    if !po_dir.exists() {
        return;
    }

    let out_dir = std::env::var("OUT_DIR").unwrap();
    let mo_root = std::path::Path::new(&out_dir).join("mo");

    for entry in std::fs::read_dir(&po_dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("po") {
            continue;
        }
        let lang = path.file_stem().unwrap().to_str().unwrap();
        let mo_subdir = mo_root.join(lang).join("LC_MESSAGES");
        std::fs::create_dir_all(&mo_subdir).unwrap();

        let mo_path = mo_subdir.join("systema.mo");
        let status = std::process::Command::new("msgfmt")
            .arg(&path)
            .arg("-o")
            .arg(&mo_path)
            .status()
            .expect("msgfmt not found — install gettext tools");
        if !status.success() {
            panic!("msgfmt failed for {:?}", path);
        }
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

/// Compile-time build record for `--full-version` (see src/version.rs):
/// target, profile, toolchain and enabled features as cargo saw them.
fn emit_build_info() {
    let target = std::env::var("TARGET").unwrap_or_default();
    let profile = std::env::var("PROFILE").unwrap_or_default();
    let opt_level = std::env::var("OPT_LEVEL").unwrap_or_default();
    let debug_info = std::env::var("DEBUG").unwrap_or_default();
    println!("cargo:rustc-env=SYSTEMA_BUILD_TARGET={target}");
    println!("cargo:rustc-env=SYSTEMA_BUILD_PROFILE={profile}");
    println!("cargo:rustc-env=SYSTEMA_BUILD_OPT_LEVEL={opt_level}");
    println!("cargo:rustc-env=SYSTEMA_BUILD_DEBUG={debug_info}");

    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let version = std::process::Command::new(&rustc)
        .arg("--version")
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "rustc (unknown)".to_string());
    println!("cargo:rustc-env=SYSTEMA_BUILD_RUSTC={version}");

    let mut features: Vec<String> = std::env::vars()
        .filter_map(|(key, _)| {
            key.strip_prefix("CARGO_FEATURE_")
                .map(|name| name.to_lowercase())
        })
        .collect();
    features.sort();
    let features = if features.is_empty() {
        "none".to_string()
    } else {
        features.join(", ")
    };
    println!("cargo:rustc-env=SYSTEMA_BUILD_FEATURES={features}");
}

fn generate_paths() {
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let dest_path = std::path::Path::new(&out_dir).join("paths.rs");

    // Build-time overrides for the compile-time defaults below: the same
    // SYSTEMA_* / SYSTEMD_* variables that paths.rs reads at *runtime*.
    // When they are exported before `cargo build` (e.g. `source
    // termux-env.sh`), their values are baked into the binary as the new
    // defaults; rerun-if-env-changed rebuilds automatically when an
    // export appears, disappears or changes — no `cargo clean` needed.
    // The runtime resolution in paths.rs is unchanged and still takes
    // precedence over these baked values.
    fn get(name: &str, default: String) -> String {
        println!("cargo:rerun-if-env-changed={name}");
        std::env::var(name).unwrap_or(default)
    }

    // Colon-separated list form of `get`, mirroring resolve_list() in
    // paths.rs (split on ':', trim each element).
    fn get_list(name: &str, default: &[&str]) -> String {
        println!("cargo:rerun-if-env-changed={name}");
        let items: Vec<String> = match std::env::var(name) {
            Ok(v) => v.split(':').map(|s| format!("{:?}", s.trim())).collect(),
            Err(_) => default.iter().map(|s| format!("{s:?}")).collect(),
        };
        items.join(",\n    ")
    }

    let runstatedir = get("SYSTEMA_RUNSTATEDIR", "/run".to_string());
    let control_socket = get(
        "SYSTEMA_CONTROL_SOCKET",
        format!("{runstatedir}/systema/control.socket"),
    );
    let notify_dir = get("SYSTEMA_NOTIFY_DIR", format!("{runstatedir}/systema/notify"));
    let systemd_lib_unit_dir = get(
        "SYSTEMD_LIB_UNIT_DIR",
        "/usr/lib/systemd/system".to_string(),
    );
    let systemd_first_boot_file = get(
        "SYSTEMD_FIRST_BOOT_FILE",
        "/run/systemd/first-boot".to_string(),
    );
    let systemd_machine_id_file = get("SYSTEMD_MACHINE_ID_FILE", "/etc/machine-id".to_string());
    let ipc_socket_path = get(
        "SYSTEMA_IPC_SOCKET",
        "/run/systema/allocator.sock".to_string(),
    );
    let systema_fdpass_sock = get("SYSTEMA_FDPASS_SOCK", "/run/systema/fdpass.sock".to_string());
    let systema_shell_path = get("SYSTEMA_SHELL_PATH", "/bin/sh".to_string());
    let systema_socket_handler_path = get(
        "SYSTEMA_SOCKET_HANDLER_PATH",
        "/usr/lib/systema/socket-handler".to_string(),
    );
    let locale_dir = get("SYSTEMA_LOCALE_DIR", "/usr/share/locale".to_string());
    let log_dir = get("SYSTEMA_LOG_DIR", "/var/log/systema".to_string());
    let reload_socket = get("SYSTEMA_RELOAD_SOCKET", "/run/systema/sysf.sock".to_string());
    let unit_search_paths = get_list(
        "SYSTEMA_UNIT_PATH",
        &[
            "/etc/systema",
            "/run/systema",
            "/usr/local/lib/systema",
            "/usr/lib/systema",
            "/etc/systemd/system",
            "/usr/lib/systemd/system",
            "/lib/systemd/system",
        ],
    );
    let generator_search_paths = get_list(
        "SYSTEMA_GENERATOR_PATH",
        &[
            "/run/systemd/generator",
            "/run/systemd/generator.late",
            "/etc/systemd/system-generators",
            "/usr/local/lib/systemd/system-generators",
            "/usr/lib/systemd/system-generators",
            "/lib/systemd/system-generators",
        ],
    );
    let finder_search_paths = get_list(
        "SYSTEMA_FINDER_PATH",
        &[
            "/etc/systema/finder",
            "/usr/etc/systema/finder",
            "/usr/local/etc/systema/finder",
            "/opt/systema/finder",
        ],
    );
    let systema_bin_search_paths = get_list(
        "SYSTEMA_BIN_PATH",
        &[
            "/",
            "/bin",
            "/sbin",
            "/lib/systema",
            "/libexec/systema",
            "/usr/bin",
            "/usr/sbin",
            "/usr/lib/systema",
            "/usr/libexec/systema",
            "/usr/local/bin",
            "/usr/local/sbin",
            "/usr/local/lib/systema",
            "/usr/local/libexec/systema",
            "/opt/systema",
        ],
    );

    let content = format!(r#"// Generated by build.rs — do not edit.
// Compile-time default paths. Override at runtime via environment variables.
// The same SYSTEMA_* / SYSTEMD_* variables exported at *build* time
// override these defaults (see generate_paths() in build.rs).

pub const SYSTEMD_LIB_UNIT_DIR: &str = {systemd_lib_unit_dir:?};
pub const SYSTEMD_FIRST_BOOT_FILE: &str = {systemd_first_boot_file:?};
pub const SYSTEMD_MACHINE_ID_FILE: &str = {systemd_machine_id_file:?};

pub const IPC_SOCKET_PATH: &str = {ipc_socket_path:?};
pub const SYSTEMA_FDPASS_SOCK: &str = {systema_fdpass_sock:?};
pub const CONTROL_SOCKET_PATH: &str = {control_socket:?};

pub const RUNSTATEDIR: &str = {runstatedir:?};

pub const NOTIFY_DIR: &str = {notify_dir:?};

pub const SYSTEMA_SHELL_PATH: &str = {systema_shell_path:?};
pub const SYSTEMA_SOCKET_HANDLER_PATH: &str = {systema_socket_handler_path:?};

pub const LOCALE_DIR: &str = {locale_dir:?};

pub const LOG_DIR: &str = {log_dir:?};

pub const UNIT_SEARCH_PATHS: &[&str] = &[
    {unit_search_paths},
];

pub const GENERATOR_SEARCH_PATHS: &[&str] = &[
    {generator_search_paths},
];

// Search paths for System F finder executables.  The `systema-sysf`
// executable locates format-specific finder binaries (e.g.
// `systema-sysf.systemd`) in these directories.
pub const FINDER_SEARCH_PATHS: &[&str] = &[
    {finder_search_paths},
];

pub const RELOAD_SOCKET: &str = {reload_socket:?};

pub const SYSTEMA_BIN_SEARCH_PATHS: &[&str] = &[
    {systema_bin_search_paths},
];
"#);

    std::fs::write(&dest_path, content).unwrap();
}
