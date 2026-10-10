#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    MissingHeader { header: &'static str },
    InvalidHost,
    InvalidSandboxId,
    InvalidSandboxPort { port: String },
    SandboxNotFound { sandbox_id: String },
    ResumePermissionDenied { sandbox_id: String },
    StillTransitioning { sandbox_id: String },
    InternalRoute { sandbox_id: String, path: String },
    ResourceExhausted { sandbox_id: String, message: String },
    MissingTrafficAccessToken { sandbox_id: String, header: &'static str },
    InvalidTrafficAccessToken { sandbox_id: String, header: &'static str },
    Unexpected { message: String },
}
