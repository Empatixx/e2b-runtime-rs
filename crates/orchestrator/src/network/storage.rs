use std::collections::BTreeSet;
use std::ops::RangeInclusive;

use parking_lot::Mutex;

pub type NamespaceCheck = Box<dyn Fn(u32) -> bool + Send + Sync>;

pub struct SlotStorage {
    indices: RangeInclusive<u32>,
    namespace_exists: NamespaceCheck,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    taken: BTreeSet<u32>,
    leaked: BTreeSet<u32>,
}

impl SlotStorage {
    pub fn new(indices: RangeInclusive<u32>, namespace_exists: NamespaceCheck) -> Self {
        Self {
            indices,
            namespace_exists,
            state: Mutex::default(),
        }
    }

    pub fn acquire(&self) -> Result<u32, String> {
        let mut state = self.state.lock();
        let index = self
            .indices
            .clone()
            .find(|index| {
                !state.taken.contains(index) && !state.leaked.contains(index) && !(self.namespace_exists)(*index)
            })
            .or_else(|| state.leaked.pop_first())
            .ok_or("no free network slot")?;
        state.taken.insert(index);
        Ok(index)
    }

    pub fn release(&self, index: u32) {
        let mut state = self.state.lock();
        state.taken.remove(&index);
        if (self.namespace_exists)(index) {
            state.leaked.insert(index);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn storage(indices: RangeInclusive<u32>, existing: &[u32]) -> (SlotStorage, Arc<Mutex<BTreeSet<u32>>>) {
        let namespaces = Arc::new(Mutex::new(existing.iter().copied().collect::<BTreeSet<u32>>()));
        let check = namespaces.clone();
        (
            SlotStorage::new(indices, Box::new(move |index| check.lock().contains(&index))),
            namespaces,
        )
    }

    #[test]
    fn indices_are_unique_and_skip_existing_namespaces() {
        let (storage, _) = storage(1..=3, &[2]);

        assert_eq!(storage.acquire(), Ok(1));
        assert_eq!(storage.acquire(), Ok(3));
        assert_eq!(storage.acquire(), Err("no free network slot".to_owned()));

        storage.release(1);
        assert_eq!(storage.acquire(), Ok(1));
    }

    #[test]
    fn a_leaked_index_is_handed_out_only_when_nothing_else_is_free() {
        let (storage, namespaces) = storage(1..=2, &[]);
        assert_eq!(storage.acquire(), Ok(1));
        namespaces.lock().insert(1);
        storage.release(1);

        assert_eq!(storage.acquire(), Ok(2));
        assert_eq!(storage.acquire(), Ok(1));
        assert_eq!(storage.acquire(), Err("no free network slot".to_owned()));
    }
}
