use super::ecr::{AwsCredentials, BasicCredentials, TokenError};
use crate::proto::FromImageRegistry;
use crate::proto::from_image_registry::Type;

const GCP_JSON_KEY_USER: &str = "_json_key";

pub async fn auth_option(registry: Option<&FromImageRegistry>) -> Result<Option<BasicCredentials>, String> {
    let Some(kind) = registry.and_then(|registry| registry.r#type.as_ref()) else {
        return Ok(None);
    };
    match kind {
        Type::Aws(aws) => AwsCredentials {
            access_key_id: aws.aws_access_key_id.clone(),
            secret_access_key: aws.aws_secret_access_key.clone(),
            session_token: None,
            region: aws.aws_region.clone(),
        }
        .ecr_basic_auth()
        .await
        .map(Some)
        .map_err(|e| format!("error getting auth option: {}", ecr_error(e))),
        Type::Gcp(gcp) => Ok(Some(basic(GCP_JSON_KEY_USER, &gcp.service_account_json))),
        Type::General(general) => Ok(Some(basic(&general.username, &general.password))),
    }
}

pub fn basic(username: &str, password: &str) -> BasicCredentials {
    BasicCredentials {
        username: username.to_owned(),
        password: password.to_owned(),
    }
}

fn ecr_error(error: TokenError) -> String {
    match error {
        TokenError::Request(e) => format!("failed to get ECR authorization token: {e}"),
        TokenError::Missing => "no ECR authorization data returned".to_owned(),
        TokenError::Decode(e) => format!("failed to decode ECR token: {e}"),
        TokenError::Format => "invalid ECR token format".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{GcpRegistry, GeneralRegistry};

    #[tokio::test]
    async fn gcp_and_general_use_basic_auth() {
        let gcp = FromImageRegistry {
            r#type: Some(Type::Gcp(GcpRegistry {
                service_account_json: "{\"k\":1}".to_owned(),
            })),
        };
        let general = FromImageRegistry {
            r#type: Some(Type::General(GeneralRegistry {
                username: String::new(),
                password: String::new(),
            })),
        };

        assert_eq!(
            auth_option(Some(&gcp)).await.unwrap(),
            Some(basic("_json_key", "{\"k\":1}"))
        );
        assert_eq!(auth_option(Some(&general)).await.unwrap(), Some(basic("", "")));
        assert_eq!(
            auth_option(Some(&FromImageRegistry { r#type: None })).await.unwrap(),
            None
        );
        assert_eq!(auth_option(None).await.unwrap(), None);
    }
}
