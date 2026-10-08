use std::io;
use std::process::{Command, Stdio};

const IP_FORWARD: &str = "/proc/sys/net/ipv4/ip_forward";
const IPTABLES_NEEDED_FOR: &str = "iptables (needed for the per-slot FORWARD accepts)";
const IPTABLES_NOT_FOUND: &str = "exec: \"iptables\": executable file not found in $PATH";

pub fn require_v2_prerequisites() -> Result<(), String> {
    check_v2_prerequisites(std::fs::read_to_string(IP_FORWARD), iptables_available())
}

fn iptables_available() -> Result<(), String> {
    Command::new("iptables")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => IPTABLES_NOT_FOUND.to_owned(),
            _ => e.to_string(),
        })
        .and_then(|status| {
            status
                .success()
                .then_some(())
                .ok_or_else(|| format!("iptables --version: {status}"))
        })
}

fn check_v2_prerequisites(forwarding: io::Result<String>, iptables: Result<(), String>) -> Result<(), String> {
    let failures: Vec<String> = [
        forwarding_failure(forwarding),
        iptables.err().map(|e| format!("{IPTABLES_NEEDED_FOR}: {e}")),
    ]
    .into_iter()
    .flatten()
    .collect();
    match failures.is_empty() {
        true => Ok(()),
        false => Err(format!("v2 networking prerequisites not met: {}", failures.join("\n"))),
    }
}

fn forwarding_failure(value: io::Result<String>) -> Option<String> {
    match value {
        Err(e) => Some(format!("cannot read {IP_FORWARD}: {e}")),
        Ok(value) if value.trim() == "1" => None,
        Ok(value) => Some(format!(
            "IPv4 forwarding: {IP_FORWARD} must be 1, got {:?}",
            value.trim()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forwarding_must_be_exactly_one() {
        assert_eq!(check_v2_prerequisites(Ok("1\n".to_owned()), Ok(())), Ok(()));
        assert_eq!(
            check_v2_prerequisites(Ok("0\n".to_owned()), Ok(())),
            Err("v2 networking prerequisites not met: IPv4 forwarding: /proc/sys/net/ipv4/ip_forward must be 1, got \"0\"".to_owned())
        );
    }

    #[test]
    fn an_unreadable_file_is_reported() {
        let error = check_v2_prerequisites(Err(io::Error::from(io::ErrorKind::NotFound)), Ok(())).unwrap_err();

        assert!(error.starts_with("v2 networking prerequisites not met: cannot read /proc/sys/net/ipv4/ip_forward: "));
    }

    #[test]
    fn every_failure_is_listed_on_its_own_line() {
        let error = check_v2_prerequisites(Ok("0".to_owned()), Err(IPTABLES_NOT_FOUND.to_owned())).unwrap_err();

        assert_eq!(
            error,
            "v2 networking prerequisites not met: IPv4 forwarding: /proc/sys/net/ipv4/ip_forward must be 1, got \"0\"\niptables (needed for the per-slot FORWARD accepts): exec: \"iptables\": executable file not found in $PATH"
        );
    }
}
