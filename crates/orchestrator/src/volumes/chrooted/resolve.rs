use std::collections::VecDeque;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use rustix::fs::{AtFlags, FileType, Mode, OFlags, openat, readlinkat, statat};
use rustix::io::Errno;

use super::error::PathError;
use super::metrics::count_resolve_loop;
use super::names::{kernel_path, split_path};
use super::syscalls::retry_interrupted;
use crate::go_path;

const MAX_SYMLINK_HOPS: usize = 40;
pub(super) const MAX_RESOLVE_STEPS: usize = 4096;
const OPERATION: &str = "resolve";

pub fn resolve_path(root: BorrowedFd<'_>, name: &str, follow_final: bool) -> Result<String, PathError> {
    let mut walk = Walk {
        root,
        opened: None,
        resolved: "/".to_owned(),
        remaining: split_path(&kernel_path(name)).into(),
        hops: 0,
        follow_final,
    };
    for _ in 0..MAX_RESOLVE_STEPS {
        let Some(component) = walk.remaining.pop_front() else {
            return Ok(walk.resolved);
        };
        if let Some(finished) = walk.advance(&component)? {
            return Ok(finished);
        }
    }
    match walk.remaining.is_empty() {
        true => Ok(walk.resolved),
        false => Err(failed(&kernel_path(name))(Errno::NAMETOOLONG)),
    }
}

struct Walk<'a> {
    root: BorrowedFd<'a>,
    opened: Option<OwnedFd>,
    resolved: String,
    remaining: VecDeque<String>,
    hops: usize,
    follow_final: bool,
}

impl Walk<'_> {
    fn current(&self) -> BorrowedFd<'_> {
        self.opened.as_ref().map_or(self.root, |opened| opened.as_fd())
    }

    fn advance(&mut self, component: &str) -> Result<Option<String>, PathError> {
        match component {
            "" | "." => Ok(None),
            ".." => self.ascend().map(|()| None),
            _ => self.descend(component),
        }
    }

    fn ascend(&mut self) -> Result<(), PathError> {
        if self.resolved == "/" {
            return Ok(());
        }
        let parent = self
            .open_directory("..", OFlags::empty())
            .map_err(failed(&self.resolved))?;
        self.opened = Some(parent);
        self.resolved = go_path::dir(&self.resolved);
        Ok(())
    }

    fn descend(&mut self, component: &str) -> Result<Option<String>, PathError> {
        let candidate = go_path::join(&[&self.resolved, component]);
        let stat = retry_interrupted(|| statat(self.current(), component, AtFlags::SYMLINK_NOFOLLOW))
            .map_err(failed(&candidate))?;
        let last = self.remaining.is_empty();
        match FileType::from_raw_mode(stat.st_mode) {
            FileType::Symlink if last && !self.follow_final => Ok(Some(candidate)),
            FileType::Symlink => self.follow(component, &candidate).map(|()| None),
            FileType::Directory if last => Ok(Some(candidate)),
            FileType::Directory => self.enter(component, candidate).map(|()| None),
            _ if last => Ok(Some(candidate)),
            _ => Err(failed(&candidate)(Errno::NOTDIR)),
        }
    }

    fn follow(&mut self, component: &str, candidate: &str) -> Result<(), PathError> {
        self.hops += 1;
        if self.hops > MAX_SYMLINK_HOPS {
            count_resolve_loop();
            return Err(failed(candidate)(Errno::LOOP));
        }
        let target = readlinkat(self.current(), component, Vec::new())
            .map(|target| target.to_string_lossy().into_owned())
            .map_err(failed(candidate))?;
        if target.starts_with('/') {
            self.opened = None;
            self.resolved = "/".to_owned();
        }
        split_path(&target)
            .into_iter()
            .rev()
            .for_each(|part| self.remaining.push_front(part));
        Ok(())
    }

    fn enter(&mut self, component: &str, candidate: String) -> Result<(), PathError> {
        let next = self
            .open_directory(component, OFlags::NOFOLLOW)
            .map_err(failed(&candidate))?;
        self.opened = Some(next);
        self.resolved = candidate;
        Ok(())
    }

    fn open_directory(&self, name: &str, extra: OFlags) -> Result<OwnedFd, Errno> {
        retry_interrupted(|| {
            openat(
                self.current(),
                name,
                OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC | extra,
                Mode::empty(),
            )
        })
    }
}

fn failed(path: &str) -> impl Fn(Errno) -> PathError + '_ {
    move |errno| {
        PathError::builder()
            .operation(OPERATION)
            .path(path)
            .errno(errno)
            .build()
    }
}
