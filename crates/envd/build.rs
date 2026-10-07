fn main() {
    connectrpc_build::Config::new()
        .files(&[
            "../../proto/envd/process/process.proto",
            "../../proto/envd/filesystem/filesystem.proto",
        ])
        .includes(&["../../proto/envd/"])
        .include_file("_connectrpc.rs")
        .compile()
        .expect("protobuf codegen failed");
}
