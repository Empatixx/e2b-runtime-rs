use tracing::field::Empty;
use tracing::{Span, info_span};

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
