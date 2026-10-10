pub fn clean(path: &str) -> String {
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

pub fn join(elements: &[&str]) -> String {
    let joined = elements
        .iter()
        .filter(|element| !element.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("/");
    match joined.is_empty() {
        true => String::new(),
        false => clean(&joined),
    }
}

pub fn dir(path: &str) -> String {
    clean(path.rfind('/').map_or("", |slash| &path[..=slash]))
}

pub fn base(path: &str) -> String {
    match path.trim_end_matches('/') {
        "" if path.is_empty() => ".".to_owned(),
        "" => "/".to_owned(),
        trimmed => trimmed.rsplit('/').next().unwrap_or(trimmed).to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_like_go_path_clean() {
        assert_eq!(clean(""), ".");
        assert_eq!(clean("//"), "/");
        assert_eq!(clean("/../../a/./b/.."), "/a");
        assert_eq!(clean("../a"), "../a");
        assert_eq!(clean("a/../.."), "..");
    }

    #[test]
    fn joins_and_splits_like_go() {
        assert_eq!(join(&["/", "a", "", "b/"]), "/a/b");
        assert_eq!(join(&["", ""]), "");
        assert_eq!(dir("/a/b"), "/a");
        assert_eq!(dir("/a"), "/");
        assert_eq!(dir("a"), ".");
        assert_eq!(dir("/a/b/"), "/a/b");
        assert_eq!(base("/a/b/"), "b");
        assert_eq!(base("/"), "/");
        assert_eq!(base(""), ".");
    }
}
