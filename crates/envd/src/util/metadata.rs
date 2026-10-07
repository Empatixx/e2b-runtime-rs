use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::path::Path;

use axum::http::HeaderMap;
use nix::errno::Errno;
use xattr::FileExt;

const XATTR_PREFIX: &str = "user.e2b.";
const HEADER_PREFIX: &str = "x-metadata-";
const MAX_KEY_LEN: usize = 255 - XATTR_PREFIX.len();
const MAX_TOTAL_LEN: usize = 4096;

pub fn read_metadata(path: &Path) -> BTreeMap<String, String> {
    xattr::list_deref(path)
        .map(|names| {
            names
                .filter_map(|name| {
                    let key = name.to_str()?.strip_prefix(XATTR_PREFIX)?.to_owned();
                    let value = xattr::get_deref(path, &name).ok().flatten()?;
                    Some((key, String::from_utf8(value).ok()?))
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn metadata_from_headers(headers: &HeaderMap) -> BTreeMap<String, String> {
    headers
        .keys()
        .filter_map(|name| {
            let key = name
                .as_str()
                .strip_prefix(HEADER_PREFIX)
                .filter(|key| !key.is_empty())?;
            let value = headers.get(name)?;
            Some((key.to_owned(), String::from_utf8_lossy(value.as_bytes()).into_owned()))
        })
        .collect()
}

pub fn validate_metadata(metadata: &BTreeMap<String, String>) -> Result<(), String> {
    let mut total = 0;
    for (key, value) in metadata {
        if key.is_empty() {
            return Err("metadata key must not be empty".to_owned());
        }
        if key.len() > MAX_KEY_LEN {
            return Err(format!("metadata key {key:?} exceeds {MAX_KEY_LEN} bytes"));
        }
        printable_ascii("key", key)?;
        printable_ascii(&format!("value for key {key:?}"), value)?;
        total += XATTR_PREFIX.len() + key.len() + value.len();
    }

    if total > MAX_TOTAL_LEN {
        return Err(format!("total metadata size {total} exceeds {MAX_TOTAL_LEN} bytes"));
    }
    Ok(())
}

fn printable_ascii(label: &str, text: &str) -> Result<(), String> {
    match text.bytes().position(|byte| !(0x20..=0x7E).contains(&byte)) {
        Some(offset) => Err(format!(
            "metadata {label} contains non-printable-ASCII byte 0x{:02x} at offset {offset}",
            text.as_bytes()[offset]
        )),
        None => Ok(()),
    }
}

pub fn write_metadata(file: &File, metadata: &BTreeMap<String, String>) -> io::Result<()> {
    file.list_xattr()?
        .filter(|name| {
            name.to_str()
                .and_then(|name| name.strip_prefix(XATTR_PREFIX))
                .is_some_and(|key| !metadata.contains_key(key))
        })
        .try_for_each(|name| match file.remove_xattr(&name) {
            Err(e) if e.raw_os_error() != Some(Errno::ENODATA as i32) => Err(e),
            _ => Ok(()),
        })?;

    metadata
        .iter()
        .try_for_each(|(key, value)| file.set_xattr(format!("{XATTR_PREFIX}{key}"), value.as_bytes()))
}

pub fn is_xattr_unsupported(err: &io::Error) -> bool {
    err.raw_os_error()
        .is_some_and(|code| code == Errno::ENOTSUP as i32 || code == Errno::EOPNOTSUPP as i32)
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn metadata(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn headers_become_lowercase_keys() {
        let mut headers = HeaderMap::new();
        headers.insert("X-Metadata-Owner", HeaderValue::from_static("team-a"));
        headers.insert("x-metadata-", HeaderValue::from_static("skipped"));
        headers.insert("content-type", HeaderValue::from_static("text/plain"));
        assert_eq!(metadata_from_headers(&headers), metadata(&[("owner", "team-a")]));
    }

    #[test]
    fn validation_messages() {
        assert_eq!(validate_metadata(&metadata(&[("a", "b")])), Ok(()));
        assert_eq!(
            validate_metadata(&metadata(&[(&"k".repeat(247), "v")])),
            Err(format!("metadata key \"{}\" exceeds 246 bytes", "k".repeat(247)))
        );
        assert_eq!(
            validate_metadata(&metadata(&[("a", "x\u{7f}")])),
            Err("metadata value for key \"a\" contains non-printable-ASCII byte 0x7f at offset 1".to_owned())
        );
        assert_eq!(
            validate_metadata(&metadata(&[("a", &"v".repeat(4096))])),
            Err("total metadata size 4106 exceeds 4096 bytes".to_owned())
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn write_replaces_previous_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f");
        let file = File::create(&path).unwrap();
        if write_metadata(&file, &metadata(&[("old", "1")])).is_err() {
            return;
        }
        write_metadata(&file, &metadata(&[("new", "2")])).unwrap();
        assert_eq!(read_metadata(&path), metadata(&[("new", "2")]));
    }
}
