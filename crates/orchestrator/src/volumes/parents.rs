use super::chrooted::Chrooted;
use crate::go_path;
use crate::go_strconv::quote;
use crate::volumes::service::DEFAULT_DIR_MODE;

pub fn ensure_dirs(fs: &Chrooted, dir_path: &str, uid: u32, gid: u32) -> Result<(), String> {
    if dir_path.is_empty() {
        return Ok(());
    }
    let missing = missing_directories(fs, dir_path)?;
    if missing.is_empty() {
        return Ok(());
    }
    fs.mkdir_all(dir_path, DEFAULT_DIR_MODE)
        .map_err(|e| format!("failed to create parent directories: {e}"))?;
    missing
        .iter()
        .rev()
        .try_for_each(|created| own_created_directory(fs, created, uid, gid))
}

fn missing_directories(fs: &Chrooted, dir_path: &str) -> Result<Vec<String>, String> {
    let mut missing = Vec::new();
    let mut current = dir_path.to_owned();
    loop {
        match fs.stat(&current) {
            Ok(_) => return Ok(missing),
            Err(e) if e.is_not_found() => {}
            Err(e) => {
                return Err(format!("failed to stat parent directory {}: {e}", quote(&current)));
            }
        }
        missing.push(current.clone());
        let next = go_path::dir(&current);
        if next == current {
            return Ok(missing);
        }
        current = next;
    }
}

fn own_created_directory(fs: &Chrooted, path: &str, uid: u32, gid: u32) -> Result<(), String> {
    fs.chmod(path, DEFAULT_DIR_MODE)
        .map_err(|e| format!("failed chmod for created parent directory {}: {e}", quote(path)))?;
    fs.chown(path, Some(uid), Some(gid))
        .map_err(|e| format!("failed chown for created parent directory {}: {e}", quote(path)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::volumes::test_support::{TestVolume, assert_owner_and_mode};

    #[test]
    fn nothing_is_done_for_an_empty_path() {
        let Some(volume) = TestVolume::new() else { return };

        assert_eq!(ensure_dirs(&volume.fs(), "", 0o755, 1006), Ok(()));
    }

    #[test]
    fn a_single_missing_level_is_created_and_owned() {
        let Some(volume) = TestVolume::new() else { return };

        ensure_dirs(&volume.fs(), "/a", 1005, 1006).unwrap();

        assert_owner_and_mode(&volume.host("a"), 1005, 1006, DEFAULT_DIR_MODE);
    }

    #[test]
    fn every_missing_level_is_created_and_owned() {
        let Some(volume) = TestVolume::new() else { return };

        ensure_dirs(&volume.fs(), "/b/c/d", 800, 900).unwrap();

        for level in ["b", "b/c", "b/c/d"] {
            assert_owner_and_mode(&volume.host(level), 800, 900, DEFAULT_DIR_MODE);
        }
    }

    #[test]
    fn an_existing_directory_is_left_alone() {
        let Some(volume) = TestVolume::new() else { return };
        let fs = volume.fs();
        fs.mkdir("/e", 0o766).unwrap();
        fs.chmod("/e", 0o766).unwrap();
        fs.chown("/e", Some(1000), Some(1000)).unwrap();

        ensure_dirs(&fs, "/e", 2000, 2001).unwrap();

        assert_owner_and_mode(&volume.host("e"), 1000, 1000, 0o766);
    }

    #[test]
    fn only_the_directories_it_created_are_changed() {
        let Some(volume) = TestVolume::new() else { return };
        let fs = volume.fs();
        fs.mkdir_all("/q/f", 0o700).unwrap();
        fs.chmod("/q", 0o700).unwrap();
        fs.chmod("/q/f", 0o700).unwrap();

        ensure_dirs(&fs, "/q/f/g", 2020, 2021).unwrap();

        assert_owner_and_mode(&volume.host("q"), 0, 0, 0o700);
        assert_owner_and_mode(&volume.host("q/f"), 0, 0, 0o700);
        assert_owner_and_mode(&volume.host("q/f/g"), 2020, 2021, DEFAULT_DIR_MODE);
    }
}
