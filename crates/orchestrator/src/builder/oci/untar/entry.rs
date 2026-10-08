use std::collections::BTreeMap;
use std::io::{self, Read};
use std::path::Path;

use nix::sys::stat::{UtimensatFlags, utimensat};
use nix::sys::time::TimeSpec;
use tar::{Entry, EntryType, Header};

const XATTR_PAX_PREFIX: &str = "SCHILY.xattr.";
const NANOS: i64 = 1_000_000_000;
const REGULAR_A: u8 = b'\0';

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Instant {
    pub seconds: i64,
    pub nanos: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Times {
    pub accessed: Instant,
    pub modified: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryMeta {
    pub kind: EntryType,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub device_major: u32,
    pub device_minor: u32,
    pub times: Times,
    pub pax: BTreeMap<String, Vec<u8>>,
}

impl EntryMeta {
    pub fn read<R: Read>(entry: &mut Entry<R>) -> io::Result<Self> {
        let pax = pax_records(entry)?;
        let header = entry.header();
        let raw = header.as_old();
        let modified = pax_time(&pax, "mtime").unwrap_or(Instant {
            seconds: numeric(header.mtime(), &raw.mtime)? as i64,
            nanos: 0,
        });
        let accessed = pax_time(&pax, "atime").or_else(|| gnu_atime(header));
        Ok(Self {
            kind: kind(header, &entry.path_bytes()),
            mode: numeric(header.mode().map(u64::from), &raw.mode)? as u32,
            uid: pax_number(&pax, "uid").map_or_else(|| numeric(header.uid(), &raw.uid), Ok)? as u32,
            gid: pax_number(&pax, "gid").map_or_else(|| numeric(header.gid(), &raw.gid), Ok)? as u32,
            device_major: header.device_major().ok().flatten().unwrap_or_default(),
            device_minor: header.device_minor().ok().flatten().unwrap_or_default(),
            times: Times {
                accessed: bound(accessed.map_or(modified, |accessed| latest(accessed, modified))),
                modified: bound(modified),
            },
            pax,
        })
    }

    pub fn apply_xattrs(&self, path: &Path) -> io::Result<()> {
        let ignored: Vec<String> = self
            .pax
            .iter()
            .filter_map(|(key, value)| key.strip_prefix(XATTR_PAX_PREFIX).map(|name| (name, value)))
            .map(|(name, value)| match xattr::set(path, name, value) {
                Err(e) if e.raw_os_error() == Some(nix::libc::EPERM) => Ok(Some(crate::go_os::reason(&e))),
                Err(e) => Err(e),
                Ok(()) => Ok(None),
            })
            .collect::<io::Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect();
        if !ignored.is_empty() {
            tracing::warn!(errors = ?ignored, "ignored xattrs in archive: underlying filesystem doesn't support them");
        }
        Ok(())
    }
}

pub fn set_times(path: &Path, times: Times, follow: bool) -> io::Result<()> {
    let flag = match follow {
        true => UtimensatFlags::FollowSymlink,
        false => UtimensatFlags::NoFollowSymlink,
    };
    utimensat(None, path, &spec(times.accessed), &spec(times.modified), flag).map_err(io::Error::from)
}

fn spec(instant: Instant) -> TimeSpec {
    TimeSpec::new(instant.seconds, instant.nanos)
}

fn kind(header: &Header, name: &[u8]) -> EntryType {
    match (header.as_old().linkflag[0], name.ends_with(b"/")) {
        (REGULAR_A, true) => EntryType::Directory,
        _ => header.entry_type(),
    }
}

fn pax_records<R: Read>(entry: &mut Entry<R>) -> io::Result<BTreeMap<String, Vec<u8>>> {
    let Some(extensions) = entry.pax_extensions()? else {
        return Ok(BTreeMap::new());
    };
    extensions
        .map(|extension| {
            let extension = extension?;
            let key = extension
                .key()
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?
                .to_owned();
            Ok((key, extension.value_bytes().to_vec()))
        })
        .collect()
}

fn pax_number(pax: &BTreeMap<String, Vec<u8>>, key: &str) -> Option<u64> {
    pax.get(key)
        .and_then(|value| std::str::from_utf8(value).ok())
        .and_then(|value| value.parse().ok())
}

fn pax_time(pax: &BTreeMap<String, Vec<u8>>, key: &str) -> Option<Instant> {
    let text = std::str::from_utf8(pax.get(key)?).ok()?;
    let (seconds, fraction) = text.split_once('.').unwrap_or((text, ""));
    let seconds: i64 = seconds.parse().ok()?;
    let digits: String = fraction.chars().take(9).collect();
    let nanos = match digits.is_empty() {
        true => 0,
        false => format!("{digits:0<9}").parse::<i64>().ok()?,
    };
    Some(match (seconds < 0 || text.starts_with('-'), nanos) {
        (true, nanos) if nanos > 0 => Instant {
            seconds: seconds - 1,
            nanos: NANOS - nanos,
        },
        _ => Instant { seconds, nanos },
    })
}

fn gnu_atime(header: &Header) -> Option<Instant> {
    header
        .as_gnu()
        .and_then(|gnu| gnu.atime().ok())
        .filter(|atime| *atime > 0)
        .map(|atime| Instant {
            seconds: atime as i64,
            nanos: 0,
        })
}

fn latest(first: Instant, second: Instant) -> Instant {
    match (first.seconds, first.nanos) < (second.seconds, second.nanos) {
        true => second,
        false => first,
    }
}

fn bound(instant: Instant) -> Instant {
    match instant.seconds < 0 {
        true => Instant { seconds: 0, nanos: 0 },
        false => instant,
    }
}

pub fn numeric(parsed: io::Result<u64>, field: &[u8]) -> io::Result<u64> {
    match parsed {
        Err(_) if field.iter().all(|byte| matches!(byte, 0 | b' ')) => Ok(0),
        parsed => parsed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn records(pairs: &[(&str, &str)]) -> BTreeMap<String, Vec<u8>> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.as_bytes().to_vec()))
            .collect()
    }

    #[test]
    fn pax_times_keep_nanoseconds_and_negatives_bound_to_the_epoch() {
        let pax = records(&[("mtime", "1700000000.123456789"), ("atime", "-5.5")]);

        assert_eq!(
            pax_time(&pax, "mtime"),
            Some(Instant {
                seconds: 1_700_000_000,
                nanos: 123_456_789
            })
        );
        let negative = pax_time(&pax, "atime").unwrap();
        assert_eq!(
            negative,
            Instant {
                seconds: -6,
                nanos: 500_000_000
            }
        );
        assert_eq!(bound(negative), Instant { seconds: 0, nanos: 0 });
        assert_eq!(pax_number(&records(&[("uid", "70000")]), "uid"), Some(70000));
    }
}
