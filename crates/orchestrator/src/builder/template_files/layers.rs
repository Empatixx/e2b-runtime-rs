use std::collections::BTreeMap;

use tar::{Builder, EntryType, Header};

use super::{LayerFile, SYMLINKS};

const SYMLINK_MODE: u32 = 0o777;

pub fn files_layer(files: &BTreeMap<String, LayerFile>) -> Result<Vec<u8>, String> {
    files
        .iter()
        .try_fold(Builder::new(Vec::new()), |mut builder, (name, file)| {
            let mut header = Header::new_ustar();
            header.set_entry_type(EntryType::Regular);
            header.set_size(file.bytes.len() as u64);
            header.set_mode(file.mode);
            builder
                .append_data(&mut header, name, file.bytes.as_slice())
                .map(|()| builder)
        })
        .and_then(Builder::into_inner)
        .map_err(|e| format!("error creating layer from files: {e}"))
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
            builder.append_link(&mut header, name, target).map(|()| builder)
        })
        .and_then(Builder::into_inner)
        .map_err(|e| format!("error creating layer from symlinks: {e}"))
}

#[cfg(test)]
mod tests {
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
}
