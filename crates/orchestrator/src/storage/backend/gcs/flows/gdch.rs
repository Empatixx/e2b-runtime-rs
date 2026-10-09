use std::sync::Arc;

use serde::Deserialize;

use super::TokenFlow;

const SUPPORTED_FORMAT_VERSION: &str = "1";
const MISSING_AUDIENCE: &str = "credentials: STSAudience must be set for the GDCH auth flows";

#[derive(Deserialize, Default)]
#[serde(default)]
struct GdchFile {
    format_version: String,
}

pub fn from_json(json: &[u8]) -> Result<Arc<dyn TokenFlow>, String> {
    let file: GdchFile = serde_json::from_slice(json).map_err(|e| e.to_string())?;
    match file.format_version == SUPPORTED_FORMAT_VERSION {
        false => Err(format!(
            "credentials: unsupported gdch_service_account format {}",
            crate::go_strconv::quote(&file.format_version)
        )),
        true => Err(MISSING_AUDIENCE.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_storage_client_never_sets_an_sts_audience() {
        assert_eq!(
            from_json(br#"{"type":"gdch_service_account","format_version":"2"}"#)
                .err()
                .unwrap(),
            "credentials: unsupported gdch_service_account format \"2\""
        );
        assert_eq!(
            from_json(br#"{"type":"gdch_service_account","format_version":"1"}"#)
                .err()
                .unwrap(),
            "credentials: STSAudience must be set for the GDCH auth flows"
        );
    }
}
