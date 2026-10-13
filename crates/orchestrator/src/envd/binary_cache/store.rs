use std::path::PathBuf;
use std::sync::Arc;

use super::error::{Kind, WarmError};
use super::scope::Scope;
use super::{Cache, Entry, MAX_ENTRIES, TEMP_PREFIX};

impl Cache {
    pub(crate) async fn store(&self, scope: &Scope, entry: Entry) -> Result<(), WarmError> {
        let current = match self.stat_source_bounded(scope, &entry.source_path).await {
            Ok(current) => current,
            Err(error) => {
                let _ = std::fs::remove_file(&entry.local_path);
                return Err(WarmError::other(format!(
                    "revalidate envd binary source {:?}: {error}",
                    entry.source_path.display()
                )));
            }
        };
        if current != entry.identity {
            let _ = std::fs::remove_file(&entry.local_path);
            return Err(WarmError::of(Kind::Superseded));
        }
        self.publish(entry);
        Ok(())
    }

    fn publish(&self, entry: Entry) {
        let mut state = self.state.lock();
        if let Some(previous) = state.entries.get(&entry.source_path)
            && previous.local_path != entry.local_path
        {
            let _ = std::fs::remove_file(&previous.local_path);
        }
        state.order.retain(|path| *path != entry.source_path);
        state.order.push(entry.source_path.clone());
        state.entries.insert(entry.source_path.clone(), Arc::new(entry));
        while state.order.len() > MAX_ENTRIES {
            let oldest = state.order.remove(0);
            if let Some(victim) = state.entries.remove(&oldest) {
                let _ = std::fs::remove_file(&victim.local_path);
            }
        }
        self.sweep_locked(&state);
    }

    fn sweep_locked(&self, state: &super::State) {
        let keep: std::collections::HashSet<PathBuf> = state
            .entries
            .values()
            .map(|entry| entry.local_path.clone())
            .chain(state.in_flight.keys().cloned())
            .collect();
        let Ok(names) = std::fs::read_dir(&self.dir) else {
            return;
        };
        names
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| !kind.is_dir()))
            .filter(|entry| !entry.file_name().to_string_lossy().starts_with(TEMP_PREFIX))
            .map(|entry| self.dir.join(entry.file_name()))
            .filter(|path| !keep.contains(path))
            .for_each(|path| drop(std::fs::remove_file(path)));
    }
}
