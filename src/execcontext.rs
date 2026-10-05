//! Default user and working directory, set by `/init` (upstream `internal/execcontext`).

pub const BUILTIN_DEFAULT_USER: &str = "root";

/// Explicit workdir wins; otherwise the default from `/init`; otherwise "" (= user's home later).
pub fn resolve_default_workdir(workdir: &str, default: Option<&str>) -> String {
    if !workdir.is_empty() {
        return workdir.to_string();
    }
    default.unwrap_or("").to_string()
}

pub fn resolve_default_username(username: Option<&str>, default: &str) -> Result<String, String> {
    if let Some(u) = username {
        return Ok(u.to_string());
    }
    if !default.is_empty() {
        return Ok(default.to_string());
    }
    Err("username not provided".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_resolution() {
        assert_eq!(resolve_default_workdir("/a", Some("/b")), "/a");
        assert_eq!(resolve_default_workdir("", Some("/b")), "/b");
        assert_eq!(resolve_default_workdir("", None), "");
        assert_eq!(resolve_default_username(None, "root").unwrap(), "root");
        assert_eq!(resolve_default_username(Some("u"), "root").unwrap(), "u");
        assert!(resolve_default_username(None, "").is_err());
    }
}
