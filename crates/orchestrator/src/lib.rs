pub mod block;
#[cfg(target_os = "linux")]
pub mod cgroup;
pub mod exposure;
pub mod network;
pub mod storage;
pub mod system;

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
