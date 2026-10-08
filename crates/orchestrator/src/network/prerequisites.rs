use std::io;

const IP_FORWARD: &str = "/proc/sys/net/ipv4/ip_forward";

pub fn require_ip_forwarding() -> Result<(), String> {
    check_ip_forwarding(std::fs::read_to_string(IP_FORWARD))
}

fn check_ip_forwarding(value: io::Result<String>) -> Result<(), String> {
    let failure = match value {
        Err(e) => format!("cannot read {IP_FORWARD}: {e}"),
        Ok(value) if value.trim() == "1" => return Ok(()),
        Ok(value) => format!("IPv4 forwarding: {IP_FORWARD} must be 1, got {:?}", value.trim()),
    };
    Err(format!("v2 networking prerequisites not met: {failure}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forwarding_must_be_exactly_one() {
        assert_eq!(check_ip_forwarding(Ok("1\n".to_owned())), Ok(()));
        assert_eq!(
            check_ip_forwarding(Ok("0\n".to_owned())),
            Err("v2 networking prerequisites not met: IPv4 forwarding: /proc/sys/net/ipv4/ip_forward must be 1, got \"0\"".to_owned())
        );
    }

    #[test]
    fn an_unreadable_file_is_reported() {
        let error = check_ip_forwarding(Err(io::Error::from(io::ErrorKind::NotFound))).unwrap_err();

        assert!(error.starts_with("v2 networking prerequisites not met: cannot read /proc/sys/net/ipv4/ip_forward: "));
    }
}
