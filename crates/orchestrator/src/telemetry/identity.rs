use opentelemetry::{Array, KeyValue, StringValue, Value};
use opentelemetry_sdk::Resource;

use crate::server::services::{ServiceType, Services};

const UNKNOWN_SERVICE: &str = "orch-unknown";

const KERNEL_RELEASE_PATH: &str = "/proc/sys/kernel/osrelease";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub node_id: String,
    pub service_name: String,
    pub service_commit: String,
    pub service_version: String,
    pub service_instance_id: String,
    pub node_labels: Vec<String>,
}

impl Identity {
    pub fn resource(&self) -> Resource {
        Resource::builder_empty().with_attributes(self.attributes()).build()
    }

    pub fn attributes(&self) -> Vec<KeyValue> {
        let mut attributes = vec![
            KeyValue::new("service.name", self.service_name.clone()),
            KeyValue::new("service.version", self.service_version.clone()),
            KeyValue::new("service.instance.id", self.service_instance_id.clone()),
            KeyValue::new("telemetry.sdk.name", "otel"),
            KeyValue::new("host.id", self.node_id.clone()),
            KeyValue::new("telemetry.sdk.language", "rust"),
        ];
        attributes.extend(
            Some(&self.service_commit)
                .filter(|commit| !commit.is_empty())
                .map(|commit| KeyValue::new("service.commit", commit.clone())),
        );
        attributes.push(KeyValue::new("host.labels", string_array(&self.node_labels)));
        attributes.extend(hostname().map(|hostname| KeyValue::new("host.name", hostname)));
        attributes.extend(kernel_version().map(|kernel| KeyValue::new("host.kernel.version", kernel)));
        attributes
    }
}

pub fn service_name(services: &Services) -> String {
    match services.types.is_empty() {
        true => UNKNOWN_SERVICE.to_owned(),
        false => services
            .types
            .iter()
            .map(|service| match service {
                ServiceType::Orchestrator => "orchestrator",
                ServiceType::TemplateManager => "template-manager",
            })
            .collect::<Vec<_>>()
            .join("_"),
    }
}

fn string_array(values: &[String]) -> Value {
    Value::Array(Array::String(values.iter().cloned().map(StringValue::from).collect()))
}

fn hostname() -> Option<String> {
    sysinfo::System::host_name().filter(|name| !name.is_empty())
}

fn kernel_version() -> Option<String> {
    std::fs::read_to_string(KERNEL_RELEASE_PATH)
        .ok()
        .map(|release| release.trim().to_owned())
        .filter(|release| !release.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(commit: &str) -> Identity {
        Identity {
            node_id: "node-1".to_owned(),
            service_name: "orchestrator".to_owned(),
            service_commit: commit.to_owned(),
            service_version: "0.1.0".to_owned(),
            service_instance_id: "instance".to_owned(),
            node_labels: vec!["gpu".to_owned()],
        }
    }

    fn value_of(attributes: &[KeyValue], key: &str) -> Option<String> {
        attributes
            .iter()
            .find(|attribute| attribute.key.as_str() == key)
            .map(|attribute| attribute.value.as_str().into_owned())
    }

    #[test]
    fn the_resource_carries_the_service_and_host_attributes_of_upstream() {
        let attributes = identity("abc").attributes();

        assert_eq!(value_of(&attributes, "service.name").as_deref(), Some("orchestrator"));
        assert_eq!(value_of(&attributes, "service.version").as_deref(), Some("0.1.0"));
        assert_eq!(
            value_of(&attributes, "service.instance.id").as_deref(),
            Some("instance")
        );
        assert_eq!(value_of(&attributes, "host.id").as_deref(), Some("node-1"));
        assert_eq!(value_of(&attributes, "service.commit").as_deref(), Some("abc"));
        assert_eq!(value_of(&attributes, "telemetry.sdk.name").as_deref(), Some("otel"));
    }

    #[test]
    fn the_service_name_joins_the_services_with_underscores() {
        assert_eq!(
            service_name(&Services::parse(Some("orchestrator,template-manager"))),
            "orchestrator_template-manager"
        );
        assert_eq!(service_name(&Services::parse(Some("bogus"))), "orch-unknown");
    }

    #[test]
    fn an_empty_commit_is_not_reported() {
        assert_eq!(value_of(&identity("").attributes(), "service.commit"), None);
    }
}
