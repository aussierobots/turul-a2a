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

    // Step 1: Generate prost types + file descriptor set
    // compile_well_known_types + extern_path maps google.protobuf to pbjson_types
    // (which provides serde Serialize/Deserialize impls)
    prost_build::Config::new()
        .compile_well_known_types()
        .extern_path(".google.protobuf", "::pbjson_types")
        .file_descriptor_set_path(&descriptor_path)
        .compile_protos(&[proto_file], proto_includes)?;

    // Step 2: Generate pbjson serde impls (camelCase JSON matching proto JSON mapping)
    // ignore_unknown_fields: allows v1.0 clients to deserialize agent cards that
    // include additive v0.3 compat fields (url, protocolVersion, additionalInterfaces)
    // without rejecting them as unknown. This is standard proto forward-compat behavior.
    let descriptor_bytes = std::fs::read(&descriptor_path)?;
    pbjson_build::Builder::new()
        .register_descriptors(&descriptor_bytes)?
        .ignore_unknown_fields()
        .build(&[".lf.a2a.v1"])?;

    Ok(())
}
