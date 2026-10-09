use std::collections::BTreeMap;
use std::ops::Range;

const BLOCK_SIZE: usize = 512;
const NAME_SIZE: usize = 100;
const PREFIX_SIZE: usize = 155;
const MODE_LIMIT: u64 = 1 << 21;
const SIZE_LIMIT: u64 = 1 << 33;

const NAME: Range<usize> = 0..100;
const MODE: Range<usize> = 100..108;
const UID: Range<usize> = 108..116;
const GID: Range<usize> = 116..124;
const SIZE: Range<usize> = 124..136;
const MODIFIED: Range<usize> = 136..148;
const CHECKSUM: Range<usize> = 148..156;
const TYPE_FLAG: usize = 156;
const LINK_NAME: Range<usize> = 157..257;
const MAGIC: Range<usize> = 257..265;
const DEV_MAJOR: Range<usize> = 329..337;
const DEV_MINOR: Range<usize> = 337..345;
const PREFIX: Range<usize> = 345..500;
const USTAR_MAGIC_AND_VERSION: &[u8; 8] = b"ustar\x0000";
const PAX_HEADER_FLAG: u8 = b'x';

#[derive(Clone, Copy)]
pub enum EntryKind {
    Regular,
    Symlink,
}

impl EntryKind {
    fn flag(self) -> u8 {
        match self {
            Self::Regular => b'0',
            Self::Symlink => b'2',
        }
    }
}

pub struct TarEntry<'a> {
    pub name: &'a str,
    pub link_name: &'a str,
    pub mode: u32,
    pub kind: EntryKind,
    pub data: &'a [u8],
}

impl<'a> TarEntry<'a> {
    pub fn file(name: &'a str, mode: u32, data: &'a [u8]) -> Self {
        Self {
            name,
            link_name: "",
            mode,
            kind: EntryKind::Regular,
            data,
        }
    }

    pub fn symlink(name: &'a str, mode: u32, target: &'a str) -> Self {
        Self {
            name,
            link_name: target,
            mode,
            kind: EntryKind::Symlink,
            data: &[],
        }
    }
}

enum Layout<'a> {
    Ustar { prefix: &'a str, name: &'a str },
    Pax { records: BTreeMap<&'static str, String> },
}

#[derive(Default)]
pub struct TarWriter {
    bytes: Vec<u8>,
}

impl TarWriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn append(&mut self, entry: &TarEntry) -> Result<(), String> {
        let layout = Layout::choose(entry)?;
        match layout {
            Layout::Ustar { prefix, name } => self.append_ustar(entry, prefix, name),
            Layout::Pax { records } => self.append_pax(entry, &records),
        }
        self.bytes.extend_from_slice(entry.data);
        self.pad(entry.data.len());
        Ok(())
    }

    pub fn finish(mut self) -> Vec<u8> {
        self.bytes.resize(self.bytes.len() + 2 * BLOCK_SIZE, 0);
        self.bytes
    }

    fn append_ustar(&mut self, entry: &TarEntry, prefix: &str, name: &str) {
        let mut block = Block::for_entry(entry, name, entry.link_name);
        block.set_string(PREFIX, prefix);
        self.push(block);
    }

    fn append_pax(&mut self, entry: &TarEntry, records: &BTreeMap<&'static str, String>) {
        if !records.is_empty() {
            let data = records
                .iter()
                .map(|(key, value)| pax_record(key, value))
                .collect::<String>();
            self.push(Block::pax_header(entry.name, data.len()));
            self.bytes.extend_from_slice(data.as_bytes());
            self.pad(data.len());
        }
        let block = Block::for_entry(entry, &to_ascii(entry.name), &to_ascii(entry.link_name));
        self.push(block);
    }

    fn push(&mut self, mut block: Block) {
        block.set_magic_and_checksum();
        self.bytes.extend_from_slice(&block.bytes);
    }

    fn pad(&mut self, length: usize) {
        self.bytes.resize(self.bytes.len() + padding(length), 0);
    }
}

impl<'a> Layout<'a> {
    fn choose(entry: &'a TarEntry) -> Result<Self, String> {
        if matches!(entry.kind, EntryKind::Regular) && entry.name.ends_with('/') {
            return Err(header_error("filename may not have trailing slash"));
        }
        if u64::from(entry.mode) >= MODE_LIMIT {
            return Err(header_error("GNU format is not supported"));
        }
        let split = split_ustar_path(entry.name);
        let name_needs_extension = entry.name.len() > NAME_SIZE || !entry.name.is_ascii();
        let link_needs_extension = entry.link_name.len() > NAME_SIZE || !entry.link_name.is_ascii();
        let size_needs_extension = entry.data.len() as u64 >= SIZE_LIMIT;
        let ustar_fits = (!name_needs_extension || split.is_some()) && !link_needs_extension && !size_needs_extension;
        if !ustar_fits {
            return Ok(Self::Pax {
                records: pax_records(entry, name_needs_extension, link_needs_extension, size_needs_extension),
            });
        }
        Ok(match split.filter(|_| name_needs_extension) {
            Some((prefix, name)) => Self::Ustar { prefix, name },
            None => Self::Ustar {
                prefix: "",
                name: entry.name,
            },
        })
    }
}

fn pax_records(entry: &TarEntry, name: bool, link_name: bool, size: bool) -> BTreeMap<&'static str, String> {
    [
        name.then(|| ("path", entry.name.to_owned())),
        link_name.then(|| ("linkpath", entry.link_name.to_owned())),
        size.then(|| ("size", entry.data.len().to_string())),
    ]
    .into_iter()
    .flatten()
    .collect()
}

struct Block {
    bytes: [u8; BLOCK_SIZE],
}

impl Block {
    fn empty() -> Self {
        Self { bytes: [0; BLOCK_SIZE] }
    }

    fn for_entry(entry: &TarEntry, name: &str, link_name: &str) -> Self {
        let mut block = Self::empty();
        block.set_string(NAME, name);
        block.set_string(LINK_NAME, link_name);
        block.set_octal(MODE, u64::from(entry.mode));
        block.set_octal(UID, 0);
        block.set_octal(GID, 0);
        block.set_octal(SIZE, entry.data.len() as u64);
        block.set_octal(MODIFIED, 0);
        block.set_octal(DEV_MAJOR, 0);
        block.set_octal(DEV_MINOR, 0);
        block.bytes[TYPE_FLAG] = entry.kind.flag();
        block
    }

    fn pax_header(real_name: &str, data_length: usize) -> Self {
        let mut block = Self::empty();
        let name = to_ascii(&pax_header_name(real_name));
        let name = name.get(..NAME_SIZE).unwrap_or(&name).trim_end_matches('/');
        block.set_string(NAME, name);
        block.set_octal(MODE, 0);
        block.set_octal(UID, 0);
        block.set_octal(GID, 0);
        block.set_octal(SIZE, data_length as u64);
        block.set_octal(MODIFIED, 0);
        block.bytes[TYPE_FLAG] = PAX_HEADER_FLAG;
        block
    }

    fn set_string(&mut self, field: Range<usize>, value: &str) {
        let field = &mut self.bytes[field];
        let length = value.len().min(field.len());
        field[..length].copy_from_slice(&value.as_bytes()[..length]);
        if let Some(terminator) = field.get_mut(value.len()) {
            *terminator = 0;
        }
        if value.len() > field.len() && field.last() == Some(&b'/') {
            let kept = value[..field.len() - 1].trim_end_matches('/').len();
            field[kept] = 0;
        }
    }

    fn set_octal(&mut self, field: Range<usize>, value: u64) {
        let width = field.len();
        let fits = value < 1 << ((width - 1) * 3);
        let digits = format!("{:o}", if fits { value } else { 0 });
        let padded = format!("{digits:0>width$}", width = width - 1);
        self.set_string(field, &padded);
    }

    fn set_magic_and_checksum(&mut self) {
        self.bytes[MAGIC].copy_from_slice(USTAR_MAGIC_AND_VERSION);
        self.bytes[CHECKSUM].fill(b' ');
        let sum = self.bytes.iter().map(|&byte| u64::from(byte)).sum::<u64>();
        self.set_octal(CHECKSUM.start..CHECKSUM.end - 1, sum);
        self.bytes[CHECKSUM.end - 1] = b' ';
    }
}

fn split_ustar_path(name: &str) -> Option<(&str, &str)> {
    let mut length = name.len();
    if length <= NAME_SIZE || !name.is_ascii() {
        return None;
    }
    if length > PREFIX_SIZE + 1 {
        length = PREFIX_SIZE + 1;
    } else if name.as_bytes()[length - 1] == b'/' {
        length -= 1;
    }
    let slash = name[..length].rfind('/')?;
    let suffix = &name[slash + 1..];
    (slash > 0 && !suffix.is_empty() && suffix.len() <= NAME_SIZE && slash <= PREFIX_SIZE)
        .then(|| (&name[..slash], suffix))
}

fn pax_record(key: &str, value: &str) -> String {
    let base = key.len() + value.len() + 3;
    let size = base + base.to_string().len();
    let record = format!("{size} {key}={value}\n");
    match record.len() == size {
        true => record,
        false => format!("{} {key}={value}\n", record.len()),
    }
}

fn pax_header_name(real_name: &str) -> String {
    let (dir, file) = real_name.split_at(real_name.rfind('/').map_or(0, |slash| slash + 1));
    clean_path(
        &[dir, "PaxHeaders.0", file]
            .into_iter()
            .filter(|element| !element.is_empty())
            .collect::<Vec<_>>()
            .join("/"),
    )
}

fn clean_path(path: &str) -> String {
    let rooted = path.starts_with('/');
    let mut kept: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." if kept.last().is_some_and(|last| *last != "..") => {
                kept.pop();
            }
            ".." if rooted => {}
            other => kept.push(other),
        }
    }
    match (rooted, kept.is_empty()) {
        (true, true) => "/".to_owned(),
        (false, true) => ".".to_owned(),
        (true, false) => format!("/{}", kept.join("/")),
        (false, false) => kept.join("/"),
    }
}

fn to_ascii(value: &str) -> String {
    value.chars().filter(|&c| c != '\0' && c.is_ascii()).collect()
}

fn padding(length: usize) -> usize {
    (BLOCK_SIZE - length % BLOCK_SIZE) % BLOCK_SIZE
}

fn header_error(cause: &str) -> String {
    format!("archive/tar: cannot encode header: {cause}")
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};

    use super::*;

    fn sha256_hex(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    fn files_tar(files: &[(String, &[u8], u32)]) -> Vec<u8> {
        let mut sorted = files.to_vec();
        sorted.sort();
        let mut writer = TarWriter::new();
        sorted
            .iter()
            .for_each(|(name, data, mode)| writer.append(&TarEntry::file(name, *mode, data)).unwrap());
        writer.finish()
    }

    fn links_tar(links: &[(String, String)]) -> Vec<u8> {
        let mut sorted = links.to_vec();
        sorted.sort();
        let mut writer = TarWriter::new();
        sorted
            .iter()
            .for_each(|(name, target)| writer.append(&TarEntry::symlink(name, 0o777, target)).unwrap());
        writer.finish()
    }

    #[test]
    fn long_and_non_ascii_names_match_go_archive_tar() {
        let files = [
            (
                format!("{}/{}/file.txt", "d".repeat(60), "e".repeat(60)),
                &b"splittable"[..],
                0o644,
            ),
            ("x".repeat(150), b"unsplittable", 0o600),
            ("etc/\u{fc}n\u{ef}.conf".to_owned(), b"non ascii", 0o644),
            (format!("/{}/{}/c", "a".repeat(90), "b".repeat(90)), b"rooted", 0o755),
            (format!("dir/{}", "y".repeat(140)), b"", 0o777),
            (format!("{}/{}", "p".repeat(99), "q".repeat(100)), b"exact", 0o644),
            (format!("a/../{}", "z".repeat(120)), b"dots", 0o644),
        ];

        assert_eq!(
            sha256_hex(&files_tar(&files)),
            "fce70035bf371112cea8dfc0eeba4f4b9c8c8fdbfdfda9544835da3f5df5f1ed"
        );
    }

    #[test]
    fn long_and_non_ascii_link_targets_match_go_archive_tar() {
        let links = [
            ("short/link".to_owned(), "t".repeat(130)),
            (format!("{}/link2", "l".repeat(120)), "/target".to_owned()),
            ("etc/\u{e9}".to_owned(), "/t\u{e4}rget".to_owned()),
            ("ok/link".to_owned(), "/ok/target".to_owned()),
        ];

        assert_eq!(
            sha256_hex(&links_tar(&links)),
            "3a2953c9496a6b89ca83bef34d9f4cbd38cf2bbfd007fdd06f080731735d435d"
        );
    }

    #[test]
    fn pax_records_carry_their_own_length_like_go() {
        assert_eq!(pax_record("path", "a"), "9 path=a\n");
        assert_eq!(
            pax_record("size", &"9".repeat(94)),
            format!("104 size={}\n", "9".repeat(94))
        );
        assert_eq!(
            pax_record("path", &"b".repeat(91)),
            format!("101 path={}\n", "b".repeat(91))
        );
    }

    #[test]
    fn cleans_paths_like_go_path_clean() {
        assert_eq!(clean_path("/usr/bin//PaxHeaders.0/x"), "/usr/bin/PaxHeaders.0/x");
        assert_eq!(clean_path("a/../b/./c"), "b/c");
        assert_eq!(clean_path("/../a"), "/a");
        assert_eq!(clean_path("../a"), "../a");
        assert_eq!(clean_path(""), ".");
    }

    #[test]
    fn regular_files_with_a_trailing_slash_are_rejected_like_go() {
        let error = TarWriter::new()
            .append(&TarEntry::file("dir/", 0o644, b""))
            .unwrap_err();

        assert_eq!(
            error,
            "archive/tar: cannot encode header: filename may not have trailing slash"
        );
    }
}
