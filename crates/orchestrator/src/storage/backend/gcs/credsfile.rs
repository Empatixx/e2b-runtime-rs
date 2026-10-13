use super::json::{Field, Target};

const fn field(json: &'static str) -> Field {
    Field { json, nested: None }
}

const fn object(json: &'static str, target: &'static Target) -> Field {
    Field {
        json,
        nested: Some(target),
    }
}

pub static FILE_TYPE: Target = Target {
    fields: &[field("type")],
};

pub static SERVICE_ACCOUNT: Target = Target {
    fields: &[
        field("type"),
        field("project_id"),
        field("private_key_id"),
        field("private_key"),
        field("client_email"),
        field("client_id"),
        field("auth_uri"),
        field("token_uri"),
        field("universe_domain"),
    ],
};

pub static USER_CREDENTIALS: Target = Target {
    fields: &[
        field("type"),
        field("client_id"),
        field("client_secret"),
        field("quota_project_id"),
        field("refresh_token"),
        field("universe_domain"),
    ],
};

static FORMAT: Target = Target {
    fields: &[field("type"), field("subject_token_field_name")],
};

static EXECUTABLE: Target = Target {
    fields: &[field("command"), field("timeout_millis"), field("output_file")],
};

static CERTIFICATE: Target = Target {
    fields: &[
        field("use_default_certificate_config"),
        field("certificate_config_location"),
        field("trust_chain_path"),
    ],
};

static CREDENTIAL_SOURCE: Target = Target {
    fields: &[
        field("file"),
        field("url"),
        field("headers"),
        object("executable", &EXECUTABLE),
        object("certificate", &CERTIFICATE),
        field("environment_id"),
        field("region_url"),
        field("regional_cred_verification_url"),
        field("cred_verification_url"),
        field("imdsv2_session_token_url"),
        object("format", &FORMAT),
    ],
};

static IMPERSONATION_INFO: Target = Target {
    fields: &[field("token_lifetime_seconds")],
};

pub static EXTERNAL_ACCOUNT: Target = Target {
    fields: &[
        field("type"),
        field("client_id"),
        field("client_secret"),
        field("audience"),
        field("subject_token_type"),
        field("service_account_impersonation_url"),
        field("token_url"),
        object("credential_source", &CREDENTIAL_SOURCE),
        field("token_info_url"),
        object("service_account_impersonation", &IMPERSONATION_INFO),
        field("quota_project_id"),
        field("workforce_pool_user_project"),
        field("universe_domain"),
    ],
};

pub static EXTERNAL_ACCOUNT_AUTHORIZED_USER: Target = Target {
    fields: &[
        field("type"),
        field("audience"),
        field("client_id"),
        field("client_secret"),
        field("refresh_token"),
        field("token_url"),
        field("token_info_url"),
        field("revoke_url"),
        field("quota_project_id"),
        field("universe_domain"),
    ],
};

pub static IMPERSONATED: Target = Target {
    fields: &[
        field("type"),
        field("service_account_impersonation_url"),
        field("delegates"),
        field("scopes"),
        field("source_credentials"),
        field("universe_domain"),
    ],
};

pub static GDCH: Target = Target {
    fields: &[
        field("type"),
        field("format_version"),
        field("project"),
        field("name"),
        field("ca_cert_path"),
        field("private_key_id"),
        field("private_key"),
        field("token_uri"),
        field("universe_domain"),
    ],
};

pub static CLIENT_CREDENTIALS: Target = Target {
    fields: &[
        object("web", &CONFIG_3LO),
        object("installed", &CONFIG_3LO),
        field("universe_domain"),
    ],
};

static CONFIG_3LO: Target = Target {
    fields: &[
        field("client_id"),
        field("client_secret"),
        field("redirect_uris"),
        field("auth_uri"),
        field("token_uri"),
    ],
};

static OAUTH2_FORMAT: Target = Target {
    fields: &[field("type"), field("subject_token_field_name")],
};

static OAUTH2_EXECUTABLE: Target = Target {
    fields: &[field("command"), field("timeout_millis"), field("output_file")],
};

static OAUTH2_CREDENTIAL_SOURCE: Target = Target {
    fields: &[
        field("file"),
        field("url"),
        field("headers"),
        object("executable", &OAUTH2_EXECUTABLE),
        field("environment_id"),
        field("region_url"),
        field("regional_cred_verification_url"),
        field("imdsv2_session_token_url"),
        object("format", &OAUTH2_FORMAT),
    ],
};

static OAUTH2_IMPERSONATION: Target = Target {
    fields: &[field("token_lifetime_seconds")],
};

pub static OAUTH2_CREDENTIALS_FILE: Target = Target {
    fields: &[
        field("type"),
        field("client_email"),
        field("private_key_id"),
        field("private_key"),
        field("auth_uri"),
        field("token_uri"),
        field("project_id"),
        field("universe_domain"),
        field("client_secret"),
        field("client_id"),
        field("refresh_token"),
        field("audience"),
        field("subject_token_type"),
        field("token_url"),
        field("token_info_url"),
        field("service_account_impersonation_url"),
        object("service_account_impersonation", &OAUTH2_IMPERSONATION),
        field("delegates"),
        object("credential_source", &OAUTH2_CREDENTIAL_SOURCE),
        field("quota_project_id"),
        field("workforce_pool_user_project"),
        field("revoke_url"),
        object("source_credentials", &OAUTH2_CREDENTIALS_FILE),
    ],
};

pub static OAUTH2_JWT_RESPONSE: Target = Target {
    fields: &[
        field("access_token"),
        field("token_type"),
        field("refresh_token"),
        field("expires_in"),
        field("id_token"),
    ],
};

pub static AUTH_TOKEN_2LO: Target = Target {
    fields: &[
        field("access_token"),
        field("token_type"),
        field("id_token"),
        field("expires_in"),
    ],
};

pub static AUTH_TOKEN_3LO: Target = Target {
    fields: &[
        field("access_token"),
        field("token_type"),
        field("refresh_token"),
        field("expires_in"),
        field("error"),
        field("error_description"),
        field("error_uri"),
    ],
};

pub static OAUTH2_TOKEN: Target = Target {
    fields: &[
        field("access_token"),
        field("token_type"),
        field("refresh_token"),
        field("expires_in"),
        field("error"),
        field("error_description"),
        field("error_uri"),
    ],
};

pub static METADATA_TOKEN: Target = Target {
    fields: &[field("access_token"), field("expires_in"), field("token_type")],
};

pub static OAUTH2_METADATA_TOKEN: Target = Target {
    fields: &[
        field("access_token"),
        field("token_type"),
        field("refresh_token"),
        field("expires_in"),
    ],
};

pub static STS_RESPONSE: Target = Target {
    fields: &[
        field("access_token"),
        field("issued_token_type"),
        field("token_type"),
        field("expires_in"),
        field("scope"),
        field("refresh_token"),
    ],
};

pub static IMPERSONATION_RESPONSE: Target = Target {
    fields: &[field("accessToken"), field("expireTime")],
};

pub static OAUTH2_IMPERSONATION_RESPONSE: Target = Target {
    fields: &[field("accessToken"), field("expireTime")],
};

pub static ANY_OBJECT: Target = Target { fields: &[] };

pub static AWS_SECURITY_CREDENTIALS: Target = Target {
    fields: &[field("AccessKeyID"), field("SecretAccessKey"), field("Token")],
};

pub static EXECUTABLE_RESPONSE: Target = Target {
    fields: &[
        field("version"),
        field("success"),
        field("token_type"),
        field("expiration_time"),
        field("id_token"),
        field("saml_response"),
        field("code"),
        field("message"),
    ],
};

static WORKLOAD_SOURCE: Target = Target {
    fields: &[field("cert_path"), field("key_path")],
};

static CERT_CONFIGS: Target = Target {
    fields: &[object("workload", &WORKLOAD_SOURCE)],
};

pub static CERTIFICATE_CONFIG: Target = Target {
    fields: &[object("cert_configs", &CERT_CONFIGS)],
};

pub static SERVICE_ACCOUNT_TOKEN: Target = Target {
    fields: &[field("client_email"), field("private_key")],
};
