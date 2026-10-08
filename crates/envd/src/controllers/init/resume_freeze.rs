use serde::Serialize;

use crate::app::AppState;
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

pub async fn unfreeze_user_cgroups(state: &AppState) {
    let freezer = state.freezer.clone();
    let thawed = tokio::spawn(async move { freezer.unfreeze().await }).await;
    if let Ok(Err(e)) = thawed {
        tracing::warn!(error = %e, "unfreeze workload cgroups");
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
