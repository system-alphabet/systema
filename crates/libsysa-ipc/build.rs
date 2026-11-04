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
}
