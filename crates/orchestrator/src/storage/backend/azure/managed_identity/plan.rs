use reqwest::Method;

use super::source::{Environment, Identity, Source};
use crate::go_url::escape::{Mode, escape};
use crate::go_url::query::Query;

const ARC_ENDPOINT: &str = "http://127.0.0.1:40342/metadata/identity/oauth2/token";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(&'static str, String)>,
    pub body: Option<String>,
}

pub fn plan(source: Source, identity: &Identity, environment: &Environment, resource: &str) -> Planned {
    match source {
        Source::Imds => imds(identity, environment, resource),
        Source::AppService => app_service(identity, environment, resource),
        Source::AzureMl => azure_ml(identity, environment, resource),
        Source::CloudShell => cloud_shell(environment, resource),
        Source::ServiceFabric => service_fabric(environment, resource),
        Source::AzureArc => arc(environment, resource, None),
    }
}

pub fn arc(environment: &Environment, resource: &str, key: Option<&str>) -> Planned {
    let endpoint = match environment.identity_endpoint.is_empty() {
        true => ARC_ENDPOINT,
        false => &environment.identity_endpoint,
    };
    let mut headers = vec![("Metadata", "true".to_owned())];
    headers.extend(key.map(|key| ("Authorization", format!("Basic {key}"))));
    Planned {
        method: Method::GET,
        url: with_query(endpoint, &[("api-version", "2020-06-01"), ("resource", resource)]),
        headers,
        body: None,
    }
}

fn imds(identity: &Identity, environment: &Environment, resource: &str) -> Planned {
    let mut pairs = vec![("api-version", "2018-02-01"), ("resource", resource)];
    pairs.extend(identity_pair(identity, "client_id"));
    Planned {
        method: Method::GET,
        url: with_query(&environment.imds_url, &pairs),
        headers: vec![("Metadata", "true".to_owned())],
        body: None,
    }
}

fn app_service(identity: &Identity, environment: &Environment, resource: &str) -> Planned {
    let mut pairs = vec![("api-version", "2019-08-01"), ("resource", resource)];
    pairs.extend(identity_pair(identity, "client_id"));
    Planned {
        method: Method::GET,
        url: with_query(&environment.identity_endpoint, &pairs),
        headers: vec![("X-IDENTITY-HEADER", environment.identity_header.clone())],
        body: None,
    }
}

fn azure_ml(identity: &Identity, environment: &Environment, resource: &str) -> Planned {
    let client_id = match identity {
        Identity::ClientId(value) => value,
        _ => &environment.default_identity_client_id,
    };
    Planned {
        method: Method::GET,
        url: with_query(
            &environment.msi_endpoint,
            &[
                ("api-version", "2017-09-01"),
                ("resource", resource),
                ("clientid", client_id),
            ],
        ),
        headers: vec![("secret", environment.msi_secret.clone())],
        body: None,
    }
}

fn cloud_shell(environment: &Environment, resource: &str) -> Planned {
    Planned {
        method: Method::POST,
        url: environment.msi_endpoint.clone(),
        headers: vec![
            ("Metadata", "true".to_owned()),
            ("Content-Type", "application/x-www-form-urlencoded".to_owned()),
        ],
        body: Some(format!("resource={}", escape(resource, Mode::QueryComponent))),
    }
}

fn service_fabric(environment: &Environment, resource: &str) -> Planned {
    Planned {
        method: Method::GET,
        url: with_query(
            &environment.identity_endpoint,
            &[("api-version", "2019-07-01-preview"), ("resource", resource)],
        ),
        headers: vec![
            ("Accept", "application/json".to_owned()),
            ("Secret", environment.identity_header.clone()),
        ],
        body: None,
    }
}

fn identity_pair<'a>(identity: &'a Identity, client: &'static str) -> Option<(&'static str, &'a str)> {
    match identity {
        Identity::SystemAssigned => None,
        Identity::ClientId(value) => Some((client, value)),
    }
}

fn with_query(endpoint: &str, pairs: &[(&str, &str)]) -> String {
    let (base, existing) = endpoint.split_once('?').unwrap_or((endpoint, ""));
    let mut query = Query::parse(existing);
    for (key, value) in pairs {
        query.values.insert((*key).to_owned(), vec![(*value).to_owned()]);
    }
    format!("{base}?{}", query.encode())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn environment() -> Environment {
        Environment {
            identity_endpoint: "http://app/msi/token?existing=1".to_owned(),
            identity_header: "header-secret".to_owned(),
            identity_server_thumbprint: String::new(),
            msi_endpoint: "http://shell/token".to_owned(),
            msi_secret: "ml-secret".to_owned(),
            imds_endpoint: String::new(),
            default_identity_client_id: "default-client".to_owned(),
            himds_present: false,
            imds_url: "http://imds/token".to_owned(),
        }
    }

    const RESOURCE: &str = "https://storage.azure.com";
    const ENCODED: &str = "https%3A%2F%2Fstorage.azure.com";

    #[test]
    fn imds_requests_name_the_identity_with_msal_parameter_names() {
        let system = plan(Source::Imds, &Identity::SystemAssigned, &environment(), RESOURCE);
        let client = plan(
            Source::Imds,
            &Identity::ClientId("c".to_owned()),
            &environment(),
            RESOURCE,
        );

        assert_eq!(
            system.url,
            format!("http://imds/token?api-version=2018-02-01&resource={ENCODED}")
        );
        assert_eq!(system.headers, vec![("Metadata", "true".to_owned())]);
        assert!(client.url.contains("&client_id=c&"));
        assert_eq!(system.method, Method::GET);
    }

    #[test]
    fn app_service_requests_keep_existing_query_parameters_and_send_the_identity_header() {
        let planned = plan(
            Source::AppService,
            &Identity::ClientId("c".to_owned()),
            &environment(),
            RESOURCE,
        );

        assert_eq!(
            planned.url,
            format!("http://app/msi/token?api-version=2019-08-01&client_id=c&existing=1&resource={ENCODED}")
        );
        assert_eq!(planned.headers, vec![("X-IDENTITY-HEADER", "header-secret".to_owned())]);
    }

    #[test]
    fn azure_ml_requests_default_the_client_id_from_the_environment() {
        let default = plan(Source::AzureMl, &Identity::SystemAssigned, &environment(), RESOURCE);
        let chosen = plan(
            Source::AzureMl,
            &Identity::ClientId("mine".to_owned()),
            &environment(),
            RESOURCE,
        );

        assert_eq!(
            default.url,
            format!("http://shell/token?api-version=2017-09-01&clientid=default-client&resource={ENCODED}")
        );
        assert!(chosen.url.contains("clientid=mine"));
        assert_eq!(default.headers, vec![("secret", "ml-secret".to_owned())]);
    }

    #[test]
    fn cloud_shell_posts_the_resource_as_a_form() {
        let planned = plan(Source::CloudShell, &Identity::SystemAssigned, &environment(), RESOURCE);

        assert_eq!(planned.method, Method::POST);
        assert_eq!(planned.url, "http://shell/token");
        assert_eq!(planned.body.as_deref(), Some(format!("resource={ENCODED}").as_str()));
        assert!(
            planned
                .headers
                .contains(&("Content-Type", "application/x-www-form-urlencoded".to_owned()))
        );
    }

    #[test]
    fn service_fabric_sends_the_secret_header_and_json_accept() {
        let planned = plan(
            Source::ServiceFabric,
            &Identity::SystemAssigned,
            &environment(),
            RESOURCE,
        );

        assert_eq!(
            planned.url,
            format!("http://app/msi/token?api-version=2019-07-01-preview&existing=1&resource={ENCODED}")
        );
        assert_eq!(
            planned.headers,
            vec![
                ("Accept", "application/json".to_owned()),
                ("Secret", "header-secret".to_owned())
            ]
        );
    }

    #[test]
    fn arc_requests_default_to_the_local_agent_and_add_the_challenge_key_on_the_second_call() {
        let mut environment = environment();
        environment.identity_endpoint = String::new();

        let first = arc(&environment, RESOURCE, None);
        let second = arc(&environment, RESOURCE, Some("k3y"));

        assert_eq!(
            first.url,
            format!("http://127.0.0.1:40342/metadata/identity/oauth2/token?api-version=2020-06-01&resource={ENCODED}")
        );
        assert_eq!(first.headers, vec![("Metadata", "true".to_owned())]);
        assert!(second.headers.contains(&("Authorization", "Basic k3y".to_owned())));
    }
}
