use std::env;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_file = "../../proto/a2a.proto";
    let proto_includes = &["../../proto"];

    let descriptor_path =
        PathBuf::from(env::var("OUT_DIR")?).join("a2a_descriptor.bin");

    // Step 1: Generate prost types + file descriptor set
    // compile_well_known_types + extern_path maps google.protobuf to pbjson_types
    // (which provides serde Serialize/Deserialize impls)
    prost_build::Config::new()
        .compile_well_known_types()
        .extern_path(".google.protobuf", "::pbjson_types")
        .file_descriptor_set_path(&descriptor_path)
        .compile_protos(&[proto_file], proto_includes)?;

    // Step 2: Generate pbjson serde impls (camelCase JSON matching proto JSON mapping)
    let descriptor_bytes = std::fs::read(&descriptor_path)?;
    pbjson_build::Builder::new()
        .register_descriptors(&descriptor_bytes)?
        .build(&[".lf.a2a.v1"])?;

    Ok(())
}
