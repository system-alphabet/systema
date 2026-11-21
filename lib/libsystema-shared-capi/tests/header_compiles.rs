//! The generated header must parse as C11 with warnings as errors, and must
//! keep the two anchors the C surface depends on: the opaque worker handle
//! and the shared success status.

use std::path::PathBuf;
use std::process::Command;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn header_path() -> PathBuf {
    manifest_dir().join("include").join("systema_shared.h")
}

#[test]
fn header_has_expected_anchors() {
    let header = std::fs::read_to_string(header_path()).expect("generated header is readable");
    assert!(
        header.contains("typedef struct SystemaWorker SystemaWorker;"),
        "opaque worker handle missing from the header"
    );
    assert!(
        header.contains("SYSTEMA_STATUS_OK"),
        "success status constant missing from the header"
    );
}

#[test]
fn header_compiles_as_c11() {
    if Command::new("cc").arg("--version").output().is_err() {
        eprintln!("cc is not available; skipping the header syntax check");
        return;
    }
    let include_dir = manifest_dir().join("include");
    let output = Command::new("cc")
        .args([
            "-std=c11",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-fsyntax-only",
            "-x",
            "c",
        ])
        .arg("-I")
        .arg(&include_dir)
        .arg("-include")
        .arg("systema_shared.h")
        .arg("/dev/null")
        .output()
        .expect("cc runs");
    assert!(
        output.status.success(),
        "cc rejected the header:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
