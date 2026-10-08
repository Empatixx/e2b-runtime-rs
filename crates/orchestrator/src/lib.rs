pub mod block;
pub mod builder;
#[cfg(target_os = "linux")]
pub mod cgroup;
pub mod clickhouse;
pub mod envd;
pub mod events;
pub mod exposure;
pub mod firecracker;
pub mod go_exec;
pub mod go_os;
pub mod go_strconv;
pub mod go_time;
pub mod go_url;
pub mod hoststats;
pub mod hyperloop;
pub mod memory;
pub mod network;
pub mod oom;
pub mod phases;
pub mod proto;
pub mod routing;
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
