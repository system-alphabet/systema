//! Generate the C header from the crate's `#[no_mangle] extern "C"` surface.

use std::path::Path;

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let config = cbindgen::Config::from_file("cbindgen.toml").expect("read cbindgen.toml");
    let bindings = cbindgen::Builder::new()
        .with_crate(&manifest_dir)
        .with_config(config)
        .generate()
        .expect("generate systema_shared.h");

    let header = Path::new(&manifest_dir).join("include/systema_shared.h");
    let mut text = Vec::new();
    bindings.write(&mut text);
    let unchanged = std::fs::read(&header)
        .map(|existing| existing == text)
        .unwrap_or(false);
    if !unchanged {
        std::fs::create_dir_all(header.parent().expect("include directory"))
            .expect("create include directory");
        std::fs::write(&header, text).expect("write systema_shared.h");
    }

    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=cbindgen.toml");
}
