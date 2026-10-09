use std::collections::BTreeMap;

use tar::{Builder, EntryType, Header};

use super::{LayerFile, SYMLINKS};
use crate::go_gzip::gzip_best_speed;

const SYMLINK_MODE: u32 = 0o777;

pub fn files_layer(files: &BTreeMap<String, LayerFile>) -> Result<Vec<u8>, String> {
    files
        .iter()
        .try_fold(Builder::new(Vec::new()), |mut builder, (name, file)| {
            let mut header = Header::new_ustar();
            header.set_entry_type(EntryType::Regular);
            header.set_size(file.bytes.len() as u64);
            header.set_mode(file.mode);
            zero_owner_and_time(&mut header);
            set_name(&mut header, name)?;
            builder.append(&header, file.bytes.as_slice()).map(|()| builder)
        })
        .and_then(Builder::into_inner)
        .map_err(|e| format!("error creating layer from files: {e}"))
}

pub fn gzipped(tar: &[u8]) -> Vec<u8> {
    gzip_best_speed(tar)
}

fn zero_owner_and_time(header: &mut Header) {
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
}

fn set_name(header: &mut Header, name: &str) -> std::io::Result<()> {
    let raw = header
        .as_ustar_mut()
        .ok_or_else(|| std::io::Error::other("not a ustar header"))?;
    let field = raw
        .name
        .get_mut(..name.len())
        .ok_or_else(|| std::io::Error::other(format!("tar name too long: {name}")))?;
    field.copy_from_slice(name.as_bytes());
    header.set_cksum();
    Ok(())
}

pub fn symlink_layer() -> Result<Vec<u8>, String> {
    let mut links = SYMLINKS.to_vec();
    links.sort();
    links
        .iter()
        .try_fold(Builder::new(Vec::new()), |mut builder, (name, target)| {
            let mut header = Header::new_ustar();
            header.set_entry_type(EntryType::Symlink);
            header.set_size(0);
            header.set_mode(SYMLINK_MODE);
            zero_owner_and_time(&mut header);
            builder.append_link(&mut header, name, target).map(|()| builder)
        })
        .and_then(Builder::into_inner)
        .map_err(|e| format!("error creating layer from symlinks: {e}"))
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use flate2::read::GzDecoder;
    use tar::Archive;

    use super::*;

    #[test]
    fn layers_hold_sorted_entries_with_modes_and_links() {
        let files = BTreeMap::from([
            (
                "usr/bin/init".to_owned(),
                LayerFile {
                    bytes: b"bb".to_vec(),
                    mode: 0o755,
                },
            ),
            (
                "etc/hostname".to_owned(),
                LayerFile {
                    bytes: b"e2b.local".to_vec(),
                    mode: 0o644,
                },
            ),
            (
                "/usr/bin/envd".to_owned(),
                LayerFile {
                    bytes: b"e".to_vec(),
                    mode: 0o777,
                },
            ),
        ]);

        let tar = files_layer(&files).unwrap();
        let entries: Vec<(String, u32, u64)> = Archive::new(tar.as_slice())
            .entries()
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                let header = entry.header();
                (
                    entry.path().unwrap().display().to_string(),
                    header.mode().unwrap(),
                    header.size().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            entries,
            vec![
                ("/usr/bin/envd".to_owned(), 0o777, 1),
                ("etc/hostname".to_owned(), 0o644, 9),
                ("usr/bin/init".to_owned(), 0o755, 2)
            ]
        );

        let links = symlink_layer().unwrap();
        let mut archive = Archive::new(links.as_slice());
        let link = archive.entries().unwrap().next().unwrap().unwrap();
        assert_eq!(
            link.path().unwrap().display().to_string(),
            "etc/systemd/system/multi-user.target.wants/envd.service"
        );
        assert_eq!(
            link.link_name().unwrap().unwrap().display().to_string(),
            "/etc/systemd/system/envd.service"
        );
    }

    #[test]
    fn gzipped_layer_unpacks_to_the_tar_with_the_go_header() {
        let tar = symlink_layer().unwrap();
        let layer = gzipped(&tar);
        assert_eq!(layer[..10], [0x1f, 0x8b, 0x08, 0x00, 0, 0, 0, 0, 0x04, 0xff]);
        let mut unpacked = Vec::new();
        GzDecoder::new(layer.as_slice()).read_to_end(&mut unpacked).unwrap();
        assert_eq!(unpacked, tar);
    }
}
