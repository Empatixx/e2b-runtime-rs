use std::time::Instant;

use super::wp_failures::ESCALATION;
use super::{Fault, Serving};
use crate::memory::page_tracker::PageState;
use crate::memory::uffd::{CopyMode, CopyOutcome};
use crate::telemetry::uffd::{WpOutcome, record_wp_promote, record_wp_resolve};

enum Presence {
    Unchecked,
    Verified,
}

enum Probe {
    Settled,
    Verified,
    Again,
    Failed(String),
}

impl Serving {
    pub(super) fn resolve_write_protect(&self, fault: Fault) -> Result<Option<Fault>, String> {
        let started = Instant::now();
        let mut outcome = WpOutcome::Resolved;
        let resolved = self.resolve(fault, &mut outcome);
        if outcome == WpOutcome::Deferred {
            self.state.counters.record_wp_deferred();
        }
        record_wp_resolve(
            self.state.balloon_mode(),
            self.state.generation,
            outcome,
            started.elapsed(),
        );
        resolved
    }

    fn resolve(&self, fault: Fault, outcome: &mut WpOutcome) -> Result<Option<Fault>, String> {
        let installing = self.state.installing();
        if installing.closed {
            *outcome = WpOutcome::Closed;
            return Ok(None);
        }
        let page_size = self.regions.page_size();
        let aligned = fault.address & !(page_size - 1);
        let offset = self.regions.offset_of(aligned)?;
        let index = (offset / page_size) as u32;

        let mut presence = Presence::Unchecked;
        let mut failure: Option<String> = None;
        if self.state.page_state(index) == PageState::Removed {
            match self.probe_presence(aligned, index, page_size, outcome)? {
                Probe::Settled => return Ok(None),
                Probe::Verified => presence = Presence::Verified,
                Probe::Again => {
                    *outcome = WpOutcome::Deferred;
                    return Ok(Some(fault));
                }
                Probe::Failed(error) => failure = Some(format!("stale-WP presence probe: {error}")),
            }
        }

        let mut captured = false;
        if failure.is_none() {
            if let Some(window) = self.state.cow_window() {
                captured = window.ensure_copied(index);
            }
            match self.uffd.get_ref().write_protect(aligned, page_size, false) {
                Ok(()) => {}
                Err(e) if is_again(&e) => {
                    *outcome = WpOutcome::Deferred;
                    return Ok(Some(fault));
                }
                Err(e) => failure = Some(e.to_string()),
            }
        }
        if let Some(error) = failure {
            return self.fail_unprotect(fault, aligned, error, outcome);
        }

        self.state.wp_failures.clear(aligned);
        let previous = match presence {
            Presence::Verified => self.state.tracker.mark_written_present(index),
            Presence::Unchecked => self.state.tracker.mark_written(index),
        };
        record_wp_promote(self.state.generation, previous);
        if captured {
            *outcome = WpOutcome::ResolvedCow;
        }
        self.state.counters.record_wp_resolved();
        Ok(None)
    }

    fn probe_presence(
        &self,
        aligned: u64,
        index: u32,
        page_size: u64,
        outcome: &mut WpOutcome,
    ) -> Result<Probe, String> {
        let zero = vec![0u8; page_size as usize];
        match self
            .uffd
            .get_ref()
            .copy_with(aligned, &zero, CopyMode::PROTECTED_WITHOUT_WAKE)
        {
            Ok(CopyOutcome::Copied) => {
                self.state.tracker.mark_installed(index..index + 1, PageState::Zero);
                *outcome = WpOutcome::StaleRemoved;
                self.uffd
                    .get_ref()
                    .wake(aligned, page_size)
                    .map_err(|wake| format!("uffd stale WP resolve at {aligned:#x} stranded the writer: {wake}"))?;
                Ok(Probe::Settled)
            }
            Ok(CopyOutcome::AlreadyPresent) => Ok(Probe::Verified),
            Ok(CopyOutcome::Retry) => Ok(Probe::Again),
            Ok(CopyOutcome::Gone | CopyOutcome::NoWriteProtect) => Ok(Probe::Settled),
            Err(e) => Ok(Probe::Failed(e.to_string())),
        }
    }

    fn fail_unprotect(
        &self,
        fault: Fault,
        aligned: u64,
        error: String,
        outcome: &mut WpOutcome,
    ) -> Result<Option<Fault>, String> {
        let woken = self.uffd.get_ref().wake(aligned, self.regions.page_size());
        *outcome = WpOutcome::Error;
        let streak = self.state.wp_failures.record(aligned);
        match woken {
            Ok(()) if streak < ESCALATION => {
                tracing::error!(
                    addr = fault.address,
                    consecutive_failures = streak,
                    error = %error,
                    "uffd WP resolve failed; writer woken to re-fault"
                );
                Ok(None)
            }
            Ok(()) => Err(format!(
                "uffd WP resolve of page {aligned:#x} failed {streak} consecutive times (deterministic failure, not retrying): {error}"
            )),
            Err(wake) => Err(format!(
                "uffd WP resolve at {:#x} stranded the writer: {error}; {wake}",
                fault.address
            )),
        }
    }
}

fn is_again(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(nix::libc::EAGAIN)
}
