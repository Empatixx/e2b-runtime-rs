use std::time::Duration;

use crate::featureflags::{
    BUILD_CACHE_MAX_USAGE_PERCENTAGE, ENVD_INIT_REQUEST_TIMEOUT_MILLISECONDS, FeatureFlags, FlagContext, USE_MEMFD,
    envd_timeout_milliseconds,
};
use crate::server::services::{ServiceType, Services};

#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeFlags {
    flags: FeatureFlags,
    services: Services,
}

impl RuntimeFlags {
    pub fn new(flags: FeatureFlags, services: Services) -> Self {
        Self { flags, services }
    }

    pub fn envd_timeout(&self) -> Duration {
        millis(self.flags.get(&envd_timeout_milliseconds(), &[]))
    }

    pub fn envd_init_request_timeout(&self) -> Duration {
        millis(self.flags.get(&ENVD_INIT_REQUEST_TIMEOUT_MILLISECONDS, &[]))
    }

    pub fn use_memfd(&self, sandbox: FlagContext) -> bool {
        self.flags.get(&USE_MEMFD, &[sandbox])
    }

    pub fn eviction_threshold(&self) -> i64 {
        self.services
            .types
            .iter()
            .map(|service| {
                self.flags.get(
                    &BUILD_CACHE_MAX_USAGE_PERCENTAGE,
                    &[FlagContext::service(service_name(*service))],
                )
            })
            .min()
            .unwrap_or(BUILD_CACHE_MAX_USAGE_PERCENTAGE.fallback)
    }
}

fn millis(value: i64) -> Duration {
    Duration::from_millis(value.max(0) as u64)
}

fn service_name(service: ServiceType) -> &'static str {
    match service {
        ServiceType::Orchestrator => "orchestrator",
        ServiceType::TemplateManager => "template-manager",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime(services: &str, flags: FeatureFlags) -> RuntimeFlags {
        RuntimeFlags::new(flags, Services::parse(Some(services)))
    }

    #[test]
    fn the_defaults_are_the_upstream_flag_fallbacks() {
        let flags = runtime("orchestrator,template-manager", FeatureFlags::offline());

        assert_eq!(flags.envd_init_request_timeout(), Duration::from_millis(50));
        assert!(flags.use_memfd(FlagContext::sandbox("s")));
        assert_eq!(flags.eviction_threshold(), 85);
    }

    #[test]
    fn overrides_are_read_and_no_services_use_the_fallback_threshold() {
        let overridden = FeatureFlags::builder()
            .override_flag("envd-timeout-milliseconds", 2500)
            .override_flag("envd-init-request-timeout-milliseconds", 80)
            .override_flag("use-memfd", false)
            .override_flag("build-cache-max-usage-percentage", 70)
            .offline();
        let flags = runtime("orchestrator", overridden.clone());

        assert_eq!(flags.envd_timeout(), Duration::from_millis(2500));
        assert_eq!(flags.envd_init_request_timeout(), Duration::from_millis(80));
        assert!(!flags.use_memfd(FlagContext::sandbox("s")));
        assert_eq!(flags.eviction_threshold(), 70);
        assert_eq!(runtime("", overridden).eviction_threshold(), 85);
    }
}
