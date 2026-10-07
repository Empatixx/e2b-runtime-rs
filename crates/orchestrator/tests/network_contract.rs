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

#[test]
fn slot_masquerades_egress_and_firewalls_the_guest() {
    if !root() {
        eprintln!("skipping: needs root");
        return;
    }
    let slot = Slot::new(4008).unwrap();
    let _ = slot.destroy();
    slot.create().unwrap();
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
    slot.destroy().unwrap();

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

#[test]
fn slots_can_be_created_and_destroyed_concurrently() {
    if !root() {
        eprintln!("skipping: needs root");
        return;
    }
    let slots: Vec<Slot> = (4010..4016).map(|index| Slot::new(index).unwrap()).collect();
    slots.iter().for_each(|slot| {
        let _ = slot.destroy();
    });
    let created: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = slots.iter().map(|slot| scope.spawn(|| slot.create())).collect();
        handles.into_iter().map(|handle| handle.join().unwrap()).collect()
    });
    let destroyed: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = slots.iter().map(|slot| scope.spawn(|| slot.destroy())).collect();
        handles.into_iter().map(|handle| handle.join().unwrap()).collect()
    });
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
    let _ = slot.destroy();
    slot.create().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("fc.sock");
    std::fs::write(&socket, "stale").unwrap();

    let started = std::time::Instant::now();
    let result = orchestrator::firecracker::process::FirecrackerProcess::spawn(
        std::path::Path::new("/bin/false"),
        &socket,
        &slot.namespace(),
        &dir.path().join("console.log"),
    )
    .await;
    slot.destroy().unwrap();

    let error = result.err().expect("a firecracker that exits must not look ready");
    assert!(error.starts_with("firecracker exited"), "{error}");
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
}
