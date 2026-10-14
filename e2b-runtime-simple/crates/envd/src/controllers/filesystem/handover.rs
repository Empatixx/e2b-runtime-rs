use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;

use super::watch::{self, FileWatcher, PendingEvents};
use crate::proto::upgrade::HandoverWatcher;

pub fn export_watchers(watchers: &HashMap<String, FileWatcher>) -> Vec<HandoverWatcher> {
    watchers
        .iter()
        .map(|(id, watcher)| HandoverWatcher {
            id: id.clone(),
            path: watcher.path.display().to_string(),
            recursive: watcher.recursive,
            include_entry_info: watcher.include_entry,
            pending_events: watcher.pending.lock().events.clone(),
            ..Default::default()
        })
        .collect()
}

pub fn import_watchers(watchers: &Mutex<HashMap<String, FileWatcher>>, carried: &[HandoverWatcher]) -> (usize, usize) {
    if carried.is_empty() {
        return (0, 0);
    }
    let mut registry = watchers.lock();
    let (rearmed, failed) = carried.iter().fold((0, 0), |(rearmed, failed), handed| {
        match rearm(handed) {
            Ok(watcher) => {
                tracing::info!(event_type = "watcher_readopted", watcher_id = %handed.id, pending_events = handed.pending_events.len(), "re-armed watcher after envd self-upgrade");
                registry.insert(handed.id.clone(), watcher);
                (rearmed + 1, failed)
            }
            Err(e) => {
                tracing::warn!(error = %e, watcher_id = %handed.id, "handover: re-arm watcher failed");
                (rearmed, failed + 1)
            }
        }
    });
    tracing::info!(
        event_type = "watchers_rearmed",
        rearmed,
        failed,
        "re-armed filesystem watchers after envd self-upgrade"
    );
    (rearmed, failed)
}

fn rearm(handed: &HandoverWatcher) -> Result<FileWatcher, String> {
    let path = PathBuf::from(&handed.path);
    let pending = Arc::new(Mutex::new(PendingEvents {
        events: handed.pending_events.clone(),
        error: None,
    }));
    let include_entry = handed.include_entry_info;
    let watcher = watch::watch(&path, handed.recursive, {
        let pending = pending.clone();
        let root = path.clone();
        move |result| watch::record_events(&pending, result, &root, include_entry)
    })
    .map_err(|e| e.to_string())?;
    Ok(FileWatcher {
        watcher,
        pending,
        path,
        recursive: handed.recursive,
        include_entry,
    })
}
