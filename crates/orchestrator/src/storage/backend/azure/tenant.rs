const WILDCARD: &str = "*";
const ADDITIONAL_TENANTS_ENV: &str = "AZURE_ADDITIONALLY_ALLOWED_TENANTS";
pub const INVALID_TENANT: &str = "invalid tenantID. You can locate your tenantID by following the instructions listed here: https://learn.microsoft.com/partner-center/find-ids-and-domain-names";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantPolicy {
    default: String,
    additional: Vec<String>,
}

pub fn valid_tenant(tenant: &str) -> Result<(), String> {
    match !tenant.is_empty()
        && tenant
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
    {
        true => Ok(()),
        false => Err(INVALID_TENANT.to_owned()),
    }
}

impl TenantPolicy {
    pub fn new(default: &str, additional: &[String]) -> Self {
        let additional = match additional.iter().any(|tenant| tenant == WILDCARD) {
            true => vec![WILDCARD.to_owned()],
            false => additional.to_vec(),
        };
        Self {
            default: default.to_owned(),
            additional,
        }
    }

    pub fn default_tenant(&self) -> &str {
        &self.default
    }

    pub fn from_environment(default: &str, env: &dyn Fn(&str) -> Option<String>) -> Self {
        let additional: Vec<String> = env(ADDITIONAL_TENANTS_ENV)
            .filter(|tenants| !tenants.is_empty())
            .map(|tenants| tenants.split(';').map(str::to_owned).collect())
            .unwrap_or_default();
        Self::new(default, &additional)
    }

    pub fn resolve(&self, credential: &str, specified: &str) -> Result<String, String> {
        if specified.is_empty() || specified == self.default {
            return Ok(self.default.clone());
        }
        if self.default == "adfs" {
            return Err("ADFS doesn't support tenants".to_owned());
        }
        valid_tenant(specified)?;
        match (
            self.allows(specified),
            self.additional.is_empty(),
            self.default.as_str(),
        ) {
            (true, _, _) | (false, true, "" | "organizations") => Ok(specified.to_owned()),
            _ => Err(format!(
                "{credential} isn't configured to acquire tokens for tenant {}. To enable acquiring tokens for this tenant add it to the AdditionallyAllowedTenants on the credential options, or add \"*\" to allow acquiring tokens for any tenant",
                crate::go_strconv::quote(specified)
            )),
        }
    }

    fn allows(&self, tenant: &str) -> bool {
        self.additional
            .iter()
            .any(|allowed| allowed == WILDCARD || allowed == tenant)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(default: &str, additional: &[&str]) -> TenantPolicy {
        TenantPolicy::new(default, &additional.iter().map(|t| (*t).to_owned()).collect::<Vec<_>>())
    }

    #[test]
    fn unspecified_and_default_tenants_resolve_to_the_default() {
        assert_eq!(policy("home", &[]).resolve("C", "").unwrap(), "home");
        assert_eq!(policy("home", &[]).resolve("C", "home").unwrap(), "home");
    }

    #[test]
    fn other_tenants_need_the_allow_list_or_a_wildcard() {
        let error = policy("home", &[])
            .resolve("ClientSecretCredential", "other")
            .unwrap_err();

        assert_eq!(
            error,
            "ClientSecretCredential isn't configured to acquire tokens for tenant \"other\". To enable acquiring tokens for this tenant add it to the AdditionallyAllowedTenants on the credential options, or add \"*\" to allow acquiring tokens for any tenant"
        );
        assert_eq!(policy("home", &["other"]).resolve("C", "other").unwrap(), "other");
        assert_eq!(policy("home", &["a", "*"]).resolve("C", "zzz").unwrap(), "zzz");
        assert!(policy("home", &["a"]).resolve("C", "other").is_err());
    }

    #[test]
    fn credentials_without_a_tenant_accept_any_valid_tenant() {
        assert_eq!(policy("", &[]).resolve("C", "x").unwrap(), "x");
        assert_eq!(policy("organizations", &[]).resolve("C", "x").unwrap(), "x");
        assert!(policy("organizations", &["a"]).resolve("C", "x").is_err());
    }

    #[test]
    fn adfs_and_malformed_tenants_are_rejected_first() {
        assert_eq!(
            policy("adfs", &["*"]).resolve("C", "x").unwrap_err(),
            "ADFS doesn't support tenants"
        );
        assert_eq!(policy("home", &["*"]).resolve("C", "a b").unwrap_err(), INVALID_TENANT);
    }

    #[test]
    fn the_environment_list_is_semicolon_separated() {
        let env = |name: &str| (name == "AZURE_ADDITIONALLY_ALLOWED_TENANTS").then(|| "a;b".to_owned());

        let policy = TenantPolicy::from_environment("home", &env);

        assert_eq!(policy.resolve("C", "b").unwrap(), "b");
        assert!(policy.resolve("C", "c").is_err());
    }
}
