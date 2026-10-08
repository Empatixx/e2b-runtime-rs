use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Support {
    Unknown,
    On,
    Off,
}

impl Support {
    fn from_raw(raw: u8) -> Self {
        match raw {
            1 => Support::On,
            2 => Support::Off,
            _ => Support::Unknown,
        }
    }
}

#[derive(Debug, Default)]
pub struct WriteProtect {
    state: AtomicU8,
}

impl WriteProtect {
    pub fn support(&self) -> Support {
        Support::from_raw(self.state.load(Ordering::Relaxed))
    }

    pub fn requested(&self) -> bool {
        self.support() != Support::Off
    }

    pub fn confirmed(&self) -> bool {
        self.support() == Support::On
    }

    pub fn confirm(&self) {
        let _ = self.state.compare_exchange(
            Support::Unknown as u8,
            Support::On as u8,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
    }

    pub fn disable(&self) {
        self.state.store(Support::Off as u8, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_trusted_before_a_protected_copy_succeeded() {
        let write_protect = WriteProtect::default();

        assert_eq!(write_protect.support(), Support::Unknown);
        assert!(write_protect.requested());
        assert!(!write_protect.confirmed());
    }

    #[test]
    fn a_successful_protected_copy_confirms_it() {
        let write_protect = WriteProtect::default();

        write_protect.confirm();

        assert_eq!(write_protect.support(), Support::On);
        assert!(write_protect.confirmed());
    }

    #[test]
    fn a_refused_protected_copy_turns_it_off_for_good() {
        let write_protect = WriteProtect::default();

        write_protect.disable();
        write_protect.confirm();

        assert_eq!(write_protect.support(), Support::Off);
        assert!(!write_protect.requested());
        assert!(!write_protect.confirmed());
    }
}
