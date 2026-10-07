use uuid::Uuid;

const MEMFILE: &str = "memfile";
const ROOTFS: &str = "rootfs.ext4";
const SNAPFILE: &str = "snapfile";
const METADATA: &str = "metadata.json";
const HEADER_SUFFIX: &str = ".header";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildPaths {
    pub build_id: Uuid,
}

impl BuildPaths {
    pub fn new(build_id: Uuid) -> Self {
        Self { build_id }
    }

    pub fn memfile(&self) -> String {
        self.file(MEMFILE)
    }

    pub fn memfile_header(&self) -> String {
        self.header(MEMFILE)
    }

    pub fn rootfs(&self) -> String {
        self.file(ROOTFS)
    }

    pub fn rootfs_header(&self) -> String {
        self.header(ROOTFS)
    }

    pub fn snapfile(&self) -> String {
        self.file(SNAPFILE)
    }

    pub fn metadata(&self) -> String {
        self.file(METADATA)
    }

    fn file(&self, name: &str) -> String {
        format!("{}/{name}", self.build_id)
    }

    fn header(&self, name: &str) -> String {
        format!("{}/{name}{HEADER_SUFFIX}", self.build_id)
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    #[test]
    fn object_names_match_upstream() {
        let build_id = Uuid::from_u128(0x1234);
        let paths = BuildPaths::new(build_id);
        let prefix = build_id.to_string();
        assert_eq!(paths.memfile(), format!("{prefix}/memfile"));
        assert_eq!(paths.memfile_header(), format!("{prefix}/memfile.header"));
        assert_eq!(paths.rootfs(), format!("{prefix}/rootfs.ext4"));
        assert_eq!(paths.rootfs_header(), format!("{prefix}/rootfs.ext4.header"));
        assert_eq!(paths.snapfile(), format!("{prefix}/snapfile"));
        assert_eq!(paths.metadata(), format!("{prefix}/metadata.json"));
    }
}
