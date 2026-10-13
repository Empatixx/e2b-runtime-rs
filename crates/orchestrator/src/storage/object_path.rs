pub fn validate_object_path(path: &str) -> Result<(), String> {
    match path.split('/').all(|element| !matches!(element, "" | "." | "..")) {
        true => Ok(()),
        false => Err(format!("invalid object path: {path:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_unrooted_slash_separated_paths_without_dot_elements_are_valid() {
        for path in [
            "",
            ".",
            "/build/memfile",
            "../build/memfile",
            "build/../../memfile",
            "build/../memfile",
            "build/./memfile",
            "build//memfile",
            "build/",
        ] {
            assert_eq!(
                validate_object_path(path),
                Err(format!("invalid object path: {path:?}")),
                "{path:?}"
            );
        }
        for path in ["build", "build/memfile", "a/deep/two", "build/rootfs.ext4.header"] {
            assert_eq!(validate_object_path(path), Ok(()), "{path:?}");
        }
    }
}
