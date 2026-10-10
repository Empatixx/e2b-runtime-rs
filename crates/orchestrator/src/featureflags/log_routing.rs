use std::net::IpAddr;
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::Counter;
use parking_lot::Mutex;
use serde_json::Value;

use super::client::FeatureFlags;
use super::flags::{Flag, JsonFlag};
use crate::go_url::GoUrl;

pub const MODE_PRIMARY_ONLY: &str = "primary_only";
pub const MODE_PRIMARY_AND_SHADOW: &str = "primary_and_shadow";
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(2000);
pub const MAX_TIMEOUT: Duration = Duration::from_millis(10000);
pub const MAX_SHADOW_URLS: usize = 4;
pub const DEFAULT_MAX_INFLIGHT_SHADOW_WRITES: i64 = 1024;
const RESOLVER_TTL: Duration = Duration::from_secs(1);
const ALLOWED_HOST_SUFFIXES: [&str; 6] = [
    ".service.consul",
    ".consul",
    ".svc.cluster.local",
    ".svc",
    ".local",
    ".internal",
];

static RESOLUTIONS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    global::meter("github.com/e2b-dev/infra/packages/shared/pkg/featureflags")
        .u64_counter("log_write_config_resolution_count")
        .with_description("Number of logs-write-config resolutions by outcome and fallback reason")
        .build()
});

pub fn logs_write_config() -> JsonFlag {
    Flag::new("logs-write-config", Value::Null)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogWriteConfig {
    pub primary_url: String,
    pub shadow_urls: Vec<String>,
    pub timeout: Duration,
    pub max_inflight_shadow_writes: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogWriteDefaults {
    pub mode: String,
    pub primary_url: String,
    pub shadow_urls: Vec<String>,
    pub timeout: Duration,
    pub max_inflight_shadow_writes: i64,
}

impl LogWriteDefaults {
    pub fn from_env(primary_url: &str) -> Self {
        Self::from_vars(primary_url, &|name| std::env::var(name).ok())
    }

    pub fn from_vars(primary_url: &str, var: &dyn Fn(&str) -> Option<String>) -> Self {
        let text = |name: &str| var(name).unwrap_or_default().trim().to_owned();
        Self {
            mode: Some(text("LOGS_WRITE_MODE"))
                .filter(|mode| is_mode(mode))
                .unwrap_or_default(),
            primary_url: primary_url.trim().to_owned(),
            shadow_urls: text("LOGS_WRITE_SHADOW_URLS")
                .split(',')
                .map(str::trim)
                .filter(|url| !url.is_empty())
                .map(str::to_owned)
                .collect(),
            timeout: text("LOGS_WRITE_TIMEOUT_MS")
                .parse::<i64>()
                .ok()
                .filter(|millis| *millis > 0)
                .map_or(Duration::ZERO, capped_timeout),
            max_inflight_shadow_writes: text("LOGS_WRITE_MAX_INFLIGHT_SHADOW_WRITES")
                .parse::<i64>()
                .ok()
                .filter(|inflight| *inflight > 0)
                .unwrap_or(0),
        }
    }

    fn mode(&self) -> &str {
        match self.mode.is_empty() {
            true => MODE_PRIMARY_ONLY,
            false => &self.mode,
        }
    }

    fn max_inflight(&self) -> i64 {
        match self.max_inflight_shadow_writes <= 0 {
            true => DEFAULT_MAX_INFLIGHT_SHADOW_WRITES,
            false => self.max_inflight_shadow_writes,
        }
    }

    fn config(&self) -> LogWriteConfig {
        let primary = self.primary_url.trim().to_owned();
        LogWriteConfig {
            shadow_urls: match self.mode() == MODE_PRIMARY_AND_SHADOW {
                true => dedupe_shadow_urls(&primary, &self.shadow_urls),
                false => Vec::new(),
            },
            primary_url: primary,
            timeout: self.timeout,
            max_inflight_shadow_writes: self.max_inflight(),
        }
    }
}

fn is_mode(mode: &str) -> bool {
    mode == MODE_PRIMARY_ONLY || mode == MODE_PRIMARY_AND_SHADOW
}

fn capped_timeout(millis: i64) -> Duration {
    Duration::from_millis(u64::try_from(millis).unwrap_or(0).min(MAX_TIMEOUT.as_millis() as u64))
}

fn dedupe_shadow_urls(primary: &str, candidates: &[String]) -> Vec<String> {
    candidates
        .iter()
        .fold(vec![primary.to_owned()], |mut seen, url| {
            if !seen.contains(url) {
                seen.push(url.clone());
            }
            seen
        })
        .split_off(1)
}

pub fn resolve_log_write_config(flags: &FeatureFlags, defaults: &LogWriteDefaults) -> LogWriteConfig {
    let fallback = defaults.config();
    let value = flags.get(&logs_write_config(), &[]);
    match resolve(&value, defaults) {
        Ok((config, mode)) => {
            record("configured", "", &mode);
            config
        }
        Err((reason, mode)) => {
            record("legacy", reason, &mode);
            fallback
        }
    }
}

fn record(outcome: &str, reason: &str, mode: &str) {
    RESOLUTIONS.add(
        1,
        &[
            KeyValue::new("outcome", outcome.to_owned()),
            KeyValue::new("reason", reason.to_owned()),
            KeyValue::new("mode", mode.to_owned()),
        ],
    );
}

type Rejected = (&'static str, String);

fn resolve(value: &Value, defaults: &LogWriteDefaults) -> Result<(LogWriteConfig, String), Rejected> {
    let object = match value {
        Value::Null => return Err(("null", String::new())),
        Value::Object(object) => object,
        _ => return Err(("non_object", String::new())),
    };
    let mode = match object.get("mode").filter(|mode| !mode.is_null()) {
        None => defaults.mode().to_owned(),
        Some(Value::String(mode)) => match is_mode(mode.trim()) {
            true => mode.trim().to_owned(),
            false => return Err(("unknown_mode", mode.trim().to_owned())),
        },
        Some(_) => return Err(("mode_not_string", String::new())),
    };
    let reject = |reason: &'static str| (reason, mode.clone());
    let primary = match object.get("primary_url").filter(|url| !url.is_null()) {
        None => defaults.primary_url.trim().to_owned(),
        Some(Value::String(url)) => match url.trim() {
            "" => defaults.primary_url.trim().to_owned(),
            url if is_safe_log_url(url) => url.to_owned(),
            _ => return Err(reject("unsafe_primary")),
        },
        Some(_) => return Err(reject("primary_not_string")),
    };
    if primary.is_empty() {
        return Err(reject("no_primary"));
    }
    let shadows = match mode == MODE_PRIMARY_AND_SHADOW {
        false => Vec::new(),
        true => {
            let candidates = match object.get("shadow_urls").filter(|urls| !urls.is_null()) {
                None => defaults.shadow_urls.clone(),
                Some(raw) => flag_shadow_urls(raw).map_err(reject)?,
            };
            dedupe_shadow_urls(&primary, &candidates)
        }
    };
    Ok((
        LogWriteConfig {
            primary_url: primary,
            shadow_urls: shadows,
            timeout: log_write_timeout(object.get("timeout_ms"), defaults.timeout),
            max_inflight_shadow_writes: max_inflight(object.get("max_inflight_shadow_writes"), defaults.max_inflight()),
        },
        mode,
    ))
}

fn flag_shadow_urls(raw: &Value) -> Result<Vec<String>, &'static str> {
    let Value::Array(items) = raw else {
        return Err("shadow_not_array");
    };
    if items.len() > MAX_SHADOW_URLS {
        return Err("too_many_shadows");
    }
    items
        .iter()
        .map(|item| match item {
            Value::String(url) => match is_safe_log_url(url.trim()) {
                true => Ok(url.trim().to_owned()),
                false => Err("unsafe_shadow"),
            },
            _ => Err("shadow_not_string"),
        })
        .collect()
}

fn integer(value: Option<&Value>) -> i64 {
    value
        .and_then(|number| number.as_i64().or_else(|| number.as_f64().map(|float| float as i64)))
        .unwrap_or(0)
}

fn log_write_timeout(value: Option<&Value>, fallback: Duration) -> Duration {
    match integer(value) {
        millis if millis <= 0 && fallback > Duration::ZERO => fallback,
        millis if millis <= 0 => DEFAULT_TIMEOUT,
        millis => capped_timeout(millis),
    }
}

fn max_inflight(value: Option<&Value>, fallback: i64) -> i64 {
    match integer(value) {
        inflight if inflight <= 0 => fallback,
        inflight => inflight,
    }
}

pub fn is_safe_log_url(raw: &str) -> bool {
    let Some(url) = Some(raw)
        .filter(|raw| !raw.is_empty())
        .and_then(|raw| GoUrl::parse(raw).ok())
    else {
        return false;
    };
    if url.scheme != "http" || url.host.is_empty() {
        return false;
    }
    let host = hostname(&url.host);
    if matches!(host, "localhost" | "127.0.0.1" | "::1") {
        return true;
    }
    let lower = host.to_ascii_lowercase();
    if ALLOWED_HOST_SUFFIXES.iter().any(|suffix| lower.ends_with(suffix)) {
        return true;
    }
    host.parse::<IpAddr>().is_ok_and(|ip| match ip {
        IpAddr::V4(ip) => ip.is_loopback() || ip.is_private() || ip.is_link_local(),
        IpAddr::V6(ip) => {
            ip.is_loopback() || (ip.segments()[0] & 0xfe00) == 0xfc00 || (ip.segments()[0] & 0xffc0) == 0xfe80
        }
    })
}

fn hostname(host: &str) -> &str {
    match host.strip_prefix('[') {
        Some(rest) => rest.split_once(']').map_or(rest, |(inside, _)| inside),
        None => match host.matches(':').count() {
            1 => host.split_once(':').map_or(host, |(name, _)| name),
            _ => host,
        },
    }
}

pub struct LogWriteConfigResolver {
    flags: FeatureFlags,
    defaults: LogWriteDefaults,
    ttl: Duration,
    cached: Mutex<Option<(Instant, LogWriteConfig)>>,
}

impl LogWriteConfigResolver {
    pub fn new(flags: FeatureFlags, defaults: LogWriteDefaults) -> Arc<Self> {
        Self::with_ttl(flags, defaults, RESOLVER_TTL)
    }

    pub fn with_ttl(flags: FeatureFlags, defaults: LogWriteDefaults, ttl: Duration) -> Arc<Self> {
        Arc::new(Self {
            flags,
            defaults,
            ttl,
            cached: Mutex::new(None),
        })
    }

    pub fn resolve(&self) -> LogWriteConfig {
        let mut cached = self.cached.lock();
        let now = Instant::now();
        match cached.as_ref() {
            Some((expiry, config)) if now < *expiry => config.clone(),
            _ => {
                let config = resolve_log_write_config(&self.flags, &self.defaults);
                *cached = Some((now + self.ttl, config.clone()));
                config
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const COLLECTOR: &str = "http://localhost:30006";

    fn collector_defaults() -> LogWriteDefaults {
        LogWriteDefaults {
            primary_url: COLLECTOR.to_owned(),
            ..LogWriteDefaults::default()
        }
    }

    fn env_defaults() -> LogWriteDefaults {
        LogWriteDefaults {
            mode: MODE_PRIMARY_AND_SHADOW.to_owned(),
            primary_url: "http://collector.example:30006".to_owned(),
            shadow_urls: vec![
                "http://shadow.example/logs".to_owned(),
                "http://collector.example:30006".to_owned(),
            ],
            timeout: Duration::from_secs(3),
            max_inflight_shadow_writes: 32,
        }
    }

    fn legacy() -> LogWriteConfig {
        LogWriteConfig {
            primary_url: COLLECTOR.to_owned(),
            shadow_urls: Vec::new(),
            timeout: Duration::ZERO,
            max_inflight_shadow_writes: DEFAULT_MAX_INFLIGHT_SHADOW_WRITES,
        }
    }

    fn config(primary: &str, shadows: &[&str], timeout: Duration, inflight: i64) -> LogWriteConfig {
        LogWriteConfig {
            primary_url: primary.to_owned(),
            shadow_urls: shadows.iter().map(|url| (*url).to_owned()).collect(),
            timeout,
            max_inflight_shadow_writes: inflight,
        }
    }

    fn env_config() -> LogWriteConfig {
        config(
            "http://collector.example:30006",
            &["http://shadow.example/logs"],
            Duration::from_secs(3),
            32,
        )
    }

    fn resolved(defaults: &LogWriteDefaults, value: Value) -> LogWriteConfig {
        let flags = FeatureFlags::builder()
            .override_flag("logs-write-config", value)
            .offline();
        resolve_log_write_config(&flags, defaults)
    }

    fn local(primary: &str) -> LogWriteConfig {
        config(primary, &[], DEFAULT_TIMEOUT, DEFAULT_MAX_INFLIGHT_SHADOW_WRITES)
    }

    #[test]
    fn unusable_flag_values_fall_back_to_the_legacy_collector() {
        let defaults = collector_defaults();
        for value in [
            Value::Null,
            json!("nonsense"),
            json!({"mode": "bogus", "primary_url": "http://localhost:9999"}),
            json!({"mode": true, "primary_url": "http://localhost:9999"}),
            json!({"mode": MODE_PRIMARY_ONLY, "primary_url": 123}),
            json!({"mode": MODE_PRIMARY_ONLY, "primary_url": "https://evil.example.com/logs"}),
            json!({"mode": MODE_PRIMARY_ONLY, "primary_url": "http://evil.example.com/logs"}),
            json!({"mode": MODE_PRIMARY_AND_SHADOW, "primary_url": COLLECTOR, "shadow_urls": "http://127.0.0.1:4321/logs"}),
            json!({"mode": MODE_PRIMARY_AND_SHADOW, "primary_url": COLLECTOR, "shadow_urls": ["http://127.0.0.1:4321/logs", 42]}),
            json!({"mode": MODE_PRIMARY_AND_SHADOW, "primary_url": COLLECTOR, "shadow_urls": [
                "http://127.0.0.1:4321/a", "http://127.0.0.1:4321/b", "http://127.0.0.1:4321/c",
                "http://127.0.0.1:4321/d", "http://127.0.0.1:4321/e"]}),
            json!({"mode": MODE_PRIMARY_AND_SHADOW, "primary_url": COLLECTOR, "shadow_urls": ["http://8.8.8.8/logs"]}),
        ] {
            assert_eq!(resolved(&defaults, value.clone()), legacy(), "{value}");
        }
    }

    #[test]
    fn a_configured_flag_overrides_the_collector_address() {
        let defaults = collector_defaults();

        assert_eq!(
            resolved(&defaults, json!({"primary_url": "http://localhost:9999"})),
            local("http://localhost:9999")
        );
        assert_eq!(
            resolved(
                &defaults,
                json!({"mode": MODE_PRIMARY_ONLY, "primary_url": "http://127.0.0.1:30006", "timeout_ms": 1500})
            ),
            config("http://127.0.0.1:30006", &[], Duration::from_millis(1500), 1024)
        );
        assert_eq!(
            resolved(
                &defaults,
                json!({"mode": MODE_PRIMARY_ONLY, "primary_url": "  http://127.0.0.1:30006/logs  "})
            ),
            local("http://127.0.0.1:30006/logs")
        );
        assert_eq!(
            resolved(
                &defaults,
                json!({"mode": MODE_PRIMARY_ONLY, "primary_url": "", "timeout_ms": 1500})
            ),
            config(COLLECTOR, &[], Duration::from_millis(1500), 1024)
        );
        assert_eq!(
            resolved(
                &defaults,
                json!({"mode": MODE_PRIMARY_ONLY, "shadow_urls": [], "timeout_ms": 2000, "max_inflight_shadow_writes": 1024})
            ),
            config(COLLECTOR, &[], Duration::from_millis(2000), 1024)
        );
    }

    #[test]
    fn a_missing_primary_url_with_no_collector_address_falls_back() {
        assert_eq!(
            resolved(&LogWriteDefaults::default(), json!({"mode": MODE_PRIMARY_ONLY})),
            config("", &[], Duration::ZERO, DEFAULT_MAX_INFLIGHT_SHADOW_WRITES)
        );
    }

    #[test]
    fn env_defaults_fill_every_key_the_flag_leaves_out() {
        let defaults = env_defaults();

        assert_eq!(resolved(&defaults, Value::Null), env_config());
        assert_eq!(
            resolved(&defaults, json!({"primary_url": "http://evil.example.com/logs"})),
            env_config()
        );
        assert_eq!(resolved(&defaults, json!({})), env_config());
        assert_eq!(
            resolved(&defaults, json!({"mode": MODE_PRIMARY_ONLY})),
            config("http://collector.example:30006", &[], Duration::from_secs(3), 32)
        );
        assert_eq!(
            resolved(&defaults, json!({"shadow_urls": []})),
            config("http://collector.example:30006", &[], Duration::from_secs(3), 32)
        );
        assert_eq!(
            resolved(
                &defaults,
                json!({"mode": MODE_PRIMARY_ONLY, "timeout_ms": 0, "max_inflight_shadow_writes": -1})
            ),
            config("http://collector.example:30006", &[], Duration::from_secs(3), 32)
        );
    }

    #[test]
    fn every_flag_key_overrides_its_env_default() {
        assert_eq!(
            resolved(
                &env_defaults(),
                json!({
                    "mode": MODE_PRIMARY_AND_SHADOW,
                    "primary_url": "http://127.0.0.1:30006",
                    "shadow_urls": ["http://127.0.0.1:4321/logs"],
                    "timeout_ms": 1500,
                    "max_inflight_shadow_writes": 64,
                })
            ),
            config(
                "http://127.0.0.1:30006",
                &["http://127.0.0.1:4321/logs"],
                Duration::from_millis(1500),
                64
            )
        );
    }

    #[test]
    fn shadow_urls_are_deduplicated_against_the_primary_and_each_other() {
        let defaults = LogWriteDefaults {
            primary_url: "http://127.0.0.1:30006".to_owned(),
            ..LogWriteDefaults::default()
        };
        assert_eq!(
            resolved(
                &defaults,
                json!({"mode": MODE_PRIMARY_AND_SHADOW, "shadow_urls": ["http://127.0.0.1:30006", "http://127.0.0.1:4321/logs"]})
            ),
            config(
                "http://127.0.0.1:30006",
                &["http://127.0.0.1:4321/logs"],
                DEFAULT_TIMEOUT,
                1024
            )
        );
        assert_eq!(
            resolved(
                &collector_defaults(),
                json!({
                    "mode": MODE_PRIMARY_AND_SHADOW,
                    "primary_url": COLLECTOR,
                    "shadow_urls": [COLLECTOR, "http://127.0.0.1:4321/logs", "http://127.0.0.1:4321/logs"],
                })
            ),
            config(COLLECTOR, &["http://127.0.0.1:4321/logs"], DEFAULT_TIMEOUT, 1024)
        );
    }

    #[test]
    fn primary_and_shadow_accepts_local_urls() {
        assert_eq!(
            resolved(
                &collector_defaults(),
                json!({
                    "mode": MODE_PRIMARY_AND_SHADOW,
                    "primary_url": COLLECTOR,
                    "shadow_urls": ["http://127.0.0.1:4321/logs", "http://[::1]:4321/logs"],
                    "timeout_ms": 2500,
                })
            ),
            config(
                COLLECTOR,
                &["http://127.0.0.1:4321/logs", "http://[::1]:4321/logs"],
                Duration::from_millis(2500),
                1024
            )
        );
    }

    #[test]
    fn timeouts_default_when_non_positive_and_are_capped() {
        let timeout = |millis: Value| {
            resolved(
                &collector_defaults(),
                json!({"mode": MODE_PRIMARY_ONLY, "primary_url": COLLECTOR, "timeout_ms": millis}),
            )
            .timeout
        };

        assert_eq!(timeout(json!(0)), DEFAULT_TIMEOUT);
        assert_eq!(timeout(json!(60000)), MAX_TIMEOUT);
        assert_eq!(timeout(json!(10000000000000i64)), MAX_TIMEOUT);
    }

    #[test]
    fn max_inflight_shadow_writes_fall_through_when_non_positive() {
        let inflight = |value: Value| {
            resolved(
                &collector_defaults(),
                json!({"mode": MODE_PRIMARY_ONLY, "primary_url": COLLECTOR, "max_inflight_shadow_writes": value}),
            )
            .max_inflight_shadow_writes
        };

        assert_eq!(inflight(json!(64)), 64);
        assert_eq!(inflight(json!(0)), DEFAULT_MAX_INFLIGHT_SHADOW_WRITES);
        assert_eq!(inflight(json!(999999)), 999999);
    }

    #[test]
    fn the_defaults_come_from_the_logs_write_variables() {
        let vars = |name: &str| {
            match name {
                "LOGS_WRITE_MODE" => Some(" primary_and_shadow "),
                "LOGS_WRITE_SHADOW_URLS" => Some("http://a/logs, ,http://b/logs"),
                "LOGS_WRITE_TIMEOUT_MS" => Some("60000"),
                "LOGS_WRITE_MAX_INFLIGHT_SHADOW_WRITES" => Some("0"),
                _ => None,
            }
            .map(str::to_owned)
        };

        assert_eq!(
            LogWriteDefaults::from_vars(" http://collector:1 ", &vars),
            LogWriteDefaults {
                mode: MODE_PRIMARY_AND_SHADOW.to_owned(),
                primary_url: "http://collector:1".to_owned(),
                shadow_urls: vec!["http://a/logs".to_owned(), "http://b/logs".to_owned()],
                timeout: MAX_TIMEOUT,
                max_inflight_shadow_writes: 0,
            }
        );
        assert_eq!(
            LogWriteDefaults::from_vars("x", &|_| Some("bogus".to_owned())).mode,
            String::new(),
            "an unknown mode is ignored"
        );
    }

    #[test]
    fn only_local_and_internal_http_urls_are_safe() {
        for url in [
            "http://localhost:30006",
            "http://127.0.0.1:1/logs",
            "http://[::1]:4321",
            "http://10.0.0.5/x",
            "http://192.168.1.1/x",
            "http://169.254.1.1/x",
            "http://logs.service.consul:8080",
            "http://collector.svc.cluster.local",
            "http://a.internal",
            "http://[fd00::1]/x",
        ] {
            assert!(is_safe_log_url(url), "{url}");
        }
        for url in [
            "",
            "https://localhost",
            "http://8.8.8.8/logs",
            "http://evil.example.com",
            "ftp://localhost",
            "localhost:3000",
            "http://",
        ] {
            assert!(!is_safe_log_url(url), "{url}");
        }
    }

    #[test]
    fn the_resolver_serves_the_cache_until_it_expires() {
        let flags = FeatureFlags::offline();
        let resolver = LogWriteConfigResolver::with_ttl(flags, collector_defaults(), Duration::from_secs(3600));

        assert_eq!(resolver.resolve(), legacy());
        assert_eq!(resolver.resolve(), legacy());
        let expired = LogWriteConfigResolver::with_ttl(FeatureFlags::offline(), collector_defaults(), Duration::ZERO);
        assert_eq!(expired.resolve(), legacy());
    }
}
