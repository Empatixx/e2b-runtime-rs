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
}

impl Config {
    pub fn checkpoints_dir(&self) -> PathBuf {
        self.data_dir.join("checkpoints")
    }

    pub fn sandboxes_dir(&self) -> PathBuf {
        self.data_dir.join("sandboxes")
    }

    pub fn templates_dir(&self) -> PathBuf {
        self.data_dir.join("templates")
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
}
