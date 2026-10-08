use crate::proto::ServiceInfoRole;

const DEFAULT_SERVICES: &str = "orchestrator";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceType {
    Orchestrator,
    TemplateManager,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Services {
    pub types: Vec<ServiceType>,
}

impl ServiceType {
    fn parse(name: &str) -> Option<Self> {
        match name.trim().to_lowercase().as_str() {
            "orchestrator" => Some(ServiceType::Orchestrator),
            "template-manager" => Some(ServiceType::TemplateManager),
            _ => None,
        }
    }

    fn role(&self) -> ServiceInfoRole {
        match self {
            ServiceType::Orchestrator => ServiceInfoRole::Orchestrator,
            ServiceType::TemplateManager => ServiceInfoRole::TemplateBuilder,
        }
    }
}

impl Services {
    pub fn parse(names: Option<&str>) -> Self {
        Self {
            types: names
                .unwrap_or(DEFAULT_SERVICES)
                .split(',')
                .filter_map(ServiceType::parse)
                .collect(),
        }
    }

    pub fn runs_template_manager(&self) -> bool {
        self.types.contains(&ServiceType::TemplateManager)
    }

    pub fn roles(&self) -> Vec<ServiceInfoRole> {
        self.types.iter().map(ServiceType::role).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn services_parse_like_upstream() {
        assert_eq!(Services::parse(None).types, vec![ServiceType::Orchestrator]);
        let both = Services::parse(Some(" Orchestrator,template-manager,bogus"));
        assert_eq!(
            both.types,
            vec![ServiceType::Orchestrator, ServiceType::TemplateManager]
        );
        assert!(both.runs_template_manager());
        assert_eq!(
            both.roles(),
            vec![ServiceInfoRole::Orchestrator, ServiceInfoRole::TemplateBuilder]
        );
        assert!(!Services::parse(Some("orchestrator")).runs_template_manager());
    }
}
