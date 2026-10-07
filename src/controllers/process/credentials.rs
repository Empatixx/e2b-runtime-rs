use std::io;

use rustix::process::{Gid, Uid};
use rustix::thread::{set_thread_groups, set_thread_res_gid, set_thread_res_uid};

use crate::user::User;

pub fn with_user_credentials<T>(user: &User, spawn: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    let original = Credentials::current()?;
    let spawned = Credentials::of(user, &original).assume().and_then(|()| spawn());
    original.restore().unwrap_or_else(|e| {
        tracing::error!(error = %e, "failed to restore thread credentials");
        std::process::abort()
    });
    spawned
}

struct Ids<T> {
    real: T,
    effective: T,
    saved: T,
}

struct Credentials {
    uid: Ids<Uid>,
    gid: Ids<Gid>,
    groups: Vec<Gid>,
}

impl Credentials {
    fn current() -> io::Result<Self> {
        let uid = nix::unistd::getresuid()?;
        let gid = nix::unistd::getresgid()?;
        Ok(Self {
            uid: Ids {
                real: Uid::from_raw(uid.real.as_raw()),
                effective: Uid::from_raw(uid.effective.as_raw()),
                saved: Uid::from_raw(uid.saved.as_raw()),
            },
            gid: Ids {
                real: Gid::from_raw(gid.real.as_raw()),
                effective: Gid::from_raw(gid.effective.as_raw()),
                saved: Gid::from_raw(gid.saved.as_raw()),
            },
            groups: nix::unistd::getgroups()?
                .into_iter()
                .map(|group| Gid::from_raw(group.as_raw()))
                .collect(),
        })
    }

    fn of(user: &User, original: &Credentials) -> Self {
        let (uid, gid) = (Uid::from_raw(user.uid), Gid::from_raw(user.gid));
        Self {
            uid: Ids {
                real: uid,
                effective: uid,
                saved: original.uid.effective,
            },
            gid: Ids {
                real: gid,
                effective: gid,
                saved: original.gid.effective,
            },
            groups: std::iter::once(user.gid)
                .chain(user.groups.iter().copied())
                .map(Gid::from_raw)
                .collect(),
        }
    }

    fn assume(&self) -> io::Result<()> {
        set_thread_groups(&self.groups)?;
        set_thread_res_gid(self.gid.real, self.gid.effective, self.gid.saved)?;
        set_thread_res_uid(self.uid.real, self.uid.effective, self.uid.saved)?;
        Ok(())
    }

    fn restore(&self) -> io::Result<()> {
        set_thread_res_uid(self.uid.real, self.uid.effective, self.uid.saved)?;
        set_thread_res_gid(self.gid.real, self.gid.effective, self.gid.saved)?;
        set_thread_groups(&self.groups)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_runs_as_user_while_thread_keeps_its_credentials() {
        if !nix::unistd::geteuid().is_root() {
            return;
        }
        let before = (nix::unistd::getresuid().unwrap(), nix::unistd::getgroups().unwrap());
        let nobody = User::lookup("nobody").unwrap();
        let output = with_user_credentials(&nobody, || {
            std::process::Command::new("sh")
                .args(["-c", "id -u; id -g; id -G"])
                .output()
        })
        .unwrap();
        let expected_groups = std::iter::once(nobody.gid)
            .chain(nobody.groups.iter().copied())
            .map(|gid| gid.to_string())
            .collect::<Vec<_>>()
            .join(" ");

        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            format!("{}\n{}\n{expected_groups}\n", nobody.uid, nobody.gid)
        );
        assert_eq!(
            (nix::unistd::getresuid().unwrap(), nix::unistd::getgroups().unwrap()),
            before
        );
    }

    #[test]
    fn failed_switch_leaves_thread_unchanged() {
        if nix::unistd::geteuid().is_root() {
            return;
        }
        let before = nix::unistd::getresuid().unwrap();
        let root = User::lookup("root").unwrap();
        let error = with_user_credentials(&root, || std::process::Command::new("true").output()).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(nix::unistd::getresuid().unwrap(), before);
    }
}
