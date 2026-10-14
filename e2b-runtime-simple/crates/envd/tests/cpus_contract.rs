use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use envd::cpus::sysfs::{CpuSysfs, KernelSysfs};
use envd::reaper::Registry;

const ENVD: &str = env!("CARGO_BIN_EXE_envd");

fn sysfs_root() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("possible"), "0-3\n").unwrap();
    fs::write(root.path().join("online"), "0-1\n").unwrap();
    fs::create_dir(root.path().join("cpu2")).unwrap();
    fs::write(root.path().join("cpu2").join("online"), "0\n").unwrap();
    fs::create_dir(root.path().join("cpu3")).unwrap();
    root
}

fn kernel_sysfs(root: &Path, children: &Arc<Registry>) -> KernelSysfs {
    KernelSysfs::builder()
        .dir(root)
        .executable(ENVD)
        .children(children.clone())
        .build()
}

#[tokio::test]
async fn the_helper_child_flips_a_cpu_through_the_online_file() {
    let root = sysfs_root();
    let children = Arc::new(Registry::default());
    let sysfs = kernel_sysfs(root.path(), &children);

    for (online, written) in [(true, "1"), (false, "0")] {
        sysfs.set_online(2, online).await.unwrap();
        assert_eq!(
            fs::read_to_string(root.path().join("cpu2").join("online")).unwrap(),
            written
        );
    }

    assert!(
        children.export_hold().pids.is_empty(),
        "a finished write leaves nothing to hand over"
    );
}

#[tokio::test]
async fn an_attribute_the_kernel_did_not_create_is_never_made_up() {
    let root = sysfs_root();
    let children = Arc::new(Registry::default());
    let sysfs = kernel_sysfs(root.path(), &children);

    let error = sysfs.set_online(3, true).await.unwrap_err();

    assert!(error.contains("cpu3/online: no such file"), "{error}");
    assert!(error.starts_with("exit status 1: "), "{error}");
    assert!(!root.path().join("cpu3").join("online").exists());
}

#[test]
fn the_masks_of_the_fake_sysfs_are_read_through_the_public_api() {
    let root = sysfs_root();
    let sysfs = kernel_sysfs(root.path(), &Arc::default());

    assert_eq!(sysfs.possible(), Ok(vec![0, 1, 2, 3]));
    assert_eq!(sysfs.online(), Ok(vec![0, 1]));
}

#[test]
fn a_helper_error_fails_the_child_with_the_message() {
    let output = Command::new(ENVD)
        .args(["cpu-online", "/etc/passwd", "1"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "\"/etc/passwd\" is not a cpuN/online file\n"
    );
}

#[test]
fn a_marked_child_without_its_helper_exits_instead_of_running_the_program() {
    let output = Command::new(ENVD)
        .arg("no-such-helper")
        .env("E2B_REEXEC_HELPER", "no-such-helper")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("no such helper"));
}
