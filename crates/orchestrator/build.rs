fn main() {
    tonic_prost_build::configure()
        .compile_protos(
            &[
                "../../proto/orchestrator/orchestrator.proto",
                "../../proto/orchestrator/info.proto",
                "../../proto/orchestrator/template-manager.proto",
            ],
            &["../../proto/orchestrator/"],
        )
        .expect("orchestrator protobuf codegen failed");
}
