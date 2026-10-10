use std::time::Duration;

use tokio::time::Instant;

use crate::template::Template;

pub const PENDING_TEXT: &str = "parent memfile header is still deduplicating";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ready,
    ReadyAfterWait,
    Refused,
    LatchedError,
}

impl Outcome {
    pub fn label(self) -> &'static str {
        match self {
            Outcome::Ready => "ready",
            Outcome::ReadyAfterWait => "ready_after_wait",
            Outcome::Refused => "refused",
            Outcome::LatchedError => "latched_error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    Pending,
    Latched(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Admission {
    pub outcome: Outcome,
    pub waited: Duration,
    pub refusal: Option<Refusal>,
}

impl Admission {
    fn admitted(outcome: Outcome, waited: Duration) -> Self {
        Self {
            outcome,
            waited,
            refusal: None,
        }
    }
}

pub async fn await_snapshot_admission(template: &Template, grace: Duration, memory_snapshot: bool) -> Admission {
    if !memory_snapshot || template.build().await.is_err() || template.durable_memfile_header_now().await.is_some() {
        return Admission::admitted(Outcome::Ready, Duration::ZERO);
    }
    let started = Instant::now();
    let waited = tokio::time::timeout(grace, template.durable_memfile_header()).await;
    let waited_for = started.elapsed();
    match waited {
        Ok(Ok(_)) => Admission::admitted(Outcome::ReadyAfterWait, waited_for),
        Ok(Err(error)) => Admission {
            outcome: Outcome::LatchedError,
            waited: waited_for,
            refusal: Some(Refusal::Latched(error.to_string())),
        },
        Err(_) => Admission {
            outcome: Outcome::Refused,
            waited: waited_for,
            refusal: Some(Refusal::Pending),
        },
    }
}

#[cfg(test)]
mod tests {
    use futures::FutureExt;
    use tokio::sync::oneshot;
    use uuid::Uuid;

    use super::*;
    use crate::storage::backend::Storage;
    use crate::storage::build::Build;
    use crate::storage::header::{Header, Metadata};
    use crate::template::metadata::TemplateMetadata;

    fn header(incomplete: bool) -> Header {
        Header {
            metadata: Metadata::template(Uuid::nil(), 4096, 4096),
            mappings: Vec::new(),
            builds: None,
            incomplete_pending_upload: incomplete,
        }
    }

    fn template(live_incomplete: bool) -> (Template, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let build = Build {
            build_id: Uuid::nil(),
            memfile: header(live_incomplete),
            rootfs: header(false),
        };
        let template = Template::cached(
            Storage::local(dir.path()).unwrap(),
            build,
            dir.path().join("snapfile"),
            TemplateMetadata::default(),
            dir.path().to_path_buf(),
        );
        (template, dir)
    }

    fn pending(template: &Template) -> oneshot::Sender<Result<Header, String>> {
        let (resolve, resolved) = oneshot::channel();
        template
            .memfile_headers()
            .set_durable(async move { resolved.await.unwrap_or_else(|e| Err(e.to_string())) }.boxed());
        resolve
    }

    #[tokio::test]
    async fn a_filesystem_only_snapshot_has_no_memory_parent_to_wait_for() {
        let (template, _dir) = template(true);
        let _unresolved = pending(&template);

        let admission = await_snapshot_admission(&template, Duration::ZERO, false).await;

        assert_eq!(admission, Admission::admitted(Outcome::Ready, Duration::ZERO));
    }

    #[tokio::test]
    async fn a_durable_parent_is_admitted_without_waiting() {
        let (template, _dir) = template(false);

        let admission = await_snapshot_admission(&template, Duration::ZERO, true).await;

        assert_eq!(admission.outcome, Outcome::Ready);
        assert_eq!(admission.waited, Duration::ZERO);
    }

    #[tokio::test]
    async fn an_instant_probe_of_a_deduplicating_parent_refuses_retryably() {
        let (template, _dir) = template(true);
        let _unresolved = pending(&template);

        let admission = await_snapshot_admission(&template, Duration::ZERO, true).await;

        assert_eq!(admission.outcome, Outcome::Refused);
        assert_eq!(admission.refusal, Some(Refusal::Pending));
    }

    #[tokio::test]
    async fn a_swap_that_lands_inside_the_grace_is_ready_after_the_wait() {
        let (template, _dir) = template(true);
        let resolve = pending(&template);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            let _ = resolve.send(Ok(header(true)));
        });

        let admission = await_snapshot_admission(&template, Duration::from_secs(5), true).await;

        assert_eq!(admission.outcome, Outcome::ReadyAfterWait);
        assert!(admission.waited > Duration::ZERO);
        assert_eq!(admission.refusal, None);
    }

    #[tokio::test]
    async fn a_dedup_that_failed_for_good_is_latched_not_retryable() {
        let (template, _dir) = template(true);
        pending(&template).send(Err("dedup exploded".to_owned())).unwrap();

        let admission = await_snapshot_admission(&template, Duration::ZERO, true).await;

        assert_eq!(admission.outcome, Outcome::LatchedError);
        assert_eq!(admission.refusal, Some(Refusal::Latched("dedup exploded".to_owned())));
    }

    #[test]
    fn the_outcome_labels_are_the_dashboard_values() {
        assert_eq!(Outcome::Ready.label(), "ready");
        assert_eq!(Outcome::ReadyAfterWait.label(), "ready_after_wait");
        assert_eq!(Outcome::Refused.label(), "refused");
        assert_eq!(Outcome::LatchedError.label(), "latched_error");
    }
}
