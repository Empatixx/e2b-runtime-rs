use std::path::{Path, PathBuf};

pub const GUEST_DIR: &str = "/fc-vm";
pub const GUEST_ROOTFS: &str = "/fc-vm/rootfs.ext4";
pub const GUEST_KERNEL_DIR: &str = "/fc-vm/kernel";
pub const GUEST_KERNEL: &str = "/fc-vm/kernel/vmlinux";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub binary: PathBuf,
    pub socket: PathBuf,
    pub namespace: String,
    pub console: PathBuf,
    pub rootfs: PathBuf,
    pub kernel: PathBuf,
}

#[derive(Debug, Default)]
pub struct LaunchBuilder {
    binary: Option<PathBuf>,
    socket: Option<PathBuf>,
    namespace: Option<String>,
    console: Option<PathBuf>,
    rootfs: Option<PathBuf>,
    kernel: Option<PathBuf>,
}

impl Launch {
    pub fn builder() -> LaunchBuilder {
        LaunchBuilder::default()
    }

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

impl LaunchBuilder {
    pub fn binary(mut self, binary: impl Into<PathBuf>) -> Self {
        self.binary = Some(binary.into());
        self
    }

    pub fn socket(mut self, socket: impl Into<PathBuf>) -> Self {
        self.socket = Some(socket.into());
        self
    }

    pub fn namespace(mut self, namespace: &str) -> Self {
        self.namespace = Some(namespace.to_owned());
        self
    }

    pub fn console(mut self, console: impl Into<PathBuf>) -> Self {
        self.console = Some(console.into());
        self
    }

    pub fn rootfs(mut self, rootfs: impl Into<PathBuf>) -> Self {
        self.rootfs = Some(rootfs.into());
        self
    }

    pub fn kernel(mut self, kernel: impl Into<PathBuf>) -> Self {
        self.kernel = Some(kernel.into());
        self
    }

    pub fn build(self) -> Result<Launch, String> {
        Ok(Launch {
            binary: self.binary.ok_or("firecracker binary is required")?,
            socket: self.socket.ok_or("api socket is required")?,
            namespace: self.namespace.ok_or("network namespace is required")?,
            console: self.console.ok_or("console path is required")?,
            rootfs: self.rootfs.ok_or("rootfs is required")?,
            kernel: self.kernel.ok_or("kernel is required")?,
        })
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
            .console("/run/sbx/console.log")
            .rootfs("/dev/nbd3")
            .kernel("/opt/e2b/vmlinux")
            .build()
            .unwrap();

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

    #[test]
    fn build_requires_every_path() {
        assert_eq!(Launch::builder().build().unwrap_err(), "firecracker binary is required");
    }
}
