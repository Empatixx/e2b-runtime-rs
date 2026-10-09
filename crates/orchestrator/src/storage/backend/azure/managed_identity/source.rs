const HIMDS_PATH: &str = "/opt/azcmagent/bin/himds";
pub const IMDS_ENDPOINT: &str = "http://169.254.169.254/metadata/identity/oauth2/token";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Imds,
    AzureArc,
    ServiceFabric,
    CloudShell,
    AzureMl,
    AppService,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identity {
    SystemAssigned,
    ClientId(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Environment {
    pub identity_endpoint: String,
    pub identity_header: String,
    pub identity_server_thumbprint: String,
    pub msi_endpoint: String,
    pub msi_secret: String,
    pub imds_endpoint: String,
    pub default_identity_client_id: String,
    pub himds_present: bool,
    pub imds_url: String,
}

impl Environment {
    pub fn read(env: &dyn Fn(&str) -> Option<String>) -> Self {
        let value = |name: &str| env(name).unwrap_or_default();
        Self {
            identity_endpoint: value("IDENTITY_ENDPOINT"),
            identity_header: value("IDENTITY_HEADER"),
            identity_server_thumbprint: value("IDENTITY_SERVER_THUMBPRINT"),
            msi_endpoint: value("MSI_ENDPOINT"),
            msi_secret: value("MSI_SECRET"),
            imds_endpoint: value("IMDS_ENDPOINT"),
            default_identity_client_id: value("DEFAULT_IDENTITY_CLIENT_ID"),
            himds_present: cfg!(target_os = "linux") && std::path::Path::new(HIMDS_PATH).exists(),
            imds_url: IMDS_ENDPOINT.to_owned(),
        }
    }

    pub fn source(&self) -> Source {
        let set = |value: &str| !value.is_empty();
        match (
            set(&self.identity_endpoint) && set(&self.identity_header),
            set(&self.msi_endpoint),
            (set(&self.identity_endpoint) && set(&self.imds_endpoint)) || self.himds_present,
        ) {
            (true, _, _) if set(&self.identity_server_thumbprint) => Source::ServiceFabric,
            (true, _, _) => Source::AppService,
            (false, true, _) if set(&self.msi_secret) => Source::AzureMl,
            (false, true, _) => Source::CloudShell,
            (false, false, true) => Source::AzureArc,
            (false, false, false) => Source::Imds,
        }
    }
}

impl Identity {
    pub fn from_client_id_variable(value: Option<String>) -> Self {
        value.map_or(Self::SystemAssigned, Self::ClientId)
    }

    pub fn is_user_assigned(&self) -> bool {
        *self != Self::SystemAssigned
    }

    pub fn validate_for(&self, source: Source) -> Result<(), String> {
        match (source, self) {
            (_, Self::SystemAssigned) => Ok(()),
            (Source::AzureArc, _) => Err("Azure Arc doesn't support user-assigned managed identities".to_owned()),
            (Source::CloudShell, _) => Err("Cloud Shell doesn't support user-assigned managed identities".to_owned()),
            (Source::ServiceFabric, _) => Err("Service Fabric API doesn't support specifying a user-assigned identity. The identity is determined by cluster resource configuration. See https://aka.ms/servicefabricmi".to_owned()),
            (_, Self::ClientId(value)) if value.is_empty() => {
                Err("empty managedidentity.UserAssignedClientID".to_owned())
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn environment(pairs: &[(&str, &str)]) -> Environment {
        Environment::read(&|name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        })
    }

    fn source(pairs: &[(&str, &str)]) -> Source {
        Environment {
            himds_present: false,
            ..environment(pairs)
        }
        .source()
    }

    #[test]
    fn the_source_follows_msal_get_source() {
        assert_eq!(source(&[]), Source::Imds);
        assert_eq!(
            source(&[("IDENTITY_ENDPOINT", "e"), ("IDENTITY_HEADER", "h")]),
            Source::AppService
        );
        assert_eq!(
            source(&[
                ("IDENTITY_ENDPOINT", "e"),
                ("IDENTITY_HEADER", "h"),
                ("IDENTITY_SERVER_THUMBPRINT", "t")
            ]),
            Source::ServiceFabric
        );
        assert_eq!(source(&[("MSI_ENDPOINT", "m")]), Source::CloudShell);
        assert_eq!(source(&[("MSI_ENDPOINT", "m"), ("MSI_SECRET", "s")]), Source::AzureMl);
        assert_eq!(
            source(&[("IDENTITY_ENDPOINT", "e"), ("IMDS_ENDPOINT", "i")]),
            Source::AzureArc
        );
        assert_eq!(source(&[("IDENTITY_ENDPOINT", "e")]), Source::Imds);
        assert_eq!(source(&[("IDENTITY_HEADER", "h")]), Source::Imds);
    }

    #[test]
    fn app_service_variables_win_over_cloud_shell_and_arc_variables() {
        assert_eq!(
            source(&[
                ("IDENTITY_ENDPOINT", "e"),
                ("IDENTITY_HEADER", "h"),
                ("MSI_ENDPOINT", "m"),
                ("IMDS_ENDPOINT", "i")
            ]),
            Source::AppService
        );
        assert_eq!(
            source(&[
                ("MSI_ENDPOINT", "m"),
                ("IDENTITY_ENDPOINT", "e"),
                ("IMDS_ENDPOINT", "i")
            ]),
            Source::CloudShell
        );
    }

    #[test]
    fn the_arc_agent_binary_selects_arc() {
        let environment = Environment {
            himds_present: true,
            ..environment(&[])
        };

        assert_eq!(environment.source(), Source::AzureArc);
    }

    #[test]
    fn user_assigned_identities_are_checked_per_source_with_msal_texts() {
        let client = Identity::ClientId("c".to_owned());

        assert!(client.validate_for(Source::Imds).is_ok());
        assert!(client.validate_for(Source::AppService).is_ok());
        assert!(client.validate_for(Source::AzureMl).is_ok());
        assert!(Identity::SystemAssigned.validate_for(Source::CloudShell).is_ok());
        assert_eq!(
            client.validate_for(Source::AzureArc).unwrap_err(),
            "Azure Arc doesn't support user-assigned managed identities"
        );
        assert_eq!(
            client.validate_for(Source::CloudShell).unwrap_err(),
            "Cloud Shell doesn't support user-assigned managed identities"
        );
        assert!(
            client
                .validate_for(Source::ServiceFabric)
                .unwrap_err()
                .starts_with("Service Fabric API doesn't support")
        );
        assert_eq!(
            Identity::ClientId(String::new())
                .validate_for(Source::Imds)
                .unwrap_err(),
            "empty managedidentity.UserAssignedClientID"
        );
    }
}
