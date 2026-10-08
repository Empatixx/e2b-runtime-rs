pub mod block;
pub mod builder;
#[cfg(target_os = "linux")]
pub mod cgroup;
pub mod envd;
pub mod exposure;
pub mod firecracker;
pub mod hyperloop;
pub mod memory;
pub mod network;
pub mod phases;
pub mod proto;
#[cfg(target_os = "linux")]
pub mod sandbox;
pub mod server;
pub mod storage;
pub mod system;
pub mod tcpfirewall;
pub mod template;

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    #[test]
    fn version_is_the_crate_version() {
        assert_eq!(super::version(), "0.1.0");
    }
}
