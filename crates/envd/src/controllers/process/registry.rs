use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use connectrpc::ConnectError;
use moka::Expiry;
use moka::sync::Cache;
use parking_lot::Mutex;

use super::spawn::Process;
use crate::proto::process::__buffa::oneof::process_selector::Selector;
use crate::proto::process::ProcessSelector;
use crate::proto::process::process_event::EndEvent;

const RETENTION: Duration = Duration::from_secs(30);

pub struct Processes {
    running: Mutex<HashMap<u32, Arc<Process>>>,
    exited: Cache<u32, Exited>,
}

#[derive(Clone)]
pub struct Exited {
    pub pid: u32,
    pub tag: Option<String>,
    pub end: EndEvent,
    pub expires: Instant,
}

struct UntilExpires;

impl Expiry<u32, Exited> for UntilExpires {
    fn expire_after_create(&self, _: &u32, exited: &Exited, created_at: Instant) -> Option<Duration> {
        Some(exited.expires.saturating_duration_since(created_at))
    }

    fn expire_after_update(
        &self,
        _: &u32,
        exited: &Exited,
        updated_at: Instant,
        _: Option<Duration>,
    ) -> Option<Duration> {
        Some(exited.expires.saturating_duration_since(updated_at))
    }
}

impl Default for Processes {
    fn default() -> Self {
        Self {
            running: Mutex::default(),
            exited: Cache::builder().expire_after(UntilExpires).build(),
        }
    }
}

impl Processes {
    pub fn add(&self, process: Arc<Process>) {
        self.forget(process.pid, process.tag.as_deref());
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
        self.exited.insert(
            process.pid,
            Exited {
                pid: process.pid,
                tag: process.tag.clone(),
                end,
                expires: Instant::now() + RETENTION,
            },
        );
    }

    pub fn retained(&self) -> Vec<Exited> {
        self.exited.iter().map(|(_, exited)| exited).collect()
    }

    pub fn restore(&self, retained: Vec<Exited>) {
        let running = self.running.lock();
        retained
            .into_iter()
            .filter(|exited| !running.contains_key(&exited.pid))
            .for_each(|exited| self.exited.insert(exited.pid, exited));
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
        match selector.and_then(|selector| selector.selector.as_ref()) {
            Some(Selector::Pid(pid)) => self.exited.get(pid),
            Some(Selector::Tag(tag)) => self.exited_with_tag(tag).next(),
            None => None,
        }
    }

    fn exited_with_tag(&self, tag: &str) -> impl Iterator<Item = Exited> {
        self.exited
            .iter()
            .map(|(_, exited)| exited)
            .filter(move |exited| exited.tag.as_deref() == Some(tag))
    }

    fn forget(&self, pid: u32, tag: Option<&str>) {
        self.exited.invalidate(&pid);
        tag.into_iter()
            .flat_map(|tag| self.exited_with_tag(tag))
            .for_each(|exited| self.exited.invalidate(&exited.pid));
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
    fn forget_drops_pid_and_every_entry_with_tag() {
        let expires = Instant::now() + RETENTION;
        let processes = Processes::default();
        processes.restore(vec![
            exited(1, Some("web"), expires),
            exited(2, Some("web"), expires),
            exited(3, Some("db"), expires),
            exited(4, None, expires),
        ]);

        processes.forget(4, Some("web"));

        let mut left: Vec<u32> = processes.retained().iter().map(|exited| exited.pid).collect();
        left.sort();
        assert_eq!(left, [3]);
    }
}
