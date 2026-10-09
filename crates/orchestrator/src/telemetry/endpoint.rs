#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    address: String,
}

impl Endpoint {
    pub fn from_env(variable: &str) -> Option<Self> {
        std::env::var(variable).ok().and_then(Self::parse)
    }

    pub fn parse(address: String) -> Option<Self> {
        Some(address)
            .filter(|address| !address.is_empty())
            .map(|address| Self { address })
    }

    pub fn url(&self) -> String {
        match self.address.contains("://") {
            true => self.address.clone(),
            false => format!("http://{}", self.address),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_endpoint_disables_telemetry() {
        assert_eq!(Endpoint::parse(String::new()), None);
    }

    #[test]
    fn a_bare_host_port_is_dialled_insecurely_like_upstream() {
        let endpoint = Endpoint::parse("localhost:4317".to_owned()).unwrap();

        assert_eq!(endpoint.url(), "http://localhost:4317");
    }

    #[test]
    fn a_full_url_is_kept() {
        let endpoint = Endpoint::parse("http://collector:4317".to_owned()).unwrap();

        assert_eq!(endpoint.url(), "http://collector:4317");
    }
}
