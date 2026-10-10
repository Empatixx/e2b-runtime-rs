use std::path::Path;
use std::time::Duration;

use super::Sandbox;
use crate::envd::report::{EffectiveDefaults, EnvdReport, HandoverResult};
use crate::envd::upgrade::{DeliveryFailure, call_envd_upgrade};
use crate::telemetry::envd_defaults::{record_applied, record_builtin_fallback, record_mismatch};

const DEFAULTS_SOURCE_METADATA: &str = "metadata";
const DEFAULTS_SOURCE_INDETERMINATE: &str = "indeterminate";

impl Sandbox {
    pub fn live_envd_version(&self) -> String {
        self.envd_report.lock().version.clone().unwrap_or_default()
    }

    pub fn handover_result(&self) -> Option<HandoverResult> {
        self.envd_report.lock().handover.clone()
    }

    pub fn reported_defaults(&self) -> Option<EffectiveDefaults> {
        self.envd_report.lock().defaults.clone()
    }

    pub fn workdir_withheld(&self) -> bool {
        self.config.workdir_withheld
    }

    pub fn sent_default_user(&self) -> String {
        self.config.envd.default_user.clone().unwrap_or_default()
    }

    pub fn built_with_envd_version(&self) -> &str {
        &self.config.envd_version
    }

    pub async fn call_envd_upgrade(
        &self,
        source: &Path,
        guest_path: &str,
        timeout: Duration,
    ) -> Result<bool, DeliveryFailure> {
        call_envd_upgrade(
            self.envd_address(),
            self.config.envd.access_token.as_deref(),
            source,
            guest_path,
            timeout,
        )
        .await
    }

    pub(super) fn record_envd_defaults(&self) {
        let source = match self.config.envd.default_user {
            Some(_) => DEFAULTS_SOURCE_METADATA,
            None => DEFAULTS_SOURCE_INDETERMINATE,
        };
        if !self.config.throwaway {
            record_applied(source, &self.config.sandbox_type, self.config.workdir_withheld);
        }
    }

    pub(super) fn keep_report(&self, report: EnvdReport) {
        report
            .defaults
            .iter()
            .for_each(|defaults| self.compare_defaults(defaults));
        let mut kept = self.envd_report.lock();
        kept.version = report.version.or_else(|| kept.version.take());
        kept.handover = report.handover.or_else(|| kept.handover.take());
        kept.defaults = report.defaults.or_else(|| kept.defaults.take());
    }

    fn compare_defaults(&self, effective: &EffectiveDefaults) {
        let sent_user = self.sent_default_user();
        let sent_workdir = self.config.envd.default_workdir.clone().unwrap_or_default();
        for field in effective.mismatches(&sent_user, &sent_workdir) {
            record_mismatch(field);
            tracing::error!(
                sandbox = %self.config.id,
                envd_version = %self.config.envd_version,
                field,
                sent_user = %sent_user,
                effective_user = %effective.user_for_display(),
                sent_workdir = %sent_workdir,
                effective_workdir = %effective.workdir_for_display(),
                envd_on_builtin_fallback = effective.fallback,
                "envd is serving different defaults than the ones sent"
            );
        }
        if effective.fallback && !self.config.throwaway {
            record_builtin_fallback(!sent_user.is_empty(), &self.config.sandbox_type);
            if !sent_user.is_empty() {
                tracing::error!(
                    sandbox = %self.config.id,
                    envd_version = %self.config.envd_version,
                    sent = %sent_user,
                    effective = %effective.user_for_display(),
                    "envd reports it was never told a default user, but one was sent"
                );
            }
        }
    }
}
