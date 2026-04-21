use std::env;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Proto files live inside this crate so they travel with `cargo package`.
    // A workspace-root symlink (`proto/` → `crates/turul-a2a-proto/proto/`)
    // preserves the historical path referenced in docs.
    let proto_file = "proto/a2a.proto";
    let proto_includes = &["proto"];

    // Rebuild if any proto changes.
    println!("cargo:rerun-if-changed=proto");

    let descriptor_path = PathBuf::from(env::var("OUT_DIR")?).join("a2a_descriptor.bin");

    // Step 1: generate prost message types (+ tonic service stubs when grpc).
    compile_protos(proto_file, proto_includes, &descriptor_path)?;

    // Step 2: pbjson serde impls (camelCase JSON matching proto JSON mapping).
    // ignore_unknown_fields: allows v1.0 clients to deserialize agent cards
    // that include additive v0.3 compat fields without rejecting them.
    let descriptor_bytes = std::fs::read(&descriptor_path)?;
    pbjson_build::Builder::new()
        .register_descriptors(&descriptor_bytes)?
        .ignore_unknown_fields()
        .build(&[".lf.a2a.v1"])?;

    Ok(())
}

#[cfg(feature = "grpc")]
fn compile_protos(
    proto_file: &str,
    proto_includes: &[&str],
    descriptor_path: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    // tonic-prost-build subsumes prost-build and emits both message types
    // and service stubs (server + client) in a single pass. Matching
    // extern_path + compile_well_known_types values to the non-grpc path
    // below keeps the generated message types byte-identical so pbjson
    // serde impls work against either output.
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_well_known_types(true)
        .extern_path(".google.protobuf", "::pbjson_types")
        .file_descriptor_set_path(descriptor_path)
        .compile_protos(&[proto_file], proto_includes)?;
    Ok(())
}

#[cfg(not(feature = "grpc"))]
fn compile_protos(
    proto_file: &str,
    proto_includes: &[&str],
    descriptor_path: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    prost_build::Config::new()
        .compile_well_known_types()
        .extern_path(".google.protobuf", "::pbjson_types")
        .file_descriptor_set_path(descriptor_path)
        .compile_protos(&[proto_file], proto_includes)?;
    Ok(())
}
