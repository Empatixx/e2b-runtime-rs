use std::path::PathBuf;

use clap::Args;

#[derive(Args, Debug, Clone)]
pub struct Config {
    #[arg(long, env = "E2B_SIMPLE_DATA", default_value = "/mnt/simple/data")]
    pub data_dir: PathBuf,
    #[arg(long, env = "E2B_SIMPLE_FIRECRACKER", default_value = "/mnt/simple/deps/firecracker")]
    pub firecracker: PathBuf,
    #[arg(long, env = "E2B_SIMPLE_KERNEL", default_value = "/mnt/simple/deps/vmlinux")]
    pub kernel: PathBuf,
    #[arg(
        long,
        env = "E2B_SIMPLE_UBLK_DAEMON",
        default_value = "/mnt/simple/deps/uvm-ublk-daemon"
    )]
    pub ublk_daemon: PathBuf,
    #[arg(long, env = "E2B_SIMPLE_SLOT_BASE", default_value_t = 0)]
    pub slot_base: u32,
    #[command(flatten)]
    pub s3: S3Config,
}

#[derive(Args, Debug, Clone)]
pub struct S3Config {
    #[arg(
        long = "s3-endpoint",
        env = "E2B_SIMPLE_S3_ENDPOINT",
        default_value = "http://127.0.0.1:9000"
    )]
    pub endpoint: String,
    #[arg(long = "s3-bucket", env = "E2B_SIMPLE_S3_BUCKET", default_value = "e2b-simple")]
    pub bucket: String,
    #[arg(long = "s3-region", env = "E2B_SIMPLE_S3_REGION", default_value = "us-east-1")]
    pub region: String,
    #[arg(long = "s3-access-key", env = "E2B_SIMPLE_S3_ACCESS_KEY")]
    pub access_key: String,
    #[arg(long = "s3-secret-key", env = "E2B_SIMPLE_S3_SECRET_KEY")]
    pub secret_key: String,
}

impl Config {
    pub fn checkpoints_dir(&self) -> PathBuf {
        self.data_dir.join("checkpoints")
    }

    pub fn layers_dir(&self) -> PathBuf {
        self.data_dir.join("layers")
    }

    pub fn sandboxes_dir(&self) -> PathBuf {
        self.data_dir.join("sandboxes")
    }

    pub fn memory_dir(&self) -> PathBuf {
        self.data_dir.join("memory")
    }

    pub fn volumes_dir(&self) -> PathBuf {
        self.data_dir.join("volumes")
    }

    pub fn builds_dir(&self) -> PathBuf {
        self.data_dir.join("builds")
    }

    pub fn placeholder_drive(&self) -> PathBuf {
        self.data_dir.join("placeholder.img")
    }
}

#[derive(Args, Debug, Clone)]
pub struct ServeConfig {
    #[arg(long, env = "E2B_API_KEY")]
    pub api_key: String,
    #[arg(long, env = "E2B_SIMPLE_API_ADDR", default_value = "0.0.0.0:3000")]
    pub api_addr: String,
    #[arg(long, env = "E2B_SIMPLE_PROXY_ADDR", default_value = "0.0.0.0:3001")]
    pub proxy_addr: String,
    #[arg(long, env = "E2B_DOMAIN", default_value = "localhost")]
    pub domain: String,
    #[arg(long, env = "E2B_SIMPLE_NODE_ID", default_value = "node-a")]
    pub node_id: String,
    #[arg(long, env = "E2B_SIMPLE_NODE_API_URL", default_value = "http://127.0.0.1:3000")]
    pub node_api_url: String,
    #[arg(long, env = "E2B_SIMPLE_CACHE_GB", default_value_t = 50)]
    pub cache_capacity_gb: u64,
}
