use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use super::gate::{self, OLD_ENVD};
use super::phase::resolve_phase_result;
use super::{Decline, Upgrade, candidate, is_disabled, resolve};
use crate::envd::binary_cache::Cache;
use crate::envd::binary_cache::metrics::{Op, Outcome};
use crate::envd::binary_cache::probe::ExecProbe;
use crate::envd::binary_cache::resolver::{
    DirectSource, REASON_COPY_VANISHED, REASON_NOT_CACHED, Resolver, deferral_outcome, gated_reason, outcome,
    source_path,
};
use crate::envd::binary_cache::scope::Scope;
use crate::envd::report::EffectiveDefaults;
use crate::envd::version::{MIN_FOR_UPGRADE, is_at_least};
use crate::featureflags::{
    FeatureFlags, FlagContext, envd_binary_cache, envd_offline_upgrade_target, envd_upgrade_target,
};

const CACHE_DIR_NAME: &str = "envdbin";
const SOURCE_STALLED: &str = "source_stalled";

pub fn cache_dir(base_path: &Path) -> Option<PathBuf> {
    (!base_path.as_os_str().is_empty()).then(|| base_path.join(CACHE_DIR_NAME))
}

pub struct Subject {
    pub contexts: Vec<FlagContext>,
    pub from_version: String,
    pub sent_user: String,
    pub workdir_withheld: bool,
    pub reported_defaults: Option<EffectiveDefaults>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseSample {
    pub result: String,
    pub took: Duration,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Skipped {
    pub gated: Option<String>,
    pub warn: Option<String>,
    pub resolve: Option<PhaseSample>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Delivery {
    pub source: PathBuf,
    pub deliver_from: PathBuf,
    pub to_version: String,
    pub cached: bool,
    pub cache_outcome: Option<Outcome>,
    pub resolve: Option<PhaseSample>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Plan {
    Skip(Skipped),
    Deliver(Delivery),
}

impl Skipped {
    fn nothing() -> Plan {
        Plan::Skip(Self::default())
    }

    fn gated(reason: impl Into<String>) -> Plan {
        Plan::Skip(Self {
            gated: Some(reason.into()),
            ..Self::default()
        })
    }
}

pub struct OfflineTarget {
    pub path: Option<PathBuf>,
    pub to_version: String,
    pub reason: String,
    pub resolver: Option<Resolver>,
}

#[derive(Clone)]
pub struct EnvdUpgrades {
    host_envd: PathBuf,
    cache: Option<Arc<Cache>>,
    flags: FeatureFlags,
    stage_root: PathBuf,
}

impl EnvdUpgrades {
    pub fn new(host_envd: PathBuf, base_path: &Path, flags: FeatureFlags) -> Self {
        let cache =
            cache_dir(base_path).map(|dir| Arc::new(Cache::builder().dir(dir).probe(Arc::new(ExecProbe)).build()));
        Self {
            stage_root: base_path.to_path_buf(),
            ..Self::with_cache(host_envd, cache, flags)
        }
    }

    pub fn with_cache(host_envd: PathBuf, cache: Option<Arc<Cache>>, flags: FeatureFlags) -> Self {
        Self {
            host_envd,
            cache,
            flags,
            stage_root: PathBuf::new(),
        }
    }

    pub fn stage_root(&self) -> &Path {
        &self.stage_root
    }

    pub async fn offline_target(&self, contexts: &[FlagContext], from_version: &str) -> OfflineTarget {
        let target = self.flags.get(&envd_offline_upgrade_target(), contexts);
        let resolver = self.resolver_for(contexts, Op::Offline);
        match self.resolve(resolver.as_ref(), &target, from_version).await {
            Ok(upgrade) => OfflineTarget {
                path: Some(upgrade.path),
                to_version: upgrade.version,
                reason: String::new(),
                resolver,
            },
            Err(decline) => OfflineTarget {
                path: None,
                to_version: String::new(),
                reason: gated_reason(decline.reason(), deferral_outcome(resolver.as_ref())),
                resolver,
            },
        }
    }

    pub fn cache(&self) -> Option<&Arc<Cache>> {
        self.cache.as_ref()
    }

    pub fn start_warm(&self) {
        let upgrades = self.clone();
        tokio::spawn(async move { upgrades.warm().await });
    }

    pub async fn warm(&self) {
        let Some(cache) = self
            .cache
            .as_ref()
            .filter(|_| self.flags.get(&envd_binary_cache(), &[]))
        else {
            return;
        };
        for target in self.warm_targets().await {
            cache.warm_async(&Scope::background(), &target);
        }
    }

    pub async fn warm_targets(&self) -> Vec<PathBuf> {
        let mut targets = vec![self.host_envd.clone()];
        let source = DirectSource::new(None, Scope::background());
        for flag in [envd_upgrade_target(), envd_offline_upgrade_target()] {
            let target = self.flags.get(&flag, &[]);
            if matches!(target.as_str(), "" | "off" | "promoted") {
                continue;
            }
            if let Ok(candidate) = candidate(&source, &target, &self.host_envd).await
                && !targets.contains(&candidate)
            {
                targets.push(candidate);
            }
        }
        targets
    }

    pub async fn plan(&self, subject: &Subject) -> Plan {
        let target = self.flags.get(&envd_upgrade_target(), &subject.contexts);
        if is_disabled(&target) {
            return Skipped::nothing();
        }
        if is_at_least(&subject.from_version, MIN_FOR_UPGRADE) != Ok(true) {
            return Skipped::gated(OLD_ENVD);
        }
        let resolver = self.resolver(&subject.contexts);
        let started = Instant::now();
        let resolved = self.resolve(resolver.as_ref(), &target, &subject.from_version).await;
        let took = started.elapsed();
        let reason = resolved.as_ref().err().map_or_else(String::new, |decline| {
            gated_reason(decline.reason(), deferral_outcome(resolver.as_ref()))
        });
        let sample = resolve_phase_result(resolved.is_ok(), &reason).map(|result| PhaseSample { result, took });
        match resolved {
            Err(_) => Self::declined(reason, sample),
            Ok(upgrade) => self.deliverable(resolver.as_ref(), upgrade, subject, sample).await,
        }
    }

    fn resolver(&self, contexts: &[FlagContext]) -> Option<Resolver> {
        self.resolver_for(contexts, Op::Live)
    }

    fn resolver_for(&self, contexts: &[FlagContext], op: Op) -> Option<Resolver> {
        self.cache
            .clone()
            .filter(|_| self.flags.get(&envd_binary_cache(), contexts))
            .map(|cache| Resolver::new(cache, op))
    }

    async fn resolve(&self, resolver: Option<&Resolver>, target: &str, from: &str) -> Result<Upgrade, Decline> {
        match resolver {
            Some(resolver) => resolve(resolver, target, from, &self.host_envd).await,
            None => {
                let direct = DirectSource::new(self.cache.clone(), Scope::background());
                resolve(&direct, target, from, &self.host_envd).await
            }
        }
    }

    fn declined(reason: String, resolve: Option<PhaseSample>) -> Plan {
        let (gated, warn) = match reason.as_str() {
            "off" | "same_version" => (None, None),
            REASON_NOT_CACHED | SOURCE_STALLED => (Some(reason), None),
            _ => (Some(reason.clone()), Some(reason)),
        };
        Plan::Skip(Skipped { gated, warn, resolve })
    }

    async fn deliverable(
        &self,
        resolver: Option<&Resolver>,
        upgrade: Upgrade,
        subject: &Subject,
        resolve: Option<PhaseSample>,
    ) -> Plan {
        let preserves = EffectiveDefaults::preserves_exec_context(subject.reported_defaults.as_ref());
        if let Some(reason) = gate::decline_reason(&subject.sent_user, subject.workdir_withheld, preserves) {
            return Skipped::gated(reason);
        }
        match source_path(resolver, &upgrade.path).await {
            None => Plan::Skip(Skipped {
                gated: Some(REASON_COPY_VANISHED.to_owned()),
                warn: Some(REASON_COPY_VANISHED.to_owned()),
                resolve,
            }),
            Some(deliver_from) => Plan::Deliver(Delivery {
                source: upgrade.path,
                deliver_from,
                to_version: upgrade.version,
                cached: resolver.is_some(),
                cache_outcome: outcome(resolver),
                resolve,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::envd::binary_cache::testing::{Rig, background, eventually, mtime, ok_probe, write_raw, write_source};

    const WAIT: Duration = Duration::from_secs(30);

    fn flags(cache: bool, target: &str) -> FeatureFlags {
        FeatureFlags::builder()
            .override_flag("envd-binary-cache", cache)
            .override_flag("envd-upgrade-target", target)
            .offline()
    }

    fn subject(from_version: &str) -> Subject {
        Subject {
            contexts: Vec::new(),
            from_version: from_version.to_owned(),
            sent_user: "user".to_owned(),
            workdir_withheld: false,
            reported_defaults: None,
        }
    }

    fn upgrades(rig: &Rig, cache: bool, target: &str) -> EnvdUpgrades {
        EnvdUpgrades::with_cache(rig.src.clone(), Some(rig.cache.clone()), flags(cache, target))
    }

    async fn warmed_rig(version: &'static str) -> Rig {
        let rig = Rig::with(ok_probe(version));
        write_source(&rig.src, "binary", mtime(1_700_000_000));
        rig.cache.warm(&background(), &rig.src).await.unwrap();
        rig
    }

    fn skipped(plan: Plan) -> Skipped {
        match plan {
            Plan::Skip(skipped) => skipped,
            Plan::Deliver(delivery) => panic!("expected a skip, got {delivery:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_old_envd_is_rejected_before_the_target_is_resolved() {
        let rig = warmed_rig("9.9.9").await;

        let plan = skipped(upgrades(&rig, true, "promoted").plan(&subject("0.6.11")).await);

        assert_eq!(plan.gated.as_deref(), Some("old_envd"));
        assert_eq!(plan.resolve, None, "the version gate must return before the resolve");
        assert_eq!(plan.warn, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_disabled_target_costs_nothing_and_counts_nothing() {
        let rig = warmed_rig("9.9.9").await;

        let plan = skipped(upgrades(&rig, true, "off").plan(&subject("0.6.11")).await);

        assert_eq!(plan, Skipped::default());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_upgrade_defers_when_the_binary_is_not_cached() {
        let rig = Rig::with(ok_probe("0.9.0"));
        write_raw(&rig.src, b"#!/bin/sh\necho 0.9.0\n", mtime(1_700_000_000));

        let plan = skipped(upgrades(&rig, true, "promoted").plan(&subject("0.6.13")).await);

        assert_eq!(plan.gated.as_deref(), Some("binary_not_cached"));
        assert_eq!(
            plan.warn, None,
            "a cold cache is this node's own expected state, not a warning"
        );
        assert_eq!(
            plan.resolve.map(|sample| sample.result).as_deref(),
            Some("binary_not_cached")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn with_the_cache_flag_off_the_source_is_probed_itself() {
        let rig = Rig::with(ok_probe("9.9.9"));
        std::fs::write(&rig.src, "#!/bin/sh\necho 0.6.13\n").unwrap();
        std::fs::set_permissions(&rig.src, std::fs::Permissions::from_mode(0o755)).unwrap();

        let plan = skipped(upgrades(&rig, false, "promoted").plan(&subject("0.6.13")).await);

        assert_eq!(
            plan.gated, None,
            "flag off must probe the source directly and see the same version"
        );
        assert_eq!(
            plan.resolve.map(|sample| sample.result).as_deref(),
            Some("same_version")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_upgrade_defers_when_the_cached_copy_is_removed() {
        let rig = warmed_rig("9.9.9").await;
        let entry = rig.cache.lookup(&rig.src).await.unwrap();
        std::fs::remove_file(&entry.local_path).unwrap();

        let plan = skipped(upgrades(&rig, true, "promoted").plan(&subject("0.6.13")).await);

        assert_eq!(plan.gated.as_deref(), Some("binary_not_cached"));
        let republished = eventually(|| rig.cache.state_has_copy(&rig.src), WAIT).await;
        assert!(republished, "the deferral must re-warm the copy it found missing");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_cached_newer_binary_is_delivered_from_its_local_copy() {
        let rig = warmed_rig("9.9.9").await;

        let plan = upgrades(&rig, true, "promoted").plan(&subject("0.6.13")).await;

        let Plan::Deliver(delivery) = plan else {
            panic!("expected a delivery, got {plan:?}");
        };
        assert_eq!(delivery.to_version, "9.9.9");
        assert_eq!(delivery.source, rig.src);
        assert_ne!(
            delivery.deliver_from, rig.src,
            "the bytes come from the local copy, not the mount"
        );
        assert!(delivery.cached);
        assert_eq!(delivery.cache_outcome, Some(Outcome::Hit));
        assert_eq!(delivery.resolve.map(|sample| sample.result).as_deref(), Some("success"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_swap_without_a_user_to_re_send_is_declined_after_the_resolve() {
        let rig = warmed_rig("9.9.9").await;
        let mut silent = subject("0.6.13");
        silent.sent_user = String::new();

        let plan = skipped(upgrades(&rig, true, "promoted").plan(&silent).await);

        assert_eq!(plan.gated.as_deref(), Some("no_default_user"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_envd_that_carries_its_own_exec_context_is_not_declined() {
        let rig = warmed_rig("9.9.9").await;
        let mut carrying = subject("0.6.13");
        carrying.sent_user = String::new();
        carrying.workdir_withheld = true;
        carrying.reported_defaults = Some(EffectiveDefaults {
            user: "user".to_owned(),
            workdir: None,
            fallback: false,
        });

        let plan = upgrades(&rig, true, "promoted").plan(&carrying).await;

        assert!(matches!(plan, Plan::Deliver(_)), "{plan:?}");
    }

    #[test]
    fn the_cache_directory_is_empty_without_a_base() {
        assert_eq!(cache_dir(Path::new("")), None);
        assert_eq!(
            cache_dir(Path::new("/orchestrator")),
            Some(PathBuf::from("/orchestrator/envdbin"))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_startup_warm_primes_the_promoted_binary() {
        let rig = Rig::with(ok_probe("9.9.9"));
        write_source(&rig.src, "binary", mtime(1_700_000_000));

        upgrades(&rig, true, "off").warm().await;

        assert!(eventually(|| rig.cache.state_has_copy(&rig.src), WAIT).await);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_startup_warm_leaves_the_cache_empty_with_the_flag_off() {
        let rig = Rig::with(ok_probe("9.9.9"));
        write_source(&rig.src, "binary", mtime(1_700_000_000));

        upgrades(&rig, false, "promoted").warm().await;

        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!rig.cache.state_has_copy(&rig.src));
        assert!(!rig.cache_dir().exists(), "flag off must not leave a directory behind");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_startup_warm_primes_a_pinned_target_beside_the_promoted_binary() {
        let rig = Rig::with(ok_probe("9.9.10"));
        let staged = rig.dir.join("envd.9f9f9f9");
        write_source(&rig.src, "promoted", mtime(1_700_000_000));
        write_source(&staged, "staged", mtime(1_700_000_001));

        upgrades(&rig, true, "9f9f9f9").warm().await;

        let both = eventually(
            || rig.cache.state_has_copy(&rig.src) && rig.cache.state_has_copy(&staged),
            WAIT,
        )
        .await;
        assert!(
            both,
            "the startup warm must prime the pinned target as well as the promoted binary"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn warm_targets_are_the_promoted_path_plus_what_a_pinned_flag_names() {
        let rig = Rig::with(ok_probe("9.9.9"));
        let staged = rig.dir.join("envd.abc123");
        write_source(&staged, "staged", mtime(1_700_000_001));

        let pinned = upgrades(&rig, true, "abc123").warm_targets().await;
        let promoted = upgrades(&rig, true, "promoted").warm_targets().await;
        let unstaged = upgrades(&rig, true, "feedbee").warm_targets().await;

        assert_eq!(pinned, vec![rig.src.clone(), staged]);
        assert_eq!(promoted, vec![rig.src.clone()]);
        assert_eq!(unstaged, vec![rig.src.clone()]);
    }
}
