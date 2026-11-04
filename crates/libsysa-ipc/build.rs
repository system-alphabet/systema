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
