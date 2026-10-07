fn main() {
    connectrpc_build::Config::new()
        .files(&["proto/process/process.proto", "proto/filesystem/filesystem.proto"])
        .includes(&["proto/"])
        .include_file("_connectrpc.rs")
        .compile()
        .expect("protobuf codegen failed");
}
