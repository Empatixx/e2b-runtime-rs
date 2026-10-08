pub mod stats;

use std::path::{Path, PathBuf};

use crate::go_exec::{CommandFailure, GoCommand};

pub const BLOCK_SIZE: i64 = 4096;
const INODES_RATIO: i64 = 4096;
const RESERVED_BLOCKS_PERCENTAGE: i64 = 0;
const E2FSCK_ACCEPTED_WITH_FIX: i32 = 1 | 2;
const MB: i64 = 1 << 20;
const FEATURES: [&str; 11] = [
    "^64bit",
    "^dir_nlink",
    "ext_attr",
    "extent",
    "extra_isize",
    "filetype",
    "flex_bg",
    "huge_file",
    "inline_data",
    "large_file",
    "sparse_super2",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tools {
    pub mkfs: String,
    pub e2fsck: String,
    pub resize2fs: String,
    pub tune2fs: String,
    pub debugfs: String,
}

impl Default for Tools {
    fn default() -> Self {
        Self {
            mkfs: "mkfs.ext4".to_owned(),
            e2fsck: "e2fsck".to_owned(),
            resize2fs: "resize2fs".to_owned(),
            tune2fs: "tune2fs".to_owned(),
            debugfs: "debugfs".to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ext4 {
    pub file: PathBuf,
    pub tools: Tools,
}

impl Ext4 {
    pub fn new(file: &Path) -> Self {
        Self {
            file: file.to_path_buf(),
            tools: Tools::default(),
        }
    }

    pub async fn make(&self, size_mb: i64, block_size: i64) -> Result<(), String> {
        if block_size < INODES_RATIO {
            return Err("block size must be greater than inodes ratio".to_owned());
        }
        GoCommand::new(&self.tools.mkfs)
            .args(["-O", &FEATURES.join(","), "-b", &block_size.to_string()])
            .args([
                "-m",
                &RESERVED_BLOCKS_PERCENTAGE.to_string(),
                "-i",
                &INODES_RATIO.to_string(),
            ])
            .args([self.file.as_os_str(), format!("{size_mb}M").as_ref()])
            .run()
            .await
            .map(drop)
            .map_err(|e| e.message)
    }

    pub async fn mount(&self, mount_point: &Path) -> Result<(), String> {
        GoCommand::new("mount")
            .args([
                "-o".as_ref(),
                "loop".as_ref(),
                self.file.as_os_str(),
                mount_point.as_os_str(),
            ])
            .run()
            .await
            .map(drop)
            .map_err(|e| e.message)
    }

    pub async fn make_writable(&self) -> Result<(), String> {
        GoCommand::new(&self.tools.tune2fs)
            .args(["-O ^read-only".as_ref(), self.file.as_os_str()])
            .run()
            .await
            .map(drop)
            .map_err(|e| e.message)
    }

    pub async fn enlarge(&self, add: i64) -> Result<i64, String> {
        let size = self.size().map_err(|e| format!("error stating rootfs file: {e}"))?;
        self.resize(size + add).await
    }

    pub async fn resize(&self, target: i64) -> Result<i64, String> {
        let resized = GoCommand::new(&self.tools.resize2fs)
            .args([self.file.as_os_str(), format!("{}M", target / MB).as_ref()])
            .run()
            .await;
        self.after_resize(resized, "error resizing rootfs file").await
    }

    pub async fn shrink(&self) -> Result<i64, String> {
        let shrunk = GoCommand::new(&self.tools.resize2fs)
            .args(["-M".as_ref(), self.file.as_os_str()])
            .run()
            .await;
        self.after_resize(shrunk, "error shrinking rootfs file").await
    }

    async fn after_resize(&self, outcome: Result<Vec<u8>, CommandFailure>, context: &str) -> Result<i64, String> {
        if let Err(e) = outcome {
            self.log_metadata().await;
            return Err(format!("{context}: {}", e.message));
        }
        self.size()
            .map_err(|e| format!("error stating rootfs file after resize: {e}"))
    }

    pub async fn free_space(&self, block_size: i64) -> Result<i64, String> {
        let output = GoCommand::new(&self.tools.debugfs)
            .args(["-R".as_ref(), "stats".as_ref(), self.file.as_os_str()])
            .env("LC_ALL", "C")
            .run()
            .await
            .map_err(|e| {
                tracing::error!(error = %e.message, output = %String::from_utf8_lossy(&e.output), "Error getting free space");
                format!("error statting ext4: {}", e.message)
            })?;
        let output = String::from_utf8_lossy(&output);
        let free = stats::parse_free_blocks(&output).map_err(|e| format!("could not parse free blocks: {e}"))?;
        let reserved =
            stats::parse_reserved_blocks(&output).map_err(|e| format!("could not parse reserved blocks: {e}"))?;
        Ok((free - reserved) * block_size)
    }

    pub async fn check_integrity(&self, fix: bool) -> Result<String, String> {
        self.log_metadata().await;
        let (flags, accepted) = match fix {
            true => ("-pfv", E2FSCK_ACCEPTED_WITH_FIX),
            false => ("-nfv", 0),
        };
        match GoCommand::new(&self.tools.e2fsck)
            .args([flags.as_ref(), self.file.as_os_str()])
            .combined_output()
            .await
        {
            Ok(output) => Ok(String::from_utf8_lossy(&output).trim().to_owned()),
            Err(failure) => integrity_failure(failure, accepted),
        }
    }

    pub async fn replay_journal(&self) -> Result<String, String> {
        match GoCommand::new(&self.tools.e2fsck)
            .args([
                "-p".as_ref(),
                "-E".as_ref(),
                "journal_only".as_ref(),
                self.file.as_os_str(),
            ])
            .combined_output()
            .await
        {
            Ok(output) => Ok(String::from_utf8_lossy(&output).trim().to_owned()),
            Err(failure) => journal_failure(failure, E2FSCK_ACCEPTED_WITH_FIX),
        }
    }

    pub async fn remove_file(&self, path: &str) -> Result<(), String> {
        GoCommand::new(&self.tools.debugfs)
            .args(["-w".as_ref(), "-R".as_ref(), format!("rm \"{path}\"").as_ref(), self.file.as_os_str()])
            .combined_output()
            .await
            .map(drop)
            .map_err(|e| {
                tracing::error!(error = %e.message, output = %String::from_utf8_lossy(&e.output), "error removing file");
                format!("error removing file: {}", e.message)
            })
    }

    pub async fn set_reserved_blocks(&self, reserved_mb: i64, block_size: i64) -> Result<(), String> {
        if reserved_mb <= 0 {
            return Ok(());
        }
        GoCommand::new(&self.tools.tune2fs)
            .args([
                "-r".as_ref(),
                (reserved_mb * MB / block_size).to_string().as_ref(),
                self.file.as_os_str(),
            ])
            .run()
            .await
            .map(drop)
            .map_err(|e| format!("error setting reserved blocks: {}", e.message))
    }

    pub async fn log_metadata(&self) {
        let outcome = GoCommand::new(&self.tools.tune2fs)
            .args(["-l".as_ref(), self.file.as_os_str()])
            .combined_output()
            .await;
        let (output, error) = match outcome {
            Ok(output) => (output, None),
            Err(failure) => (failure.output, Some(failure.message)),
        };
        tracing::debug!(
            path = %self.file.display(),
            output = %String::from_utf8_lossy(&output),
            error = ?error,
            "tune2fs -l output"
        );
    }

    fn size(&self) -> Result<i64, String> {
        std::fs::metadata(&self.file)
            .map(|metadata| metadata.len() as i64)
            .map_err(|e| crate::go_os::path_error("stat", &self.file, &e))
    }
}

pub async fn unmount(path: &Path) -> Result<(), String> {
    GoCommand::new("umount")
        .args([path])
        .run()
        .await
        .map(drop)
        .map_err(|e| format!("error unmounting ext4 filesystem: {}", e.message))
}

fn integrity_failure(failure: CommandFailure, accepted: i32) -> Result<String, String> {
    let output = String::from_utf8_lossy(&failure.output).into_owned();
    match failure.exit_code {
        None => Err(format!("error running e2fsck: {}\n{output}", failure.message)),
        Some(code) if code & !accepted != 0 => Err(format!("error running e2fsck [exit {code}]\n{output}")),
        Some(_) => Ok(output.trim().to_owned()),
    }
}

fn journal_failure(failure: CommandFailure, accepted: i32) -> Result<String, String> {
    let output = String::from_utf8_lossy(&failure.output).into_owned();
    match failure.exit_code {
        Some(code) if code & !accepted == 0 => Ok(output.trim().to_owned()),
        _ => Err(format!("error replaying ext4 journal: {}\n{output}", failure.message)),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn fake(dir: &Path, script: &str) -> Ext4 {
        let e2fsck = dir.join("e2fsck");
        std::fs::write(&e2fsck, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&e2fsck, std::fs::Permissions::from_mode(0o755)).unwrap();
        Ext4 {
            file: dir.join("rootfs.ext4"),
            tools: Tools {
                e2fsck: e2fsck.display().to_string(),
                tune2fs: "true".to_owned(),
                ..Tools::default()
            },
        }
    }

    #[tokio::test]
    async fn e2fsck_exit_codes_follow_upstream() {
        let cases = [
            ("exit 0", true, true),
            ("exit 4", true, false),
            ("exit 8", true, false),
            ("exit 1", false, false),
            ("exit 1", true, true),
            ("exit 2", true, true),
            ("exit 3", true, true),
            ("kill -9 $$", true, false),
        ];

        for (script, fix, ok) in cases {
            let dir = tempfile::tempdir().unwrap();
            let outcome = fake(dir.path(), script).check_integrity(fix).await;
            assert_eq!(outcome.is_ok(), ok, "{script} fix={fix}: {outcome:?}");
        }
    }

    #[tokio::test]
    async fn e2fsck_failures_carry_the_exit_and_output() {
        let dir = tempfile::tempdir().unwrap();
        let killed = fake(dir.path(), "kill -9 $$").check_integrity(true).await;
        let failed = fake(dir.path(), "echo broken; exit 4").check_integrity(true).await;
        let missing = Ext4 {
            tools: Tools {
                e2fsck: dir.path().join("missing").display().to_string(),
                tune2fs: "true".to_owned(),
                ..Tools::default()
            },
            ..Ext4::new(&dir.path().join("rootfs.ext4"))
        }
        .check_integrity(true)
        .await;

        assert_eq!(killed.unwrap_err(), "error running e2fsck: signal: killed\n");
        assert_eq!(failed.unwrap_err(), "error running e2fsck [exit 4]\nbroken\n");
        assert!(missing.unwrap_err().starts_with("error running e2fsck: fork/exec "));
    }

    #[tokio::test]
    async fn journal_replay_accepts_corrected_and_reboot_exit_codes_only() {
        let cases = [
            ("exit 0", true),
            ("exit 1", true),
            ("exit 2", true),
            ("exit 3", true),
            ("exit 4", false),
            ("exit 8", false),
            ("kill -9 $$", false),
        ];

        for (script, ok) in cases {
            let dir = tempfile::tempdir().unwrap();
            let outcome = fake(dir.path(), script).replay_journal().await;
            assert_eq!(outcome.is_ok(), ok, "{script}: {outcome:?}");
        }
    }

    #[tokio::test]
    async fn journal_replay_failures_carry_the_exit_and_output() {
        let dir = tempfile::tempdir().unwrap();

        let failed = fake(dir.path(), "echo broken; exit 4").replay_journal().await;
        let replayed = fake(dir.path(), "echo recovering; exit 1").replay_journal().await;

        assert_eq!(
            failed.unwrap_err(),
            "error replaying ext4 journal: exit status 4\nbroken\n"
        );
        assert_eq!(replayed.unwrap(), "recovering");
    }

    #[tokio::test]
    async fn small_block_sizes_are_refused() {
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(
            Ext4::new(&dir.path().join("x")).make(64, 1024).await,
            Err("block size must be greater than inodes ratio".to_owned())
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn mkfs_keeps_dir_index_when_e2fsprogs_is_present() {
        if GoCommand::new("mkfs.ext4")
            .args(["-V"])
            .combined_output()
            .await
            .is_err()
        {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let ext4 = Ext4::new(&dir.path().join("rootfs.ext4"));

        ext4.make(64, BLOCK_SIZE).await.unwrap();

        let output = GoCommand::new("dumpe2fs")
            .args(["-h".as_ref(), ext4.file.as_os_str()])
            .env("LC_ALL", "C")
            .run()
            .await
            .unwrap();
        let features = String::from_utf8_lossy(&output)
            .lines()
            .find(|line| line.starts_with("Filesystem features:"))
            .map(str::to_owned)
            .unwrap();
        assert!(features.contains("dir_index"), "{features}");
        assert!(ext4.free_space(BLOCK_SIZE).await.unwrap() > 0);
        assert!(ext4.check_integrity(true).await.is_ok());
    }
}
