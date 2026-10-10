use opentelemetry::trace::TraceContextExt;
use tracing::field::Empty;
use tracing::{Span, info_span};
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::memory::prefetch::plan::PrefetchSummary;
use crate::proto::{SandboxCheckpointRequest, SandboxConfig, SandboxPauseRequest};

const BALLOON_MODE_UNKNOWN: &str = "unknown";

macro_rules! sandbox_span {
    ($name:literal) => {
        info_span!(
            $name,
            "sandbox.id" = Empty,
            "team.id" = Empty,
            "template.id" = Empty,
            "build.id" = Empty,
            "sandbox.firecracker.version" = Empty,
            "sandbox.kernel.version" = Empty,
            "envd.version" = Empty,
            "balloon_mode" = Empty,
            route = Empty,
        )
    };
}

pub fn sandbox_create(config: Option<&SandboxConfig>) -> Span {
    let span = sandbox_span!("sandbox-create");
    config.iter().for_each(|config| record_config(&span, config));
    span
}

pub fn sandbox_update(sandbox_id: &str) -> Span {
    let span = sandbox_span!("sandbox-update");
    record(&span, "sandbox.id", sandbox_id);
    span
}

pub fn sandbox_list() -> Span {
    info_span!("sandbox-list")
}

pub fn sandbox_delete(sandbox_id: &str) -> Span {
    let span = sandbox_span!("sandbox-delete");
    record(&span, "sandbox.id", sandbox_id);
    span
}

pub fn sandbox_pause(request: &SandboxPauseRequest) -> Span {
    let span = sandbox_span!("sandbox-pause");
    record(&span, "sandbox.id", &request.sandbox_id);
    record(&span, "template.id", &request.template_id);
    record(&span, "build.id", &request.build_id);
    span
}

pub fn sandbox_checkpoint(request: &SandboxCheckpointRequest) -> Span {
    let span = sandbox_span!("sandbox-checkpoint");
    record(&span, "sandbox.id", &request.sandbox_id);
    record(&span, "build.id", &request.build_id);
    span
}

pub fn execution(config: &SandboxConfig, prefetch: Option<&PrefetchSummary>, init_origin: &str) -> Span {
    let span = info_span!(
        parent: None,
        "execute sandbox",
        "sandbox.id" = %config.sandbox_id,
        "template.id" = %config.template_id,
        "build.id" = %config.build_id,
        "resume.prefetch.source" = Empty,
        "resume.prefetch.init_origin" = Empty,
        "resume.prefetch.init_blocks" = Empty,
        "resume.prefetch.last_cycle_blocks" = Empty,
    );
    prefetch.iter().for_each(|summary| {
        record(&span, "resume.prefetch.source", summary.source);
        record(&span, "resume.prefetch.init_origin", init_origin);
        span.record("resume.prefetch.init_blocks", summary.init_blocks as i64);
        span.record("resume.prefetch.last_cycle_blocks", summary.last_cycle_blocks as i64);
    });
    Span::current().add_link(span.context().span().span_context().clone());
    span
}

pub fn record_balloon_mode() {
    record(&Span::current(), "balloon_mode", BALLOON_MODE_UNKNOWN);
}

pub fn record_checkpoint_route(route: &str, balloon_mode: &str) {
    let span = Span::current();
    record(&span, "balloon_mode", balloon_mode);
    record(&span, "route", route);
}

pub fn record_running(config: &SandboxConfig) {
    let span = Span::current();
    record_config(&span, config);
    record(&span, "balloon_mode", BALLOON_MODE_UNKNOWN);
}

fn record_config(span: &Span, config: &SandboxConfig) {
    [
        ("build.id", &config.build_id),
        ("team.id", &config.team_id),
        ("template.id", &config.template_id),
        ("sandbox.kernel.version", &config.kernel_version),
        ("sandbox.id", &config.sandbox_id),
        ("envd.version", &config.envd_version),
        ("sandbox.firecracker.version", &config.firecracker_version),
    ]
    .into_iter()
    .for_each(|(field, value)| record(span, field, value));
}

fn record(span: &Span, field: &str, value: &str) {
    span.record(field, value);
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use parking_lot::Mutex;
    use tracing::field::{Field, Visit};
    use tracing::span::Attributes;
    use tracing::{Id, Subscriber};
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::{Context, SubscriberExt};
    use tracing_subscriber::registry::LookupSpan;

    use super::*;

    type Recorded = Arc<Mutex<BTreeMap<String, String>>>;

    struct Capture(Recorded);

    struct Fields<'a>(&'a mut BTreeMap<String, String>);

    impl Visit for Fields<'_> {
        fn record_str(&mut self, field: &Field, value: &str) {
            self.0.insert(field.name().to_owned(), value.to_owned());
        }

        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.0.insert(field.name().to_owned(), format!("{value:?}"));
        }
    }

    impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for Capture {
        fn on_new_span(&self, attributes: &Attributes<'_>, _: &Id, _: Context<'_, S>) {
            attributes.record(&mut Fields(&mut self.0.lock()));
        }

        fn on_record(&self, _: &Id, values: &tracing::span::Record<'_>, _: Context<'_, S>) {
            values.record(&mut Fields(&mut self.0.lock()));
        }
    }

    fn config() -> SandboxConfig {
        SandboxConfig {
            sandbox_id: "sbx-exec-span".to_owned(),
            template_id: "tpl-exec-span".to_owned(),
            build_id: "build-exec-span".to_owned(),
            ..SandboxConfig::default()
        }
    }

    #[test]
    fn the_execution_span_is_a_new_root_that_carries_the_sandbox_identity() {
        let recorded = Recorded::default();
        let subscriber = tracing_subscriber::registry().with(Capture(recorded.clone()));

        tracing::subscriber::with_default(subscriber, || {
            let span = execution(&config(), None, "none");
            assert!(span.is_disabled() || span.id().is_some());
        });

        let fields = recorded.lock();
        assert_eq!(fields["sandbox.id"], "sbx-exec-span");
        assert_eq!(fields["template.id"], "tpl-exec-span");
        assert_eq!(fields["build.id"], "build-exec-span");
    }

    #[test]
    fn a_resume_records_the_prefetch_source_origin_and_sizes() {
        let recorded = Recorded::default();
        let subscriber = tracing_subscriber::registry().with(Capture(recorded.clone()));
        let summary = PrefetchSummary {
            source: "init",
            init_blocks: 12,
            last_cycle_blocks: 3,
        };

        tracing::subscriber::with_default(subscriber, || drop(execution(&config(), Some(&summary), "harvest")));

        let fields = recorded.lock();
        assert_eq!(fields["resume.prefetch.source"], "init");
        assert_eq!(fields["resume.prefetch.init_origin"], "harvest");
        assert_eq!(fields["resume.prefetch.init_blocks"], "12");
        assert_eq!(fields["resume.prefetch.last_cycle_blocks"], "3");
    }
}
