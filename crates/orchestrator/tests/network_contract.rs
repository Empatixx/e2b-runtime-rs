#![cfg(target_os = "linux")]

use std::process::Command;

use orchestrator::firecracker::launch::Launch;
use orchestrator::firecracker::process::FirecrackerProcess;
use orchestrator::network::slot::Slot;

fn root() -> bool {
    nix::unistd::geteuid().is_root()
}

fn succeeds(program: &str, args: &[&str]) -> bool {
    Command::new(program).args(args).output().unwrap().status.success()
}

#[tokio::test]
async fn slot_is_created_once_and_destroyed_cleanly() {
    if !root() {
        eprintln!("skipping: needs root");
        return;
    }
    let slot = Slot::new(4001).unwrap();
    let _ = slot.destroy().await;

    slot.create().await.unwrap();
    assert!(succeeds(
        "ip",
        &["netns", "exec", "ns-4001", "ip", "link", "show", "tap0"]
    ));
    assert!(succeeds("ip", &["link", "show", "veth-4001"]));
    assert!(succeeds("ip", &["route", "show", "10.11.15.161"]));
    let tap_mac = Command::new("ip")
        .args(["netns", "exec", "ns-4001", "cat", "/sys/class/net/tap0/address"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&tap_mac.stdout).trim(), "02:fc:00:00:00:06");

    let again = slot.create().await.unwrap_err();
    assert!(again.contains("ip netns add ns-4001"), "{again}");
    assert!(
        succeeds("ip", &["link", "show", "veth-4001"]),
        "a failed create must not tear down the live slot"
    );

    slot.destroy().await.unwrap();
    assert!(!succeeds("ip", &["netns", "exec", "ns-4001", "true"]));
    assert!(!succeeds("ip", &["link", "show", "veth-4001"]));
}

#[tokio::test]
async fn destroy_finishes_a_slot_whose_namespace_is_already_gone() {
    if !root() {
        eprintln!("skipping: needs root");
        return;
    }
    let slot = Slot::new(4016).unwrap();
    let _ = slot.destroy().await;
    slot.create().await.unwrap();
    assert!(succeeds("ip", &["netns", "delete", "ns-4016"]));

    slot.destroy().await.unwrap();

    assert!(!succeeds("ip", &["link", "show", "veth-4016"]));
    slot.create().await.unwrap();
    slot.destroy().await.unwrap();
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

fn output(program: &str, args: &[&str]) -> String {
    String::from_utf8(Command::new(program).args(args).output().unwrap().stdout).unwrap()
}

fn gateway() -> String {
    output("ip", &["route", "show", "default"])
        .split_whitespace()
        .skip_while(|word| *word != "dev")
        .nth(1)
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn slot_masquerades_egress_and_firewalls_the_guest() {
    if !root() {
        eprintln!("skipping: needs root");
        return;
    }
    let slot = Slot::new(4008).unwrap();
    let _ = slot.destroy().await;
    slot.create().await.unwrap();
    let gateway = gateway();
    let masquerade = [
        "-w",
        "-t",
        "nat",
        "-C",
        "POSTROUTING",
        "-s",
        "10.11.15.168/32",
        "-o",
        &gateway,
        "-j",
        "MASQUERADE",
    ];
    let spoofing = [
        "netns",
        "exec",
        "ns-4008",
        "iptables",
        "-w",
        "-C",
        "FORWARD",
        "-i",
        "tap0",
        "!",
        "-s",
        "169.254.0.21",
        "-j",
        "DROP",
    ];
    let metadata = [
        "netns",
        "exec",
        "ns-4008",
        "iptables",
        "-w",
        "-C",
        "FORWARD",
        "-i",
        "tap0",
        "-d",
        "169.254.0.0/16",
        "-j",
        "DROP",
    ];
    let has_masquerade = succeeds("iptables", &masquerade);
    let blocks_spoofing = succeeds("ip", &spoofing);
    let blocks_metadata = succeeds("ip", &metadata);
    let broad_forward = succeeds("iptables", &["-w", "-C", "FORWARD", "-i", "veth-4008", "-j", "ACCEPT"]);
    slot.destroy().await.unwrap();

    assert!(has_masquerade, "missing MASQUERADE for the slot host IP");
    assert!(blocks_spoofing, "guest source spoofing is not dropped");
    assert!(blocks_metadata, "guest can reach link-local (metadata) addresses");
    assert!(!broad_forward, "FORWARD accepts everything from the guest veth");
    assert!(
        !succeeds("iptables", &masquerade),
        "MASQUERADE left behind after destroy"
    );
    assert_eq!(output("ip", &["route", "show", "10.11.15.168"]).trim(), "");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slots_can_be_created_and_destroyed_concurrently() {
    if !root() {
        eprintln!("skipping: needs root");
        return;
    }
    let slots: Vec<Slot> = (4010..4016).map(|index| Slot::new(index).unwrap()).collect();
    for slot in &slots {
        let _ = slot.destroy().await;
    }
    let created = futures::future::join_all(slots.iter().map(Slot::create)).await;
    let destroyed = futures::future::join_all(slots.iter().map(Slot::destroy)).await;
    assert!(created.iter().all(Result::is_ok), "{created:?}");
    assert!(destroyed.iter().all(Result::is_ok), "{destroyed:?}");
}

#[tokio::test]
async fn firecracker_that_exits_early_is_reported_not_awaited() {
    if !root() {
        eprintln!("skipping: needs root");
        return;
    }
    let slot = Slot::new(4009).unwrap();
    let _ = slot.destroy().await;
    slot.create().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("fc.sock");
    std::fs::write(&socket, "stale").unwrap();

    let started = std::time::Instant::now();
    let launch = Launch::builder()
        .binary("/bin/false")
        .socket(&socket)
        .namespace(&slot.namespace())
        .console(dir.path().join("console.log"))
        .rootfs("/dev/null")
        .kernel("/dev/null")
        .build()
        .unwrap();
    let result = FirecrackerProcess::spawn(&launch).await;
    slot.destroy().await.unwrap();

    let error = result.err().expect("a firecracker that exits must not look ready");
    assert!(error.starts_with("firecracker exited"), "{error}");
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
}
