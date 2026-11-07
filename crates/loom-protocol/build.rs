fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Vendored protoc: no system protoc required (works on macOS/Linux/CI).
    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    std::env::set_var("PROTOC", protoc);

    let proto = "../../proto/agent.proto";
    println!("cargo:rerun-if-changed={proto}");

    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .protoc_arg("--experimental_allow_proto3_optional")
        .compile_protos(&[proto], &["../../proto"])?;
    Ok(())
}
