pub const STAT_UNPARSEABLE: &str = "debugfs stat output has no parseable size";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    Unknown,
    Absent,
    NotExecutable,
    Executable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Content {
    Unreadable,
    Target,
    Original,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Applied,
    OriginalIntact,
    Damaged,
    Unknown,
}

pub fn classify(presence: Presence, content: Content) -> Verdict {
    match (presence, content) {
        (Presence::Unknown, _) => Verdict::Unknown,
        (Presence::Absent, _) => Verdict::Damaged,
        (_, Content::Unreadable) => Verdict::Unknown,
        (Presence::NotExecutable, _) => Verdict::Damaged,
        (Presence::Executable, Content::Target) => Verdict::Applied,
        (Presence::Executable, Content::Original) => Verdict::OriginalIntact,
        (Presence::Executable, Content::Other) => Verdict::Damaged,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvdState {
    pub presence: Presence,
    pub content: Content,
    pub sha: String,
    pub stat_error: Option<String>,
    pub dump_error: Option<String>,
}

impl EnvdState {
    pub fn unread(presence: Presence) -> Self {
        Self {
            presence,
            content: Content::Unreadable,
            sha: String::new(),
            stat_error: None,
            dump_error: None,
        }
    }

    pub fn verdict(&self) -> Verdict {
        classify(self.presence, self.content)
    }

    pub fn describe(&self) -> String {
        let presence = match self.presence {
            Presence::Unknown => "un-stat-able",
            Presence::Absent => "absent",
            Presence::NotExecutable => "present but not an executable regular file",
            Presence::Executable => "present and executable",
        };
        let content = match self.content {
            Content::Unreadable => "unreadable",
            Content::Target => "the target binary",
            Content::Original => "the original binary",
            Content::Other => "neither the target nor the original",
        };
        let sha = (!self.sha.is_empty()).then(|| format!(" (sha {})", &self.sha[..self.sha.len().min(12)]));
        let stat = self.stat_error.as_ref().map(|e| format!("; stat: {e}"));
        let dump = self.dump_error.as_ref().map(|e| format!("; dump: {e}"));
        [sha, stat, dump]
            .into_iter()
            .flatten()
            .fold(format!("{presence}, content {content}"), |out, part| out + &part)
    }
}

pub fn envd_absent(stat: &str) -> bool {
    let lower = stat.to_lowercase();
    lower.contains("file not found") || lower.contains("no such file")
}

pub fn envd_executable(stat: &str) -> bool {
    stat.contains("Type: regular") && mode_bits(stat).is_some_and(|mode| mode & 0o100 != 0)
}

fn mode_bits(stat: &str) -> Option<u32> {
    let rest = stat.split_once("Mode:")?.1.trim_start().trim_start_matches('0');
    let digits: String = rest.chars().take_while(|c| ('0'..='7').contains(c)).collect();
    (3..=4)
        .contains(&digits.len())
        .then(|| u32::from_str_radix(&digits, 8).ok())
        .flatten()
}

pub fn parse_envd_size(stat: &str) -> Result<i64, String> {
    stat.match_indices("Size:")
        .filter(|(at, _)| {
            stat[..*at]
                .chars()
                .last()
                .is_none_or(|before| !before.is_ascii_alphanumeric() && before != '_')
        })
        .map(|(at, key)| stat[at + key.len()..].trim_start())
        .map(|rest| rest.chars().take_while(char::is_ascii_digit).collect::<String>())
        .find(|digits| !digits.is_empty())
        .ok_or_else(|| STAT_UNPARSEABLE.to_owned())
        .and_then(|digits| digits.parse().map_err(|_| STAT_UNPARSEABLE.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_cell_of_the_decision_table_matches_go() {
        use Content::*;
        use Presence::*;
        use Verdict::*;
        let table = [
            (
                Presence::Unknown,
                [Verdict::Unknown, Verdict::Unknown, Verdict::Unknown, Verdict::Unknown],
            ),
            (Absent, [Damaged, Damaged, Damaged, Damaged]),
            (NotExecutable, [Verdict::Unknown, Damaged, Damaged, Damaged]),
            (Executable, [Verdict::Unknown, Applied, OriginalIntact, Damaged]),
        ];
        for (presence, verdicts) in table {
            for (content, verdict) in [Unreadable, Target, Original, Other].into_iter().zip(verdicts) {
                assert_eq!(classify(presence, content), verdict, "{presence:?}/{content:?}");
            }
        }
    }

    #[test]
    fn an_unreadable_inode_never_authorises_the_rollback() {
        for presence in [Presence::Unknown, Presence::NotExecutable, Presence::Executable] {
            assert_eq!(
                classify(presence, Content::Unreadable),
                Verdict::Unknown,
                "{presence:?}"
            );
        }
        assert_eq!(classify(Presence::Absent, Content::Unreadable), Verdict::Damaged);
        assert_eq!(classify(Presence::Executable, Content::Target), Verdict::Applied);
    }

    #[test]
    fn the_exec_bit_is_read_from_a_regular_file_mode() {
        assert!(envd_executable(
            "Inode: 12   Type: regular    Mode:  0755   Flags: 0x0\nUser: 0   Group: 0   Size: 100\n"
        ));
        assert!(envd_executable("Type: regular    Mode:  0555"));
        assert!(!envd_executable(
            "Inode: 12   Type: regular    Mode:  0644   Flags: 0x0\nUser: 0   Group: 0   Size: 100\n"
        ));
        assert!(!envd_executable(
            "Inode: 2   Type: directory    Mode:  0755   Flags: 0x0\n"
        ));
        assert!(!envd_executable(""));
        assert!(!envd_executable("Type: regular"));
    }

    #[test]
    fn the_size_is_the_inode_size_and_never_a_decoy() {
        let stat = "Inode: 12   Type: regular    Mode:  0755   Flags: 0x80000\n\
                    Generation: 0    Version: 0x00000000:00000001\n\
                    User:     0   Group:     0   Project:     0   Size: 18967672\n\
                    File ACL: 0\n\
                    Links: 1   Blockcount: 37048\n\
                    Fragment:  Address: 0    Number: 0    Size: of extra inode fields: 32\n";

        assert_eq!(parse_envd_size(stat), Ok(18_967_672));
        assert_eq!(
            parse_envd_size("Type: regular   Mode: 0755   Size: 8589934592"),
            Ok(8_589_934_592)
        );
        let unparseable = Err(STAT_UNPARSEABLE.to_owned());
        assert_eq!(
            parse_envd_size("Inode: 12   Type: regular    Mode:  0755\n"),
            unparseable
        );
        assert_eq!(parse_envd_size(""), unparseable);
        assert_eq!(
            parse_envd_size("Type: regular   Mode: 0755   Size: 18446744073709551615"),
            unparseable
        );
    }

    #[test]
    fn absence_is_read_from_the_debugfs_message() {
        for stat in [
            "/usr/bin/envd: File not found by ext2_lookup",
            "stat: File not found by ext2_lookup while looking up /usr/bin/envd",
            "debugfs: No such file or directory while opening /usr/bin/envd",
        ] {
            assert!(envd_absent(stat), "{stat}");
        }
        for stat in [
            "Inode: 12   Type: regular    Mode:  0755   Flags: 0x80000",
            "",
            "debugfs: Filesystem opened read/write",
        ] {
            assert!(!envd_absent(stat), "{stat}");
        }
    }
}
