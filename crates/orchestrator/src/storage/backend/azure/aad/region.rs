use std::time::Duration;

use reqwest::Client;

const IMDS_LOCATION: &str =
    "http://169.254.169.254/metadata/instance/compute/location?format=text&api-version=2021-10-01";
const AUTO_DETECT: &str = "TryAutoDetect";
const DISABLE_FORCE: &str = "DisableMsalForceRegion";
const REGION_ENV: &str = "REGION_NAME";
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Region {
    Global,
    AutoDetect,
    Named(String),
}

impl Region {
    pub fn configured(requested: &str, forced: &str) -> Self {
        let value = match requested.is_empty() {
            true => forced,
            false => requested,
        };
        match value {
            "" => Self::Global,
            value if value.eq_ignore_ascii_case(DISABLE_FORCE) => Self::Global,
            AUTO_DETECT => Self::AutoDetect,
            value => Self::Named(value.to_owned()),
        }
    }

    pub fn is_global(&self) -> bool {
        *self == Self::Global
    }

    pub async fn resolved(&self, client: &Client, probe_url: Option<&str>) -> Option<String> {
        match self {
            Self::Global => None,
            Self::Named(name) => Some(name.clone()),
            Self::AutoDetect => detect(client, probe_url.unwrap_or(IMDS_LOCATION)).await,
        }
    }
}

async fn detect(client: &Client, url: &str) -> Option<String> {
    match std::env::var(REGION_ENV).ok().filter(|region| !region.is_empty()) {
        Some(region) => Some(region.replace(' ', "").to_lowercase()),
        None => location(client, url).await,
    }
    .filter(|region| !region.is_empty())
}

async fn location(client: &Client, url: &str) -> Option<String> {
    for _ in 0..2 {
        let response = client
            .get(url)
            .header("Metadata", "true")
            .timeout(PROBE_TIMEOUT)
            .send()
            .await
            .ok()
            .filter(|response| response.status() == reqwest::StatusCode::OK);
        if let Some(response) = response {
            return response.text().await.ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::super::super::testing::serve;
    use super::*;

    #[test]
    fn the_requested_region_beats_the_forced_one() {
        assert_eq!(Region::configured("", ""), Region::Global);
        assert_eq!(
            Region::configured("westus2", "eastus"),
            Region::Named("westus2".to_owned())
        );
        assert_eq!(Region::configured("", "eastus"), Region::Named("eastus".to_owned()));
        assert_eq!(Region::configured("TryAutoDetect", ""), Region::AutoDetect);
        assert_eq!(
            Region::configured("tryautodetect", ""),
            Region::Named("tryautodetect".to_owned())
        );
        assert_eq!(Region::configured("disablemsalforceregion", "eastus"), Region::Global);
        assert_eq!(Region::configured("", "DisableMsalForceRegion"), Region::Global);
    }

    #[tokio::test]
    async fn auto_detection_retries_the_location_probe_once() {
        let (base, handle) = serve(vec![
            (500, "text/plain", String::new()),
            (200, "text/plain", "westus2".to_owned()),
        ])
        .await;

        let region = Region::AutoDetect.resolved(&Client::new(), Some(&base)).await;

        assert_eq!(region.as_deref(), Some("westus2"));
        let requests = handle.await.unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].contains("metadata: true"));
    }

    #[tokio::test]
    async fn a_probe_that_fails_twice_leaves_the_global_endpoint() {
        let (base, _handle) = serve(vec![
            (404, "text/plain", String::new()),
            (404, "text/plain", String::new()),
        ])
        .await;

        assert_eq!(Region::AutoDetect.resolved(&Client::new(), Some(&base)).await, None);
    }
}
