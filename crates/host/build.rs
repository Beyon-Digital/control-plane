fn main() -> Result<(), Box<dyn std::error::Error>> {
    std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path()?);
    tonic_build::configure()
        .compile_protos(&["../../proto/control_plane.proto"], &["../../proto"])?;
    println!("cargo:rerun-if-changed=../../proto/control_plane.proto");
    Ok(())
}
