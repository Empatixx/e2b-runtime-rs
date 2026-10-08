#![cfg(target_os = "linux")]

use orchestrator::builder::config::BuilderConfig;
use orchestrator::builder::ext4::Ext4;
use orchestrator::builder::logs::BuildLog;
use orchestrator::builder::oci::pull::Puller;
use orchestrator::builder::phase::PhaseMeta;
use orchestrator::builder::rootfs::{RootfsRequest, create_ext4_filesystem};
use orchestrator::builder::template_files::{InjectedFiles, provision_script};
use orchestrator::go_exec::GoCommand;

const DISK_SIZE_MB: i64 = 512;

async fn debugfs(ext4: &Ext4, request: &str) -> String {
    let output = GoCommand::new("debugfs")
        .args(["-R".as_ref(), request.as_ref(), ext4.file.as_os_str()])
        .run()
        .await
        .unwrap();
    String::from_utf8_lossy(&output).into_owned()
}

#[tokio::test]
async fn an_alpine_image_becomes_an_ext4_with_the_injected_files() {
    if !nix::unistd::geteuid().is_root() {
        eprintln!("skipped: needs root");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rootfs.filesystem.build");
    let log = BuildLog::default();
    let fields = Default::default();
    let meta = PhaseMeta {
        phase: "base",
        step_type: "base".to_owned(),
        step_number: None,
    };
    let puller = Puller::new(&BuilderConfig::from_vars(&Default::default())).unwrap();

    let config = create_ext4_filesystem(
        &log.scoped(&fields),
        &puller,
        RootfsRequest {
            meta: &meta,
            from_image: "alpine:3.20",
            registry: None,
            disk_size_mb: DISK_SIZE_MB,
            injected: InjectedFiles {
                envd: b"#!/bin/sh\necho envd\n".to_vec(),
                busybox: b"busybox".to_vec(),
                provision_script: provision_script("gcp").unwrap(),
                memory_mb: 1024,
                envd_memory_protection: false,
            },
            path: &path,
        },
    )
    .await
    .unwrap();

    let ext4 = Ext4::new(&path);
    assert!(
        config
            .config
            .env
            .unwrap_or_default()
            .iter()
            .any(|env| env.starts_with("PATH="))
    );
    assert_eq!(
        debugfs(&ext4, "cat /etc/hostname").await.trim_end_matches('\0'),
        "e2b.local"
    );
    assert!(debugfs(&ext4, "stat /etc/hostname").await.contains("Size: 9\n"));
    assert!(debugfs(&ext4, "cat /etc/os-release").await.contains("Alpine"));
    assert!(debugfs(&ext4, "stat /usr/bin/envd").await.contains("Mode:  0777"));
    assert!(
        debugfs(&ext4, "stat /etc/systemd/system/multi-user.target.wants/envd.service")
            .await
            .contains("Fast link dest: \"/etc/systemd/system/envd.service\"")
    );
    assert!(ext4.free_space(4096).await.unwrap() >= DISK_SIZE_MB * (1 << 20) - (64 << 20));
    let lines: Vec<String> = log
        .query(&Default::default())
        .into_iter()
        .map(|entry| entry.message)
        .collect();
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("Root filesystem structure: bin, dev, etc")),
        "{lines:?}"
    );
    assert!(lines.contains(&"Layers extracted".to_owned()));
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(leftovers, vec![std::ffi::OsString::from("rootfs.filesystem.build")]);
}
