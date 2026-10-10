pub mod block;
pub mod builder;
#[cfg(target_os = "linux")]
pub mod cgroup;
pub mod clickhouse;
pub mod envd;
pub mod events;
pub mod exposure;
pub mod featureflags;
pub mod firecracker;
pub mod go_exec;
pub mod go_gzip;
pub mod go_humanize;
pub mod go_os;
pub mod go_path;
pub mod go_sniff;
pub mod go_strconv;
pub mod go_tar;
pub mod go_time;
pub mod go_url;
pub mod hoststats;
pub mod hyperloop;
pub mod memory;
pub mod network;
pub mod oom;
pub mod peer;
pub mod phases;
pub mod proto;
pub mod proxy;
pub mod routing;
#[cfg(target_os = "linux")]
pub mod sandbox;
#[cfg(target_os = "linux")]
pub mod sandbox_observer;
pub mod server;
pub mod storage;
pub mod system;
pub mod tcpfirewall;
pub mod telemetry;
pub mod template;
#[cfg(target_os = "linux")]
pub mod volumes;

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
