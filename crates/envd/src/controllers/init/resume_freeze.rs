use std::sync::Arc;

use serde::Serialize;

use super::presence::Presence;
use crate::app::AppState;
use crate::cgroups::freeze::Thaw;
use crate::cgroups::hierarchy::AuditResult;

pub const FREEZE_AUDIT_HEADER: &str = "X-Envd-Freeze-Audit";

#[derive(Serialize)]
struct FreezeAudit {
    visited: usize,
    frozen: usize,
    escaped: usize,
    violations: usize,
    truncated: bool,
}

pub fn audit_header(state: &AppState) -> Option<String> {
    let audit = match state.freezer.audit()? {
        Ok(audit) => audit,
        Err(e) => {
            tracing::warn!(error = %e, "could not audit the frozen cgroup set at resume");
            return None;
        }
    };
    if !audit.applicable {
        return None;
    }
    log_audit(&audit);
    serde_json::to_string(&FreezeAudit {
        visited: audit.visited,
        frozen: audit.frozen,
        escaped: audit.escaped,
        violations: audit.violations,
        truncated: audit.truncated,
    })
    .ok()
}

pub async fn thaw_workload(state: &AppState, entry_generation: u64, presence: Arc<dyn Presence>) {
    let freezer = state.freezer.clone();
    let thawed = tokio::spawn(async move {
        let unobserved = freezer.clone();
        freezer
            .thaw_if_generation(entry_generation, move || {
                if presence.gone() {
                    unobserved.add_unobserved_init();
                }
            })
            .await
    })
    .await;
    match thawed {
        Ok(Thaw::Withheld { current }) => tracing::warn!(
            entry_freeze_generation = entry_generation,
            current_freeze_generation = current,
            "a freeze came after this /init entered; not thawing the workload"
        ),
        Ok(Thaw::Done { outcome: Err(e), .. }) => tracing::warn!(error = %e, "unfreeze workload cgroups"),
        Ok(Thaw::Done { outcome: Ok(_), .. }) => {}
        Err(e) => tracing::warn!(error = %e, "unfreeze workload cgroups"),
    }
}

fn log_audit(audit: &AuditResult) {
    if audit.truncated {
        tracing::warn!(
            visited = audit.visited,
            "the resume audit stopped at its bound; the rest of the hierarchy was not classified"
        );
    }
    if audit.violations > 0 {
        tracing::error!(
            violations = audit.violations,
            frozen = audit.frozen,
            visited = audit.visited,
            paths = %audit.violation_paths.join(" "),
            "cgroups the resume depends on were frozen; the allowlist did not hold"
        );
    }
    if audit.escaped > 0 {
        tracing::warn!(
            escaped = audit.escaped,
            frozen = audit.frozen,
            paths = %audit.escaped_paths.join(" "),
            "cgroups were running at resume that the pre-pause sweep should have stopped; created after it, or missed by a truncated or failed sweep"
        );
    }
}
