use futures::future::BoxFuture;

use std::collections::BTreeMap;

use super::logs::{BuildLog, ScopedLog};
use crate::proto::{LogLevel, TemplateBuildStatusReason};
use crate::template::metadata::TemplateMetadata;

pub const INTERNAL_ERROR_MESSAGE: &str =
    "An internal error occurred. Please try again or contact support with the build ID.";
pub const CANCELLED: &str = "build was cancelled";
pub const TIMED_OUT: &str = "build timed out";
const GO_NIL: &str = "<nil>";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseMeta {
    pub phase: &'static str,
    pub step_type: String,
    pub step_number: Option<usize>,
}

impl PhaseMeta {
    pub fn log_fields(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("phase".to_owned(), self.phase.to_owned()),
            ("step_type".to_owned(), self.step_type.clone()),
            (
                "step_number".to_owned(),
                self.step_number
                    .map_or_else(|| GO_NIL.to_owned(), |number| number.to_string()),
            ),
            ("step".to_owned(), self.step()),
        ])
    }

    pub fn step(&self) -> String {
        self.step_number
            .map_or_else(|| self.step_type.clone(), |number| number.to_string())
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LayerResult {
    pub metadata: TemplateMetadata,
    pub cached: bool,
    pub hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildError {
    User {
        phase: String,
        step: String,
        message: String,
    },
    Internal(String),
}

impl BuildError {
    pub fn user(meta: &PhaseMeta, message: impl Into<String>) -> Self {
        BuildError::User {
            phase: meta.phase.to_owned(),
            step: meta.step(),
            message: message.into(),
        }
    }

    pub fn cancelled() -> Self {
        BuildError::User {
            phase: String::new(),
            step: String::new(),
            message: CANCELLED.to_owned(),
        }
    }

    pub fn timed_out() -> Self {
        BuildError::User {
            phase: String::new(),
            step: String::new(),
            message: TIMED_OUT.to_owned(),
        }
    }

    pub fn reason(&self) -> TemplateBuildStatusReason {
        match self {
            BuildError::User { step, message, .. } => TemplateBuildStatusReason {
                message: message.clone(),
                step: Some(step.clone()),
            },
            BuildError::Internal(_) => TemplateBuildStatusReason {
                message: INTERNAL_ERROR_MESSAGE.to_owned(),
                step: None,
            },
        }
    }
}

pub trait Phase: Send + Sync {
    fn prefix(&self) -> String;
    fn describe(&self) -> String;
    fn meta(&self) -> PhaseMeta;
    fn hash<'a>(&'a self, source: &'a LayerResult) -> BoxFuture<'a, Result<String, BuildError>>;
    fn layer<'a>(&'a self, source: &'a LayerResult, hash: String) -> BoxFuture<'a, Result<LayerResult, BuildError>>;
    fn build<'a>(
        &'a self,
        log: ScopedLog<'a>,
        source: &'a LayerResult,
        current: LayerResult,
    ) -> BoxFuture<'a, Result<LayerResult, BuildError>>;
}

pub async fn run_phases(log: &BuildLog, phases: &[Box<dyn Phase>]) -> Result<LayerResult, BuildError> {
    let mut source = LayerResult::default();
    for phase in phases {
        source = run_phase(log, phase.as_ref(), &source).await?;
    }
    Ok(source)
}

async fn run_phase(log: &BuildLog, phase: &dyn Phase, source: &LayerResult) -> Result<LayerResult, BuildError> {
    let fields = phase.meta().log_fields();
    let log = log.scoped(&fields);
    let hash = phase
        .hash(source)
        .await
        .map_err(|e| internal_context("getting hash", e))?;
    let current = phase
        .layer(source, hash)
        .await
        .map_err(|e| internal_context("getting layer", e))?;
    log.push(
        LogLevel::Info,
        layer_info(current.cached, &phase.prefix(), &phase.describe(), &current.hash),
    );
    if current.cached {
        return Ok(current);
    }
    validate_layer(&current).map_err(|e| BuildError::Internal(format!("validating layer: {e}")))?;
    phase.build(log, source, current).await
}

fn internal_context(context: &str, error: BuildError) -> BuildError {
    match error {
        BuildError::Internal(e) => BuildError::Internal(format!("{context}: {e}")),
        user => user,
    }
}

pub fn layer_info(cached: bool, prefix: &str, text: &str, hash: &str) -> String {
    let cached_prefix = if cached { "CACHED " } else { "" };
    format!("{cached_prefix}[{prefix}] {text} [{hash}]")
}

fn validate_layer(layer: &LayerResult) -> Result<(), String> {
    let template = &layer.metadata.template;
    let context = &layer.metadata.context;
    let problems: Vec<&str> = [
        (layer.hash.is_empty(), "layer hash is empty"),
        (template.build_id.is_empty(), "template build ID is empty"),
        (template.kernel_version.is_empty(), "template kernel version is empty"),
        (
            template.firecracker_version.is_empty(),
            "template firecracker version is empty",
        ),
        (
            context.user.as_deref().unwrap_or_default().is_empty(),
            "context user is empty",
        ),
        (context.workdir.as_deref() == Some(""), "context working dir is empty"),
    ]
    .into_iter()
    .filter_map(|(failed, problem)| failed.then_some(problem))
    .collect();
    match problems.is_empty() {
        true => Ok(()),
        false => Err(problems.join("\n")),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures::FutureExt;
    use parking_lot::Mutex;

    use super::*;
    use crate::template::metadata::{Context, TemplateVersions};

    struct Stub {
        name: &'static str,
        cached: bool,
        fails: Option<BuildError>,
        built: Arc<Mutex<Vec<&'static str>>>,
    }

    fn valid(build_id: &str) -> TemplateMetadata {
        TemplateMetadata {
            version: 2,
            template: TemplateVersions {
                build_id: build_id.to_owned(),
                kernel_version: "vmlinux-6.1.158".to_owned(),
                firecracker_version: "v1.14".to_owned(),
            },
            context: Context {
                user: Some("user".to_owned()),
                ..Context::default()
            },
            ..TemplateMetadata::default()
        }
    }

    impl Phase for Stub {
        fn prefix(&self) -> String {
            self.name.to_owned()
        }

        fn describe(&self) -> String {
            format!("RUN {}", self.name)
        }

        fn meta(&self) -> PhaseMeta {
            PhaseMeta {
                phase: "steps",
                step_type: "RUN".to_owned(),
                step_number: Some(1),
            }
        }

        fn hash<'a>(&'a self, source: &'a LayerResult) -> BoxFuture<'a, Result<String, BuildError>> {
            async move { Ok(format!("{}{}", source.hash, self.name)) }.boxed()
        }

        fn layer<'a>(&'a self, _: &'a LayerResult, hash: String) -> BoxFuture<'a, Result<LayerResult, BuildError>> {
            async move {
                Ok(LayerResult {
                    metadata: valid(self.name),
                    cached: self.cached,
                    hash,
                })
            }
            .boxed()
        }

        fn build<'a>(
            &'a self,
            _: ScopedLog<'a>,
            _: &'a LayerResult,
            current: LayerResult,
        ) -> BoxFuture<'a, Result<LayerResult, BuildError>> {
            async move {
                self.built.lock().push(self.name);
                self.fails.clone().map_or(Ok(current), Err)
            }
            .boxed()
        }
    }

    fn stub(
        name: &'static str,
        cached: bool,
        fails: Option<BuildError>,
        built: &Arc<Mutex<Vec<&'static str>>>,
    ) -> Box<dyn Phase> {
        Box::new(Stub {
            name,
            cached,
            fails,
            built: built.clone(),
        })
    }

    #[tokio::test]
    async fn cached_layers_are_logged_and_skipped() {
        let built = Arc::default();
        let log = BuildLog::default();

        let result = run_phases(&log, &[stub("a", true, None, &built), stub("b", false, None, &built)])
            .await
            .unwrap();

        assert_eq!(result.hash, "ab");
        assert_eq!(*built.lock(), vec!["b"]);
        let lines: Vec<String> = log
            .query(&Default::default())
            .into_iter()
            .map(|entry| entry.message)
            .collect();
        assert_eq!(lines, vec!["CACHED [a] RUN a [a]", "[b] RUN b [ab]"]);
        let fields = log.query(&Default::default())[0].fields.clone();
        assert_eq!(
            fields.into_iter().collect::<BTreeMap<_, _>>(),
            BTreeMap::from([
                ("phase".to_owned(), "steps".to_owned()),
                ("step_type".to_owned(), "RUN".to_owned()),
                ("step_number".to_owned(), "1".to_owned()),
                ("step".to_owned(), "1".to_owned()),
            ])
        );
    }

    #[tokio::test]
    async fn a_user_error_keeps_its_step_and_internal_errors_are_hidden() {
        let built = Arc::default();
        let meta = PhaseMeta {
            phase: "steps",
            step_type: "RUN".to_owned(),
            step_number: Some(3),
        };
        let user = BuildError::user(&meta, "command exited with 1");

        let error = run_phases(&BuildLog::default(), &[stub("a", false, Some(user), &built)])
            .await
            .unwrap_err();
        assert_eq!(
            error.reason(),
            TemplateBuildStatusReason {
                message: "command exited with 1".to_owned(),
                step: Some("3".to_owned()),
            }
        );
        assert_eq!(
            BuildError::Internal("disk".to_owned()).reason().message,
            INTERNAL_ERROR_MESSAGE
        );
        assert_eq!(BuildError::cancelled().reason().message, "build was cancelled");
    }

    #[test]
    fn invalid_layers_name_every_problem() {
        let layer = LayerResult::default();

        assert_eq!(
            validate_layer(&layer).unwrap_err(),
            "layer hash is empty\ntemplate build ID is empty\ntemplate kernel version is empty\ntemplate firecracker version is empty\ncontext user is empty"
        );
    }
}
