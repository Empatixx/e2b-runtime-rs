use std::collections::HashMap;

use super::Fault;

#[derive(Debug, Default)]
pub struct DeferredFaults {
    faults: Vec<Fault>,
    positions: HashMap<(u64, bool), usize>,
}

impl DeferredFaults {
    pub fn push(&mut self, fault: Fault) {
        let key = (fault.address, fault.write_protect);
        match self.positions.get(&key) {
            Some(&position) => self.faults[position].write |= fault.write,
            None => {
                self.positions.insert(key, self.faults.len());
                self.faults.push(fault);
            }
        }
    }

    pub fn extend(&mut self, faults: impl IntoIterator<Item = Fault>) {
        faults.into_iter().for_each(|fault| self.push(fault));
    }

    pub fn drain(&mut self) -> Vec<Fault> {
        self.positions.clear();
        std::mem::take(&mut self.faults)
    }

    pub fn is_empty(&self) -> bool {
        self.faults.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn missing(address: u64, write: bool) -> Fault {
        Fault {
            address,
            write,
            write_protect: false,
        }
    }

    fn protected(address: u64) -> Fault {
        Fault {
            address,
            write: true,
            write_protect: true,
        }
    }

    #[test]
    fn faults_are_deduped_by_address() {
        let mut deferred = DeferredFaults::default();
        deferred.push(missing(42, false));
        deferred.push(missing(42, false));
        deferred.push(missing(43, false));

        assert_eq!(deferred.drain().len(), 2);
        assert!(deferred.drain().is_empty());

        deferred.push(missing(42, false));
        assert_eq!(deferred.drain().len(), 1);
    }

    #[test]
    fn a_read_is_upgraded_to_a_write() {
        let mut deferred = DeferredFaults::default();
        deferred.push(missing(42, false));
        deferred.push(missing(42, true));

        let drained = deferred.drain();

        assert_eq!(drained.len(), 1);
        assert!(drained[0].write);
    }

    #[test]
    fn a_write_protect_fault_stays_distinct_from_a_missing_one() {
        let mut deferred = DeferredFaults::default();
        deferred.push(missing(42, false));
        deferred.push(protected(42));

        let drained = deferred.drain();

        assert_eq!(
            drained.len(),
            2,
            "WP and MISSING faults for the same page are distinct retries"
        );
        let write_protect = drained.iter().find(|fault| fault.write_protect);
        let plain = drained.iter().find(|fault| !fault.write_protect);
        assert!(write_protect.is_some(), "the WP fault must survive the dedup");
        assert!(
            plain.is_some_and(|fault| !fault.write),
            "the WP fault's WRITE flag must not leak into the MISSING entry"
        );

        deferred.push(protected(42));
        deferred.push(protected(42));
        deferred.push(missing(42, false));
        assert_eq!(deferred.drain().len(), 2);
    }
}
