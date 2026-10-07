#![cfg(target_os = "linux")]

use std::process::Command;

use orchestrator::network::slot::Slot;

fn root() -> bool {
    nix::unistd::geteuid().is_root()
}

fn succeeds(program: &str, args: &[&str]) -> bool {
    Command::new(program).args(args).output().unwrap().status.success()
}

#[test]
fn slot_is_created_once_and_destroyed_cleanly() {
    if !root() {
        eprintln!("skipping: needs root");
        return;
    }
    let slot = Slot::new(4001).unwrap();
    let _ = slot.destroy();

    slot.create().unwrap();
    assert!(succeeds(
        "ip",
        &["netns", "exec", "ns-4001", "ip", "link", "show", "tap0"]
    ));
    assert!(succeeds("ip", &["link", "show", "veth-4001"]));
    assert!(succeeds("ip", &["route", "show", "10.11.15.161"]));

    let again = slot.create().unwrap_err();
    assert!(again.contains("ip netns add ns-4001"), "{again}");
    assert!(
        succeeds("ip", &["link", "show", "veth-4001"]),
        "a failed create must not tear down the live slot"
    );

    slot.destroy().unwrap();
    assert!(!succeeds("ip", &["netns", "exec", "ns-4001", "true"]));
    assert!(!succeeds("ip", &["link", "show", "veth-4001"]));
}

#[test]
fn cgroup_holds_a_process_and_is_removed() {
    if !root() {
        eprintln!("skipping: needs root");
        return;
    }
    let cgroup = orchestrator::cgroup::Cgroup::create("contract-test").unwrap();
    let mut child = Command::new("sleep").arg("30").spawn().unwrap();
    cgroup.add(child.id()).unwrap();
    assert_eq!(cgroup.pids().unwrap(), vec![child.id()]);

    child.kill().unwrap();
    child.wait().unwrap();
    let path = cgroup.path().to_path_buf();
    cgroup.remove().unwrap();
    assert!(!path.exists());
}
