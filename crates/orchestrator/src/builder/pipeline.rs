use std::sync::Arc;

use futures::FutureExt;
use futures::future::BoxFuture;

use super::builds::BuildInfo;
use super::phase::{BuildError, Phase, run_phases};
use super::service::TemplateBuilder;
use crate::proto::{TemplateBuildMetadata, TemplateCreateRequest};

pub struct PhasePipeline;

impl PhasePipeline {
    fn phases(&self, _request: &TemplateCreateRequest) -> Vec<Box<dyn Phase>> {
        Vec::new()
    }
}

impl TemplateBuilder for PhasePipeline {
    fn build(
        &self,
        request: TemplateCreateRequest,
        info: Arc<BuildInfo>,
    ) -> BoxFuture<'static, Result<TemplateBuildMetadata, BuildError>> {
        let phases = self.phases(&request);
        async move {
            if phases.is_empty() {
                return Err(BuildError::Internal(
                    "template build phases are not ported yet".to_owned(),
                ));
            }
            run_phases(&info.log, &phases)
                .await
                .map(|_| TemplateBuildMetadata::default())
        }
        .boxed()
    }
}
