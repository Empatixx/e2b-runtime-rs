use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use connectrpc::ConnectError;
use parking_lot::Mutex;

use super::spawn::Process;
use crate::proto::process::__buffa::oneof::process_selector::Selector;
use crate::proto::process::ProcessSelector;
use crate::proto::process::process_event::EndEvent;

const RETENTION: Duration = Duration::from_secs(30);

#[derive(Default)]
pub struct Processes {
    running: Mutex<HashMap<u32, Arc<Process>>>,
    exited: Mutex<ExitedProcesses>,
}

#[derive(Default)]
struct ExitedProcesses {
    by_pid: HashMap<u32, Exited>,
    expiry_order: VecDeque<Expiry>,
}

struct Expiry {
    pid: u32,
    at: Instant,
}

#[derive(Clone)]
pub struct Exited {
    pub pid: u32,
    pub tag: Option<String>,
    pub end: EndEvent,
    pub expires: Instant,
}

impl Processes {
    pub fn add(&self, process: Arc<Process>) {
        self.exited.lock().forget(process.pid, process.tag.as_deref());
        self.running.lock().insert(process.pid, process);
    }

    pub fn finish(&self, process: &Arc<Process>, end: EndEvent) {
        let mut running = self.running.lock();
        if !running
            .get(&process.pid)
            .is_some_and(|current| Arc::ptr_eq(current, process))
        {
            return;
        }
        running.remove(&process.pid);

        let now = Instant::now();
        let mut exited = self.exited.lock();
        exited.evict_expired(now);
        exited.insert(Exited {
            pid: process.pid,
            tag: process.tag.clone(),
            end,
            expires: now + RETENTION,
        });
    }

    pub fn all(&self) -> Vec<Arc<Process>> {
        self.running.lock().values().cloned().collect()
    }

    pub fn find(&self, selector: Option<&ProcessSelector>) -> Result<Arc<Process>, ConnectError> {
        let running = self.running.lock();
        match selector.and_then(|selector| selector.selector.as_ref()) {
            Some(Selector::Pid(pid)) => running
                .get(pid)
                .cloned()
                .ok_or_else(|| ConnectError::not_found(format!("process with pid {pid} not found"))),
            Some(Selector::Tag(tag)) => running
                .values()
                .find(|process| process.tag.as_deref() == Some(tag))
                .cloned()
                .ok_or_else(|| ConnectError::not_found(format!("process with tag {tag} not found"))),
            None => Err(ConnectError::unimplemented(
                "invalid input type *process.ProcessSelector",
            )),
        }
    }

    pub fn find_exited(&self, selector: Option<&ProcessSelector>) -> Option<Exited> {
        let now = Instant::now();
        let exited = self.exited.lock();
        match selector.and_then(|selector| selector.selector.as_ref()) {
            Some(Selector::Pid(pid)) => exited.by_pid.get(pid).filter(|exited| exited.expires > now).cloned(),
            Some(Selector::Tag(tag)) => exited
                .by_pid
                .values()
                .find(|exited| exited.expires > now && exited.tag.as_deref() == Some(tag))
                .cloned(),
            None => None,
        }
    }
}

impl ExitedProcesses {
    fn insert(&mut self, exited: Exited) {
        self.expiry_order.push_back(Expiry {
            pid: exited.pid,
            at: exited.expires,
        });
        self.by_pid.insert(exited.pid, exited);
    }

    fn forget(&mut self, pid: u32, tag: Option<&str>) {
        self.by_pid.remove(&pid);
        if let Some(tag) = tag {
            self.by_pid.retain(|_, exited| exited.tag.as_deref() != Some(tag));
        }
    }

    fn evict_expired(&mut self, now: Instant) {
        while let Some(expiry) = self.expiry_order.pop_front_if(|expiry| expiry.at <= now) {
            if self
                .by_pid
                .get(&expiry.pid)
                .is_some_and(|exited| exited.expires == expiry.at)
            {
                self.by_pid.remove(&expiry.pid);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exited(pid: u32, tag: Option<&str>, expires: Instant) -> Exited {
        Exited {
            pid,
            tag: tag.map(str::to_owned),
            end: EndEvent::default(),
            expires,
        }
    }

    #[test]
    fn evicts_only_expired_entries_in_order() {
        let start = Instant::now();
        let mut processes = ExitedProcesses::default();
        processes.insert(exited(1, None, start));
        processes.insert(exited(2, None, start + RETENTION));

        processes.evict_expired(start);

        assert!(!processes.by_pid.contains_key(&1));
        assert!(processes.by_pid.contains_key(&2));
        assert_eq!(processes.expiry_order.len(), 1);
    }

    #[test]
    fn stale_expiry_keeps_entry_of_reused_pid() {
        let start = Instant::now();
        let mut processes = ExitedProcesses::default();
        processes.insert(exited(7, None, start));
        processes.insert(exited(7, None, start + RETENTION));

        processes.evict_expired(start);

        assert_eq!(processes.by_pid[&7].expires, start + RETENTION);
    }

    #[test]
    fn forget_drops_pid_and_every_entry_with_tag() {
        let expires = Instant::now() + RETENTION;
        let mut processes = ExitedProcesses::default();
        processes.insert(exited(1, Some("web"), expires));
        processes.insert(exited(2, Some("web"), expires));
        processes.insert(exited(3, Some("db"), expires));
        processes.insert(exited(4, None, expires));

        processes.forget(4, Some("web"));

        let mut left: Vec<u32> = processes.by_pid.keys().copied().collect();
        left.sort();
        assert_eq!(left, [3]);
    }
}
