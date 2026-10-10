use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const PROTO_DIR: &str = "../../proto/orchestrator";
const PROTOS: [&str; 5] = [
    "orchestrator.proto",
    "info.proto",
    "template-manager.proto",
    "volume.proto",
    "chunks.proto",
];
const DEPRECATED_OPTION: &str = " [deprecated = true]";

fn main() {
    println!("cargo:rerun-if-changed={PROTO_DIR}");
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets OUT_DIR")).join("protos");
    let protos = PROTOS.map(|name| undeprecated(name, &out_dir).expect("orchestrator proto copy failed"));
    tonic_prost_build::configure()
        .compile_protos(&protos, &[out_dir])
        .expect("orchestrator protobuf codegen failed");
}

fn undeprecated(name: &str, out_dir: &Path) -> io::Result<PathBuf> {
    let target = out_dir.join(name);
    fs::create_dir_all(out_dir)?;
    fs::write(
        &target,
        fs::read_to_string(Path::new(PROTO_DIR).join(name))?.replace(DEPRECATED_OPTION, ""),
    )?;
    Ok(target)
}
