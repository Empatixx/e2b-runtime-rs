use std::path::{Path, PathBuf};

pub const GUEST_DIR: &str = "/fc-vm";
pub const GUEST_ROOTFS: &str = "/fc-vm/rootfs.ext4";
pub const GUEST_KERNEL_DIR: &str = "/fc-vm/kernel";
pub const GUEST_KERNEL: &str = "/fc-vm/kernel/vmlinux";

#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(_, into))]
pub struct Launch {
    pub binary: PathBuf,
    pub socket: PathBuf,
    pub namespace: String,
    pub rootfs: PathBuf,
    pub kernel: PathBuf,
    pub cgroup: Option<PathBuf>,
}

impl Launch {
    pub fn script(&self) -> String {
        let link = |target: &Path, name: &str| format!("ln -s {} {}", quote_path(target), shell_quote(name));
        [
            "mount --make-rprivate /".to_owned(),
            format!("mount -t tmpfs tmpfs {GUEST_DIR} -o X-mount.mkdir"),
            link(&self.rootfs, GUEST_ROOTFS),
            format!("mkdir -p {}", shell_quote(GUEST_KERNEL_DIR)),
            link(&self.kernel, GUEST_KERNEL),
            format!(
                "exec ip netns exec {} {} --api-sock {}",
                shell_quote(&self.namespace),
                quote_path(&self.binary),
                quote_path(&self.socket)
            ),
        ]
        .join(" && ")
    }
}

fn quote_path(path: &Path) -> String {
    shell_quote(&path.to_string_lossy())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_mounts_a_private_tmpfs_and_execs_firecracker_in_the_netns() {
        let launch = Launch::builder()
            .binary("/opt/e2b/firecracker")
            .socket("/run/sbx/fc.sock")
            .namespace("ns-7")
            .rootfs("/dev/nbd3")
            .kernel("/opt/e2b/vmlinux")
            .build();

        assert_eq!(
            launch.script(),
            "mount --make-rprivate / && mount -t tmpfs tmpfs /fc-vm -o X-mount.mkdir && \
             ln -s '/dev/nbd3' '/fc-vm/rootfs.ext4' && mkdir -p '/fc-vm/kernel' && \
             ln -s '/opt/e2b/vmlinux' '/fc-vm/kernel/vmlinux' && \
             exec ip netns exec 'ns-7' '/opt/e2b/firecracker' --api-sock '/run/sbx/fc.sock'"
        );
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }
}
