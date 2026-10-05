//! Hyperloop: point `events.e2b.local` at the given IP in /etc/hosts (upstream `rewriteHostsFile`).

use std::path::Path;

pub const EVENTS_HOST: &str = "events.e2b.local";

pub fn rewrite_hosts_file(address: &str, path: &Path) -> std::io::Result<()> {
    let current = std::fs::read_to_string(path).unwrap_or_default();
    let mut lines: Vec<&str> = current
        .lines()
        .filter(|line| !line.split_whitespace().skip(1).any(|h| h == EVENTS_HOST))
        .collect();
    let entry = format!("{address}\t{EVENTS_HOST}");
    if current.lines().any(|l| l.split_whitespace().collect::<Vec<_>>() == [address, EVENTS_HOST]) {
        return Ok(());
    }
    lines.push(&entry);
    std::fs::write(path, lines.join("\n") + "\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_existing_entry() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "127.0.0.1\tlocalhost\n10.0.0.1\tevents.e2b.local\n").unwrap();
        rewrite_hosts_file("10.0.0.2", tmp.path()).unwrap();
        assert_eq!(std::fs::read_to_string(tmp.path()).unwrap(), "127.0.0.1\tlocalhost\n10.0.0.2\tevents.e2b.local\n");
    }
}
