use std::sync::atomic::{AtomicBool, Ordering};

use crate::proto::PeerAvailability;

pub fn not_available() -> PeerAvailability {
    PeerAvailability {
        not_available: true,
        use_storage: false,
    }
}

pub fn use_storage() -> PeerAvailability {
    PeerAvailability {
        not_available: false,
        use_storage: true,
    }
}

pub fn served_by_peer(availability: Option<&PeerAvailability>, uploaded: &AtomicBool) -> bool {
    match availability {
        Some(availability) if availability.not_available => false,
        Some(availability) if availability.use_storage => {
            uploaded.store(true, Ordering::SeqCst);
            false
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_response_without_availability_is_served_by_the_peer() {
        let uploaded = AtomicBool::new(false);

        assert!(served_by_peer(None, &uploaded));
        assert!(served_by_peer(Some(&PeerAvailability::default()), &uploaded));
        assert!(!uploaded.load(Ordering::SeqCst));
    }

    #[test]
    fn not_available_falls_back_without_marking_the_build_uploaded() {
        let uploaded = AtomicBool::new(false);

        assert!(!served_by_peer(Some(&not_available()), &uploaded));
        assert!(!uploaded.load(Ordering::SeqCst));
    }

    #[test]
    fn use_storage_falls_back_and_marks_the_build_uploaded() {
        let uploaded = AtomicBool::new(false);

        assert!(!served_by_peer(Some(&use_storage()), &uploaded));
        assert!(uploaded.load(Ordering::SeqCst));
    }
}
