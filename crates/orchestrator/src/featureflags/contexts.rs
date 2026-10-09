use std::collections::BTreeMap;

use launchdarkly_server_sdk::{Context, ContextBuilder, MultiContextBuilder};

const NONE_KIND: &str = "none";
const ORCHESTRATOR_COMMIT_ATTRIBUTE: &str = "commit";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlagContext {
    kind: String,
    key: String,
    name: Option<String>,
    attributes: BTreeMap<String, String>,
}

#[derive(Debug, Default)]
pub struct FlagContextBuilder {
    kind: String,
    key: String,
    name: Option<String>,
    attributes: BTreeMap<String, String>,
}

impl FlagContextBuilder {
    pub fn kind(mut self, kind: impl Into<String>) -> Self {
        self.kind = kind.into();
        self
    }

    pub fn key(mut self, key: impl Into<String>) -> Self {
        self.key = key.into();
        self
    }

    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn attribute(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.attributes.insert(name.into(), value.into());
        self
    }

    pub fn build(self) -> FlagContext {
        FlagContext {
            kind: self.kind,
            key: self.key,
            name: self.name,
            attributes: self.attributes,
        }
    }
}

impl FlagContext {
    pub fn builder() -> FlagContextBuilder {
        FlagContextBuilder::default()
    }

    fn of_kind(kind: &str, key: impl Into<String>) -> Self {
        Self::builder().kind(kind).key(key).build()
    }

    pub fn sandbox(sandbox_id: &str) -> Self {
        Self::of_kind("sandbox", sandbox_id)
    }

    pub fn team(team_id: &str) -> Self {
        Self::of_kind("team", team_id)
    }

    pub fn team_with_name(team_id: &str, team_name: &str) -> Self {
        Self::builder().kind("team").key(team_id).name(team_name).build()
    }

    pub fn template(template_id: &str) -> Self {
        Self::of_kind("template", template_id)
    }

    pub fn cluster(cluster_id: &str) -> Self {
        Self::of_kind("cluster", cluster_id)
    }

    pub fn instance_group(instance_group_name: &str) -> Self {
        Self::of_kind("instance-group", instance_group_name)
    }

    pub fn deployment(deployment_name: &str) -> Self {
        Self::of_kind("deployment", deployment_name)
    }

    pub fn service(service_name: &str) -> Self {
        Self::of_kind("service", service_name)
    }

    pub fn orchestrator(node_id: &str, commit: &str) -> Self {
        Self::builder()
            .kind("orchestrator")
            .key(node_id)
            .attribute(ORCHESTRATOR_COMMIT_ATTRIBUTE, commit)
            .build()
    }

    pub fn sandbox_runtime(
        sandbox_id: &str,
        template_id: &str,
        kernel_version: &str,
        firecracker_version: &str,
        envd_version: &str,
        sandbox_type: &str,
    ) -> Self {
        Self::builder()
            .kind("sandbox")
            .key(sandbox_id)
            .attribute("template-id", template_id)
            .attribute("kernel-version", kernel_version)
            .attribute("firecracker-version", firecracker_version)
            .attribute("envd-version", envd_version)
            .attribute("sandbox-type", sandbox_type)
            .build()
    }

    pub fn kind(&self) -> &str {
        &self.kind
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    fn overridden_by(self, later: Self) -> Self {
        Self {
            kind: self.kind,
            key: later.key,
            name: later.name.or(self.name),
            attributes: self.attributes.into_iter().chain(later.attributes).collect(),
        }
    }

    fn to_launch_darkly(&self) -> Option<Context> {
        let mut builder = ContextBuilder::new(self.key.as_str());
        builder.kind(self.kind.as_str());
        if let Some(name) = &self.name {
            builder.name(name.as_str());
        }
        self.attributes.iter().for_each(|(name, value)| {
            builder.set_string(name, value.as_str());
        });
        builder.build().ok()
    }
}

pub fn merge_contexts(contexts: impl IntoIterator<Item = FlagContext>) -> Context {
    let by_kind = contexts.into_iter().fold(BTreeMap::new(), |mut by_kind, context| {
        let merged = match by_kind.remove(&context.kind) {
            Some(earlier) => FlagContext::overridden_by(earlier, context),
            None => context,
        };
        by_kind.insert(merged.kind.clone(), merged);
        by_kind
    });
    let valid: Vec<Context> = by_kind.values().filter_map(FlagContext::to_launch_darkly).collect();
    match valid.len() {
        0 => none_context(),
        _ => MultiContextBuilder::of(valid)
            .build()
            .unwrap_or_else(|_| none_context()),
    }
}

fn none_context() -> Context {
    ContextBuilder::new(NONE_KIND)
        .kind(NONE_KIND)
        .build()
        .expect("the none context is valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(context: &Context) -> Vec<(String, String)> {
        let mut keys: Vec<_> = context
            .context_keys()
            .into_iter()
            .map(|(kind, key)| (kind.to_string(), key.to_owned()))
            .collect();
        keys.sort();
        keys
    }

    fn pair(kind: &str, key: &str) -> (String, String) {
        (kind.to_owned(), key.to_owned())
    }

    #[test]
    fn no_valid_context_is_the_none_context() {
        assert_eq!(keys(&merge_contexts([])), vec![pair("none", "none")]);
        assert_eq!(
            keys(&merge_contexts([FlagContext::instance_group("")])),
            vec![pair("none", "none")]
        );
    }

    #[test]
    fn one_context_stays_single_and_several_become_a_multi_context() {
        let single = merge_contexts([FlagContext::template("t1")]);
        assert!(!single.is_multi());
        assert_eq!(keys(&single), vec![pair("template", "t1")]);

        let multi = merge_contexts([
            FlagContext::template("t1"),
            FlagContext::team("tm"),
            FlagContext::team(""),
        ]);
        assert!(!multi.is_multi());
        assert_eq!(keys(&multi), vec![pair("template", "t1")]);

        let multi = merge_contexts([FlagContext::template("t1"), FlagContext::team("tm")]);
        assert!(multi.is_multi());
        assert_eq!(keys(&multi), vec![pair("team", "tm"), pair("template", "t1")]);
    }

    #[test]
    fn a_later_context_of_the_same_kind_overrides_key_name_and_attributes() {
        let earlier = FlagContext::builder()
            .kind("team")
            .key("a")
            .name("first")
            .attribute("x", "1")
            .attribute("y", "1")
            .build();
        let later = FlagContext::builder().kind("team").key("b").attribute("y", "2").build();

        let merged = earlier.overridden_by(later);

        assert_eq!(
            merged,
            FlagContext::builder()
                .kind("team")
                .key("b")
                .name("first")
                .attribute("x", "1")
                .attribute("y", "2")
                .build()
        );
    }

    #[test]
    fn an_empty_key_in_the_later_context_drops_the_kind() {
        let merged = merge_contexts([FlagContext::team("a"), FlagContext::team("")]);

        assert_eq!(keys(&merged), vec![pair("none", "none")]);
    }

    #[test]
    fn the_orchestrator_context_carries_the_commit() {
        let context = FlagContext::orchestrator("node-1", "abc123");

        assert_eq!((context.kind(), context.key()), ("orchestrator", "node-1"));
        let built = context.to_launch_darkly().unwrap();
        let commit = launchdarkly_server_sdk::Reference::new("commit");
        assert_eq!(
            built.get_value(&commit),
            Some(launchdarkly_server_sdk::AttributeValue::String("abc123".to_owned()))
        );
    }
}
