use std::collections::BTreeSet;
use std::io;
use std::process::{Command, Output};
use std::sync::Arc;
use std::thread::JoinHandle;

use parking_lot::{Mutex, MutexGuard};
use rustix::io::Errno;
use rustix::process::{Pid, WaitOptions, waitpid};

type Pids = Arc<Mutex<BTreeSet<i32>>>;

#[derive(Default)]
pub struct Registry {
    pids: Pids,
    adopted: Mutex<Vec<JoinHandle<()>>>,
}

pub struct Held<'a> {
    pub pids: Vec<i32>,
    _children_cannot_start: MutexGuard<'a, BTreeSet<i32>>,
}

impl Registry {
    pub async fn run(&self, command: Command) -> io::Result<Output> {
        let pids = self.pids.clone();
        tokio::task::spawn_blocking(move || run_tracked(&pids, command))
            .await
            .map_err(io::Error::other)?
    }

    pub fn adopt(&self, pid: i32) {
        if pid <= 0 {
            return;
        }
        self.pids.lock().insert(pid);
        let pids = self.pids.clone();
        let reaper = std::thread::spawn(move || {
            reap(pid);
            pids.lock().remove(&pid);
        });
        self.adopted.lock().push(reaper);
    }

    pub async fn wait_adopted(&self) {
        let reapers = std::mem::take(&mut *self.adopted.lock());
        let _ = tokio::task::spawn_blocking(move || reapers.into_iter().for_each(|reaper| drop(reaper.join()))).await;
    }

    pub fn export_hold(&self) -> Held<'_> {
        let guard = self.pids.lock();
        Held {
            pids: guard.iter().copied().collect(),
            _children_cannot_start: guard,
        }
    }
}

fn run_tracked(pids: &Pids, mut command: Command) -> io::Result<Output> {
    let child = {
        let mut tracked = pids.lock();
        let child = command.spawn()?;
        tracked.insert(child_pid(&child));
        child
    };
    let pid = child_pid(&child);
    let output = child.wait_with_output();
    pids.lock().remove(&pid);
    output
}

fn child_pid(child: &std::process::Child) -> i32 {
    child.id() as i32
}

pub fn reap(pid: i32) {
    if pid <= 0 {
        return;
    }
    let Some(pid) = Pid::from_raw(pid) else {
        return;
    };
    while matches!(waitpid(Some(pid), WaitOptions::empty()), Err(Errno::INTR)) {}
}

#[cfg(test)]
mod tests {
    use std::io::PipeWriter;
    use std::time::Duration;

    use super::*;

    async fn eventually(condition: impl Fn() -> bool) {
        for _ in 0..1000 {
            if condition() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("condition never held");
    }

    fn exported(registry: &Registry) -> Vec<i32> {
        registry.export_hold().pids
    }

    fn waiting_command() -> (Command, PipeWriter) {
        let (reader, writer) = std::io::pipe().unwrap();
        let mut command = Command::new("cat");
        command.stdin(reader);
        (command, writer)
    }

    fn orphan(command: &mut Command) -> i32 {
        let child = command.spawn().unwrap();
        let pid = child.id() as i32;
        std::mem::forget(child);
        pid
    }

    fn is_gone(pid: i32) -> bool {
        rustix::process::test_kill_process(Pid::from_raw(pid).unwrap()) == Err(Errno::SRCH)
    }

    #[tokio::test]
    async fn run_lists_the_child_while_it_lives() {
        let registry = Arc::new(Registry::default());
        let (command, writer) = waiting_command();
        let running = tokio::spawn({
            let registry = registry.clone();
            async move { registry.run(command).await }
        });

        eventually(|| exported(&registry).len() == 1).await;
        drop(writer);

        assert!(running.await.unwrap().unwrap().status.success());
        assert!(exported(&registry).is_empty());
    }

    #[tokio::test]
    async fn export_hold_keeps_children_from_starting() {
        let registry = Arc::new(Registry::default());
        let mark = tempfile::tempdir().unwrap().keep().join("started");
        let held = registry.export_hold();
        assert!(held.pids.is_empty());

        let mut touch = Command::new("touch");
        touch.arg(&mark);
        let running = tokio::spawn({
            let registry = registry.clone();
            async move { registry.run(touch).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!mark.exists(), "a child started while the export was held");

        drop(held);

        assert!(running.await.unwrap().unwrap().status.success());
        assert!(mark.exists());
        assert!(exported(&registry).is_empty());
    }

    #[tokio::test]
    async fn run_reports_the_childs_failure() {
        let registry = Registry::default();

        assert!(!registry.run(Command::new("false")).await.unwrap().status.success());
        let missing = registry.run(Command::new("/nonexistent/helper")).await.unwrap_err();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
        assert!(exported(&registry).is_empty());
    }

    #[tokio::test]
    async fn adopt_lists_the_child_until_reaped() {
        let (mut command, writer) = waiting_command();
        let pid = orphan(&mut command);
        let registry = Registry::default();
        registry.adopt(pid);

        assert_eq!(exported(&registry), vec![pid]);

        drop(writer);
        eventually(|| is_gone(pid)).await;
        eventually(|| exported(&registry).is_empty()).await;
    }

    #[tokio::test]
    async fn wait_adopted_returns_once_every_adopted_child_is_reaped() {
        let none = Registry::default();
        tokio::time::timeout(Duration::from_secs(5), none.wait_adopted())
            .await
            .expect("nothing adopted, so nothing to wait for");

        let registry = Arc::new(Registry::default());
        let (mut writers, pids): (Vec<_>, Vec<_>) = (0..2)
            .map(|_| {
                let (mut command, writer) = waiting_command();
                (writer, orphan(&mut command))
            })
            .unzip();
        pids.iter().for_each(|pid| registry.adopt(*pid));
        let waiting = tokio::spawn({
            let registry = registry.clone();
            async move { registry.wait_adopted().await }
        });

        drop(writers.remove(0));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiting.is_finished(), "returned while an adopted child still runs");

        drop(writers);
        tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn adopt_drops_a_pid_that_is_not_a_child() {
        let registry = Registry::default();
        registry.adopt(std::os::unix::process::parent_id() as i32);

        eventually(|| exported(&registry).is_empty()).await;
    }

    #[tokio::test]
    async fn reap_collects_the_handed_over_child() {
        let pid = orphan(&mut Command::new("true"));

        std::thread::spawn(move || reap(pid));

        eventually(|| is_gone(pid)).await;
    }

    #[tokio::test]
    async fn reap_refuses_a_process_group() {
        let (mut command, writer) = waiting_command();
        let mut child = command.spawn().unwrap();

        let group = -(child.id() as i32);
        let refused = tokio::task::spawn_blocking(move || reap(group));
        tokio::time::timeout(Duration::from_secs(5), refused)
            .await
            .expect("reap waited on a process group")
            .unwrap();

        drop(writer);
        assert!(child.wait().unwrap().success());
    }
}
