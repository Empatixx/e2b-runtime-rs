use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::go_url::escape::{Mode, escape};

const KEY_LEN: usize = 32;

#[derive(Clone, PartialEq, Eq)]
pub struct UploadSigner {
    pub base_url: String,
    key: Vec<u8>,
}

impl UploadSigner {
    pub fn generate(base_url: &str) -> Self {
        Self::with_key(base_url, random_key())
    }

    pub fn with_key(base_url: &str, key: Vec<u8>) -> Self {
        Self {
            base_url: base_url.to_owned(),
            key,
        }
    }

    pub fn signed_url(&self, path: &str, expires_in: Duration) -> String {
        let expires = unix_now() + expires_in.as_secs() as i64;
        format!(
            "{}/upload?path={}&expires={expires}&token={}",
            self.base_url,
            escape(path, Mode::QueryComponent),
            escape(&self.token(path, expires), Mode::QueryComponent)
        )
    }

    pub fn validate(&self, path: &str, expires: i64, token: &str) -> bool {
        unix_now() <= expires && constant_time_eq(self.token(path, expires).as_bytes(), token.as_bytes())
    }

    fn token(&self, path: &str, expires: i64) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.key).expect("hmac accepts any key length");
        mac.update(path.as_bytes());
        mac.update(&[0]);
        mac.update(expires.to_string().as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }
}

fn random_key() -> Vec<u8> {
    rand::random::<[u8; KEY_LEN]>().to_vec()
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len() && left.iter().zip(right).fold(0, |diff, (a, b)| diff | (a ^ b)) == 0
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use crate::go_url::GoUrl;

    use super::*;

    #[test]
    fn tokens_match_go_compute_upload_hmac() {
        let signer = UploadSigner::with_key("http://localhost:5008", b"key".to_vec());

        assert_eq!(
            signer.token("t/files/h.tar", 1_700_000_000),
            "4891776bd4bf4e2bd5938f567cbd2417bc00e20d659da83e5cb569351c37d4ea"
        );
    }

    #[test]
    fn signed_urls_validate_until_they_expire() {
        let signer = UploadSigner::with_key("http://localhost:5008", vec![7; KEY_LEN]);
        let url = GoUrl::parse(&signer.signed_url("scope/files/a b.tar", Duration::from_secs(60))).unwrap();
        let query = url.query();
        let value = |key: &str| query.values[key][0].clone();

        assert_eq!(url.path, "/upload");
        assert_eq!(value("path"), "scope/files/a b.tar");
        let expires = value("expires").parse().unwrap();
        assert!(signer.validate("scope/files/a b.tar", expires, &value("token")));
        assert!(!signer.validate("scope/files/other.tar", expires, &value("token")));
        assert!(!signer.validate(
            "scope/files/a b.tar",
            unix_now() - 1,
            &signer.token("scope/files/a b.tar", unix_now() - 1)
        ));
    }
}
