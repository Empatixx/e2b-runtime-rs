use rustix::io::Errno;

use super::metrics::count_path_refused;
use crate::go_path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParentName {
    pub dir: String,
    pub base: String,
    pub trailing_slash: bool,
}

pub fn kernel_path(name: &str) -> String {
    match name.starts_with('/') {
        true => name.to_owned(),
        false => format!("/{name}"),
    }
}

pub fn display_path(name: &str) -> String {
    go_path::clean(&kernel_path(name))
}

pub fn split_parent(name: &str) -> ParentName {
    let name = kernel_path(name);
    let trailing_slash = name.len() > 1 && name.ends_with('/');
    let trimmed = name.trim_end_matches('/');
    match trimmed.rfind('/') {
        None => ParentName {
            dir: "/".to_owned(),
            base: ".".to_owned(),
            trailing_slash: false,
        },
        Some(0) => ParentName {
            dir: "/".to_owned(),
            base: trimmed[1..].to_owned(),
            trailing_slash,
        },
        Some(slash) => ParentName {
            dir: trimmed[..slash].to_owned(),
            base: trimmed[slash + 1..].to_owned(),
            trailing_slash,
        },
    }
}

pub fn split_path(path: &str) -> Vec<String> {
    path.trim_matches('/').split('/').map(str::to_owned).collect()
}

pub fn check_component(name: &[u8]) -> Result<(), Errno> {
    match name.is_empty() || name == b"." || name == b".." || name.contains(&b'/') {
        true => {
            count_path_refused();
            Err(Errno::INVAL)
        }
        false => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parent(dir: &str, base: &str, trailing_slash: bool) -> ParentName {
        ParentName {
            dir: dir.to_owned(),
            base: base.to_owned(),
            trailing_slash,
        }
    }

    #[test]
    fn paths_become_absolute_without_collapsing_anything() {
        assert_eq!(kernel_path("a/../b"), "/a/../b");
        assert_eq!(kernel_path("/a"), "/a");
        assert_eq!(display_path("dir//file"), "/dir/file");
    }

    #[test]
    fn the_root_has_itself_as_parent_and_dot_as_name() {
        assert_eq!(split_parent("/"), parent("/", ".", false));
        assert_eq!(split_parent(""), parent("/", ".", false));
        assert_eq!(split_parent("///"), parent("/", ".", false));
    }

    #[test]
    fn a_trailing_slash_is_remembered() {
        assert_eq!(split_parent("/a/b/"), parent("/a", "b", true));
        assert_eq!(split_parent("/a"), parent("/", "a", false));
        assert_eq!(split_parent("a/b/.."), parent("/a/b", "..", false));
    }

    #[test]
    fn only_a_single_entry_name_is_a_component() {
        assert_eq!(check_component(b"file"), Ok(()));
        for refused in [&b""[..], b".", b"..", b"a/b"] {
            assert_eq!(check_component(refused), Err(Errno::INVAL));
        }
    }

    #[test]
    fn paths_split_into_components_without_the_outer_slashes() {
        assert_eq!(split_path("/a//b/"), ["a", "", "b"]);
        assert_eq!(split_path("/"), [""]);
    }
}
