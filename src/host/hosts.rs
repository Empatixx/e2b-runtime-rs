use std::path::Path;

pub const EVENTS_HOST: &str = "events.e2b.local";

pub fn rewrite_hosts_file(address: &str, path: &Path) -> std::io::Result<()> {
    let current = std::fs::read_to_string(path).unwrap_or_default();
    let names_events = |line: &&str| line.split_whitespace().skip(1).any(|host| host == EVENTS_HOST);

    let already_set = current
        .lines()
        .filter(names_events)
        .any(|line| line.split_whitespace().eq([address, EVENTS_HOST]));
    if already_set {
        return Ok(());
    }

    let entry = format!("{address}\t{EVENTS_HOST}");
    let lines: Vec<&str> = current
        .lines()
        .filter(|line| !names_events(line))
        .chain([entry.as_str()])
        .collect();
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
        assert_eq!(
            std::fs::read_to_string(tmp.path()).unwrap(),
            "127.0.0.1\tlocalhost\n10.0.0.2\tevents.e2b.local\n"
        );
    }
}
