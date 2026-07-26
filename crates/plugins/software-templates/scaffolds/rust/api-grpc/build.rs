//! Turns the protobuf contract into Rust before every build, so the code and the contract cannot
//! drift apart. The descriptor set it writes is what the reflection service serves, which is how
//! `grpcurl` knows what this service offers without being given the proto file.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR")?);
    tonic_build::configure()
        .file_descriptor_set_path(out.join("descriptor.bin"))
        .compile_protos(&["proto/service.proto"], &["proto"])?;
    println!("cargo:rerun-if-changed=proto/service.proto");
    Ok(())
}
