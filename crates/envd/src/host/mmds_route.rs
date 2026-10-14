use std::sync::atomic::{AtomicBool, Ordering};

use super::command::combined_output;

static PINNING: AtomicBool = AtomicBool::new(false);
const RULE: [&str; 8] = ["-d", "169.254.169.254", "-p", "tcp", "--dport", "80", "-j", "RETURN"];

pub async fn pin_mmds_route() -> Result<(), String> {
    if PINNING.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    let pinned = pin_chains().await;
    PINNING.store(false, Ordering::SeqCst);
    pinned
}

async fn pin_chains() -> Result<(), String> {
    let rule = &RULE[..];
    for chain in ["PREROUTING", "OUTPUT"] {
        let _ = iptables(&[&["-D", chain][..], rule].concat()).await;
        iptables(&[&["-I", chain, "1"][..], rule].concat())
            .await
            .map_err(|e| format!("iptables -I nat {chain}: {e}"))?;
    }
    Ok(())
}

async fn iptables(args: &[&str]) -> Result<(), String> {
    let full: Vec<&str> = ["-w", "5", "-t", "nat"]
        .iter()
        .copied()
        .chain(args.iter().copied())
        .collect();
    combined_output("iptables", &full)
        .await
        .map(|_| ())
        .map_err(|failure| format!("{}: {}", failure.error, failure.output))
}
