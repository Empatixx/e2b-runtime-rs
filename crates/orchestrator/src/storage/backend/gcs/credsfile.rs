use super::go_json::{Field, Kind, Target};

const fn text(json: &'static str) -> Field {
    Field { json, kind: Kind::Text }
}

const fn object(json: &'static str, target: &'static Target) -> Field {
    Field {
        json,
        kind: Kind::Object(target),
    }
}

const fn number(json: &'static str, go_type: &'static str) -> Field {
    Field {
        json,
        kind: Kind::Integer(go_type),
    }
}

const fn of(json: &'static str, kind: Kind) -> Field {
    Field { json, kind }
}

pub static FILE_TYPE: Target = Target {
    go_type: "struct { Type string \"json:\\\"type\\\"\" }",
    struct_name: "",
    fields: &[text("type")],
};

pub static SERVICE_ACCOUNT: Target = Target {
    go_type: "credsfile.ServiceAccountFile",
    struct_name: "ServiceAccountFile",
    fields: &[
        text("type"),
        text("project_id"),
        text("private_key_id"),
        text("private_key"),
        text("client_email"),
        text("client_id"),
        text("auth_uri"),
        text("token_uri"),
        text("universe_domain"),
    ],
};

pub static USER_CREDENTIALS: Target = Target {
    go_type: "credsfile.UserCredentialsFile",
    struct_name: "UserCredentialsFile",
    fields: &[
        text("type"),
        text("client_id"),
        text("client_secret"),
        text("quota_project_id"),
        text("refresh_token"),
        text("universe_domain"),
    ],
};

static FORMAT: Target = Target {
    go_type: "credsfile.Format",
    struct_name: "Format",
    fields: &[text("type"), text("subject_token_field_name")],
};

static EXECUTABLE: Target = Target {
    go_type: "credsfile.ExecutableConfig",
    struct_name: "ExecutableConfig",
    fields: &[text("command"), number("timeout_millis", "int"), text("output_file")],
};

static CERTIFICATE: Target = Target {
    go_type: "credsfile.CertificateConfig",
    struct_name: "CertificateConfig",
    fields: &[
        of("use_default_certificate_config", Kind::Flag),
        text("certificate_config_location"),
        text("trust_chain_path"),
    ],
};

static CREDENTIAL_SOURCE: Target = Target {
    go_type: "credsfile.CredentialSource",
    struct_name: "CredentialSource",
    fields: &[
        text("file"),
        text("url"),
        of("headers", Kind::TextMap),
        object("executable", &EXECUTABLE),
        object("certificate", &CERTIFICATE),
        text("environment_id"),
        text("region_url"),
        text("regional_cred_verification_url"),
        text("cred_verification_url"),
        text("imdsv2_session_token_url"),
        object("format", &FORMAT),
    ],
};

static IMPERSONATION_INFO: Target = Target {
    go_type: "credsfile.ServiceAccountImpersonationInfo",
    struct_name: "ServiceAccountImpersonationInfo",
    fields: &[number("token_lifetime_seconds", "int")],
};

pub static EXTERNAL_ACCOUNT: Target = Target {
    go_type: "credsfile.ExternalAccountFile",
    struct_name: "ExternalAccountFile",
    fields: &[
        text("type"),
        text("client_id"),
        text("client_secret"),
        text("audience"),
        text("subject_token_type"),
        text("service_account_impersonation_url"),
        text("token_url"),
        object("credential_source", &CREDENTIAL_SOURCE),
        text("token_info_url"),
        object("service_account_impersonation", &IMPERSONATION_INFO),
        text("quota_project_id"),
        text("workforce_pool_user_project"),
        text("universe_domain"),
    ],
};

pub static EXTERNAL_ACCOUNT_AUTHORIZED_USER: Target = Target {
    go_type: "credsfile.ExternalAccountAuthorizedUserFile",
    struct_name: "ExternalAccountAuthorizedUserFile",
    fields: &[
        text("type"),
        text("audience"),
        text("client_id"),
        text("client_secret"),
        text("refresh_token"),
        text("token_url"),
        text("token_info_url"),
        text("revoke_url"),
        text("quota_project_id"),
        text("universe_domain"),
    ],
};

pub static IMPERSONATED: Target = Target {
    go_type: "credsfile.ImpersonatedServiceAccountFile",
    struct_name: "ImpersonatedServiceAccountFile",
    fields: &[
        text("type"),
        text("service_account_impersonation_url"),
        of("delegates", Kind::TextList),
        of("scopes", Kind::TextList),
        of("source_credentials", Kind::Any),
        text("universe_domain"),
    ],
};

pub static GDCH: Target = Target {
    go_type: "credsfile.GDCHServiceAccountFile",
    struct_name: "GDCHServiceAccountFile",
    fields: &[
        text("type"),
        text("format_version"),
        text("project"),
        text("name"),
        text("ca_cert_path"),
        text("private_key_id"),
        text("private_key"),
        text("token_uri"),
        text("universe_domain"),
    ],
};

pub static CLIENT_CREDENTIALS: Target = Target {
    go_type: "credsfile.ClientCredentialsFile",
    struct_name: "ClientCredentialsFile",
    fields: &[
        object("web", &CONFIG_3LO),
        object("installed", &CONFIG_3LO),
        text("universe_domain"),
    ],
};

static CONFIG_3LO: Target = Target {
    go_type: "credsfile.Config3LO",
    struct_name: "Config3LO",
    fields: &[
        text("client_id"),
        text("client_secret"),
        of("redirect_uris", Kind::TextList),
        text("auth_uri"),
        text("token_uri"),
    ],
};

static OAUTH2_FORMAT: Target = Target {
    go_type: "externalaccount.Format",
    struct_name: "Format",
    fields: &[text("type"), text("subject_token_field_name")],
};

static OAUTH2_EXECUTABLE: Target = Target {
    go_type: "externalaccount.ExecutableConfig",
    struct_name: "ExecutableConfig",
    fields: &[text("command"), number("timeout_millis", "int"), text("output_file")],
};

static OAUTH2_CREDENTIAL_SOURCE: Target = Target {
    go_type: "externalaccount.CredentialSource",
    struct_name: "CredentialSource",
    fields: &[
        text("file"),
        text("url"),
        of("headers", Kind::TextMap),
        object("executable", &OAUTH2_EXECUTABLE),
        text("environment_id"),
        text("region_url"),
        text("regional_cred_verification_url"),
        text("imdsv2_session_token_url"),
        object("format", &OAUTH2_FORMAT),
    ],
};

static OAUTH2_IMPERSONATION: Target = Target {
    go_type: "google.serviceAccountImpersonationInfo",
    struct_name: "serviceAccountImpersonationInfo",
    fields: &[number("token_lifetime_seconds", "int")],
};

pub static OAUTH2_CREDENTIALS_FILE: Target = Target {
    go_type: "google.credentialsFile",
    struct_name: "credentialsFile",
    fields: &[
        text("type"),
        text("client_email"),
        text("private_key_id"),
        text("private_key"),
        text("auth_uri"),
        text("token_uri"),
        text("project_id"),
        text("universe_domain"),
        text("client_secret"),
        text("client_id"),
        text("refresh_token"),
        text("audience"),
        text("subject_token_type"),
        text("token_url"),
        text("token_info_url"),
        text("service_account_impersonation_url"),
        object("service_account_impersonation", &OAUTH2_IMPERSONATION),
        of("delegates", Kind::TextList),
        object("credential_source", &OAUTH2_CREDENTIAL_SOURCE),
        text("quota_project_id"),
        text("workforce_pool_user_project"),
        text("revoke_url"),
        object("source_credentials", &OAUTH2_CREDENTIALS_FILE),
    ],
};

pub static OAUTH2_JWT_RESPONSE: Target = Target {
    go_type: "struct { oauth2.Token; IDToken string \"json:\\\"id_token\\\"\" }",
    struct_name: "",
    fields: &[
        text("access_token"),
        text("token_type"),
        text("refresh_token"),
        number("expires_in", "int64"),
        text("id_token"),
    ],
};

pub static AUTH_TOKEN_2LO: Target = Target {
    go_type: "struct { AccessToken string \"json:\\\"access_token\\\"\"; TokenType string \"json:\\\"token_type\\\"\"; IDToken string \"json:\\\"id_token\\\"\"; ExpiresIn int64 \"json:\\\"expires_in\\\"\" }",
    struct_name: "",
    fields: &[
        text("access_token"),
        text("token_type"),
        text("id_token"),
        number("expires_in", "int64"),
    ],
};

pub static AUTH_TOKEN_3LO: Target = Target {
    go_type: "auth.tokenJSON",
    struct_name: "tokenJSON",
    fields: &[
        text("access_token"),
        text("token_type"),
        text("refresh_token"),
        number("expires_in", "int"),
        text("error"),
        text("error_description"),
        text("error_uri"),
    ],
};

pub static OAUTH2_TOKEN: Target = Target {
    go_type: "internal.tokenJSON",
    struct_name: "tokenJSON",
    fields: &[
        text("access_token"),
        text("token_type"),
        text("refresh_token"),
        of("expires_in", Kind::Seconds),
        text("error"),
        text("error_description"),
        text("error_uri"),
    ],
};

pub static METADATA_TOKEN: Target = Target {
    go_type: "credentials.metadataTokenResp",
    struct_name: "metadataTokenResp",
    fields: &[text("access_token"), number("expires_in", "int"), text("token_type")],
};

pub static OAUTH2_METADATA_TOKEN: Target = Target {
    go_type: "oauth2.Token",
    struct_name: "Token",
    fields: &[
        text("access_token"),
        text("token_type"),
        text("refresh_token"),
        number("expires_in", "int64"),
    ],
};

pub static STS_RESPONSE: Target = Target {
    go_type: "stsexchange.TokenResponse",
    struct_name: "TokenResponse",
    fields: &[
        text("access_token"),
        text("issued_token_type"),
        text("token_type"),
        number("expires_in", "int"),
        text("scope"),
        text("refresh_token"),
    ],
};

pub static IMPERSONATION_RESPONSE: Target = Target {
    go_type: "impersonate.impersonateTokenResponse",
    struct_name: "impersonateTokenResponse",
    fields: &[text("accessToken"), text("expireTime")],
};

pub static OAUTH2_IMPERSONATION_RESPONSE: Target = Target {
    go_type: "impersonate.impersonateTokenResponse",
    struct_name: "impersonateTokenResponse",
    fields: &[text("accessToken"), text("expireTime")],
};

pub static ANY_OBJECT: Target = Target {
    go_type: "map[string]interface {}",
    struct_name: "",
    fields: &[],
};

pub static AWS_SECURITY_CREDENTIALS: Target = Target {
    go_type: "externalaccount.AwsSecurityCredentials",
    struct_name: "AwsSecurityCredentials",
    fields: &[text("AccessKeyID"), text("SecretAccessKey"), text("Token")],
};

pub static EXECUTABLE_RESPONSE: Target = Target {
    go_type: "externalaccount.executableResponse",
    struct_name: "executableResponse",
    fields: &[
        number("version", "int"),
        of("success", Kind::Flag),
        text("token_type"),
        number("expiration_time", "int64"),
        text("id_token"),
        text("saml_response"),
        text("code"),
        text("message"),
    ],
};

static WORKLOAD_SOURCE: Target = Target {
    go_type: "cert.workloadSource",
    struct_name: "workloadSource",
    fields: &[text("cert_path"), text("key_path")],
};

static CERT_CONFIGS: Target = Target {
    go_type: "cert.certConfigs",
    struct_name: "certConfigs",
    fields: &[object("workload", &WORKLOAD_SOURCE)],
};

pub static CERTIFICATE_CONFIG: Target = Target {
    go_type: "cert.certificateConfig",
    struct_name: "certificateConfig",
    fields: &[object("cert_configs", &CERT_CONFIGS)],
};

pub static SERVICE_ACCOUNT_TOKEN: Target = Target {
    go_type: "storage.gcpServiceToken",
    struct_name: "gcpServiceToken",
    fields: &[text("client_email"), text("private_key")],
};
