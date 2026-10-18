/// Compile-time build record for `--full-version` (see src/version.rs):
/// target, profile, toolchain and enabled features as cargo saw them.
fn main() {
    emit_build_info();
}

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
