use std::collections::HashMap;
use std::path::PathBuf;

use nix::unistd::{Gid, Group, Uid};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
    pub groups: Vec<u32>,
}

impl User {
    pub fn lookup(name: &str) -> Option<Self> {
        let user = nix::unistd::User::from_name(name).ok().flatten()?;
        Some(Self {
            groups: supplementary_groups(&user),
            name: user.name,
            uid: user.uid.as_raw(),
            gid: user.gid.as_raw(),
            home: user.dir,
        })
    }
}

pub fn user_name(uid: u32) -> String {
    nix::unistd::User::from_uid(Uid::from_raw(uid))
        .ok()
        .flatten()
        .map_or_else(|| uid.to_string(), |u| u.name)
}

pub fn group_name(gid: u32) -> String {
    Group::from_gid(Gid::from_raw(gid))
        .ok()
        .flatten()
        .map_or_else(|| gid.to_string(), |g| g.name)
}

#[derive(Debug, Default)]
pub struct OwnerNames {
    users: HashMap<u32, String>,
    groups: HashMap<u32, String>,
}

impl OwnerNames {
    pub fn user(&mut self, uid: u32) -> String {
        self.users.entry(uid).or_insert_with(|| user_name(uid)).clone()
    }

    pub fn group(&mut self, gid: u32) -> String {
        self.groups.entry(gid).or_insert_with(|| group_name(gid)).clone()
    }
}

#[cfg(target_os = "linux")]
fn supplementary_groups(user: &nix::unistd::User) -> Vec<u32> {
    std::ffi::CString::new(user.name.as_str())
        .ok()
        .and_then(|name| {
            nix::unistd::getgrouplist(&name, user.gid)
                .inspect_err(|e| tracing::warn!(user = %user.name, error = %e, "failed to get supplementary groups"))
                .ok()
        })
        .map(|gids| gids.into_iter().filter(|g| *g != user.gid).map(Gid::as_raw).collect())
        .unwrap_or_default()
}

#[cfg(not(target_os = "linux"))]
fn supplementary_groups(_user: &nix::unistd::User) -> Vec<u32> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup() {
        assert_eq!(User::lookup("root").unwrap().uid, 0);
        assert_eq!(User::lookup("definitely-missing-user"), None);
    }

    #[test]
    fn names_fall_back_to_numbers() {
        assert_eq!(user_name(0), "root");
        assert_eq!(user_name(4_000_000), "4000000");
        assert_eq!(group_name(4_000_000), "4000000");
    }

    #[test]
    fn owner_names_match_direct_lookup() {
        let mut owners = OwnerNames::default();
        assert_eq!(owners.user(0), user_name(0));
        assert_eq!(owners.user(0), user_name(0));
        assert_eq!(owners.group(0), group_name(0));
        assert_eq!(owners.user(4_000_000), "4000000");
    }
}
