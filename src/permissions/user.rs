//! Linux user lookup (upstream uses Go's `os/user`).

use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserInfo {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
    /// Supplementary group ids (primary gid not included).
    pub groups: Vec<u32>,
}

pub fn lookup(name: &str) -> Result<UserInfo, String> {
    let user = nix::unistd::User::from_name(name)
        .map_err(|e| format!("lookup user '{name}': {e}"))?
        .ok_or_else(|| format!("unknown user {name}"))?;
    Ok(UserInfo {
        name: user.name.clone(),
        uid: user.uid.as_raw(),
        gid: user.gid.as_raw(),
        home: user.dir.clone(),
        groups: supplementary_groups(&user),
    })
}

#[cfg(target_os = "linux")]
fn supplementary_groups(user: &nix::unistd::User) -> Vec<u32> {
    let Ok(name) = std::ffi::CString::new(user.name.as_str()) else {
        return Vec::new();
    };
    match nix::unistd::getgrouplist(&name, user.gid) {
        Ok(gids) => gids.into_iter().map(|g| g.as_raw()).filter(|g| *g != user.gid.as_raw()).collect(),
        Err(e) => {
            tracing::warn!(user = %user.name, error = %e, "failed to get supplementary groups");
            Vec::new()
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn supplementary_groups(_user: &nix::unistd::User) -> Vec<u32> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_exists() {
        let root = lookup("root").unwrap();
        assert_eq!(root.uid, 0);
    }

    #[test]
    fn unknown_user_is_error() {
        assert!(lookup("definitely-missing-user").is_err());
    }
}
