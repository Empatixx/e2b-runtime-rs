use std::collections::HashMap;
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
    exited: Mutex<HashMap<u32, Exited>>,
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
        self.exited
            .lock()
            .retain(|pid, exited| *pid != process.pid && (process.tag.is_none() || exited.tag != process.tag));
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
        exited.retain(|_, exited| exited.expires > now);
        exited.insert(
            process.pid,
            Exited {
                pid: process.pid,
                tag: process.tag.clone(),
                end,
                expires: now + RETENTION,
            },
        );
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
        let mut alive = exited.values().filter(|exited| exited.expires > now);
        match selector.and_then(|selector| selector.selector.as_ref()) {
            Some(Selector::Pid(pid)) => alive.find(|exited| exited.pid == *pid).cloned(),
            Some(Selector::Tag(tag)) => alive.find(|exited| exited.tag.as_deref() == Some(tag)).cloned(),
            None => None,
        }
    }
}
