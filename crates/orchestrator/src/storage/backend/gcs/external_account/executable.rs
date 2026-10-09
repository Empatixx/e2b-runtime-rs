use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use tokio::process::Command;

use super::config::{CredentialSource, ExternalAccountFile};
use super::exit_status::{exit_code, exit_text, start_failure};

const EMAIL_PREFIX: &str = "https://iamcredentials";
const EMAIL_PATH: &str = "/v1/projects/-/serviceAccounts/";
const EMAIL_MARKER: &str = ":generateAccessToken";
const ALLOW_ENV: &str = "GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES";
const SUPPORTED_MAX_VERSION: i64 = 1;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const MINIMUM_TIMEOUT: Duration = Duration::from_secs(5);
const MAXIMUM_TIMEOUT: Duration = Duration::from_secs(120);
const RESPONSE_SOURCE: &str = "response";
const OUTPUT_FILE_SOURCE: &str = "output file";
const JWT_TYPE: &str = "urn:ietf:params:oauth:token-type:jwt";
const ID_TOKEN_TYPE: &str = "urn:ietf:params:oauth:token-type:id_token";
const SAML_TYPE: &str = "urn:ietf:params:oauth:token-type:saml2";

pub struct ExecutableSubject {
    command: String,
    timeout: Duration,
    output_file: String,
    audience: String,
    subject_token_type: String,
    impersonated_email: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum ParseError {
    Cacheable(String),
    Reportable(String),
}

impl ParseError {
    fn into_message(self) -> String {
        match self {
            Self::Cacheable(message) | Self::Reportable(message) => message,
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Response {
    version: i64,
    success: Option<bool>,
    token_type: String,
    expiration_time: i64,
    id_token: String,
    saml_response: String,
    code: String,
    message: String,
}

impl ExecutableSubject {
    pub fn new(account: &ExternalAccountFile, source: &CredentialSource) -> Result<Self, String> {
        let config = source.executable.clone().unwrap_or_default();
        if config.command.is_empty() {
            return Err("credentials: missing `command` field — executable command must be provided".to_owned());
        }
        let timeout = match config.timeout_millis {
            0 => DEFAULT_TIMEOUT,
            millis => Duration::from_millis(u64::try_from(millis).unwrap_or(0)),
        };
        if config.timeout_millis != 0 && !(MINIMUM_TIMEOUT..=MAXIMUM_TIMEOUT).contains(&timeout) {
            return Err(format!(
                "credentials: invalid `timeout_millis` field — executable timeout must be between {} and {} seconds",
                MINIMUM_TIMEOUT.as_secs(),
                MAXIMUM_TIMEOUT.as_secs()
            ));
        }
        Ok(Self {
            command: config.command,
            timeout,
            output_file: config.output_file,
            audience: account.audience.clone(),
            subject_token_type: account.subject_token_type.clone(),
            impersonated_email: impersonated_email(&account.service_account_impersonation_url),
        })
    }

    pub async fn token(&self) -> Result<String, String> {
        if let Some(token) = self.token_from_output_file()? {
            return Ok(token);
        }
        self.token_from_command().await
    }

    fn token_from_output_file(&self) -> Result<Option<String>, String> {
        if self.output_file.is_empty() {
            return Ok(None);
        }
        let data = match std::fs::read(&self.output_file) {
            Ok(data) if !data.is_empty() => data,
            _ => return Ok(None),
        };
        match self.parse(&data, OUTPUT_FILE_SOURCE, now_unix()) {
            Ok(token) => Ok(Some(token)),
            Err(ParseError::Cacheable(_)) => Ok(None),
            Err(ParseError::Reportable(message)) => Err(message),
        }
    }

    async fn token_from_command(&self) -> Result<String, String> {
        if std::env::var(ALLOW_ENV).ok().as_deref() != Some("1") {
            return Err(format!(
                "credentials: executables need to be explicitly allowed (set {ALLOW_ENV} to '1') to run"
            ));
        }
        let output = tokio::time::timeout(self.timeout, self.run())
            .await
            .map_err(|_| "context deadline exceeded".to_owned())??;
        self.parse(&output, RESPONSE_SOURCE, now_unix())
            .map_err(ParseError::into_message)
    }

    async fn run(&self) -> Result<Vec<u8>, String> {
        let mut words = self.command.split_whitespace();
        let program = words.next().unwrap_or_default();
        let mut command = Command::new(program);
        command
            .args(words)
            .env("GOOGLE_EXTERNAL_ACCOUNT_AUDIENCE", &self.audience)
            .env("GOOGLE_EXTERNAL_ACCOUNT_TOKEN_TYPE", &self.subject_token_type)
            .env("GOOGLE_EXTERNAL_ACCOUNT_INTERACTIVE", "0")
            .kill_on_drop(true);
        if let Some(email) = &self.impersonated_email {
            command.env("GOOGLE_EXTERNAL_ACCOUNT_IMPERSONATED_EMAIL", email);
        }
        if !self.output_file.is_empty() {
            command.env("GOOGLE_EXTERNAL_ACCOUNT_OUTPUT_FILE", &self.output_file);
        }
        let output = command
            .output()
            .await
            .map_err(|e| format!("credentials: executable command failed: {}", start_failure(program, &e)))?;
        if !output.status.success() {
            return Err(format!(
                "credentials: executable command failed with exit code {}: {}",
                exit_code(output.status),
                exit_text(output.status)
            ));
        }
        let stdout = output.stdout.trim_ascii();
        Ok(match stdout.is_empty() {
            false => stdout.to_vec(),
            true => output.stderr.trim_ascii().to_vec(),
        })
    }

    fn parse(&self, response: &[u8], source: &str, now: i64) -> Result<String, ParseError> {
        let parsed: Response = serde_json::from_slice(response).map_err(|_| {
            ParseError::Reportable(format!(
                "credentials: unable to parse {}: {}",
                crate::go_strconv::quote(source),
                String::from_utf8_lossy(response)
            ))
        })?;
        let missing = |field: &str| {
            ParseError::Reportable(format!(
                "credentials: {} missing {} field",
                crate::go_strconv::quote(source),
                crate::go_strconv::quote(field)
            ))
        };
        if parsed.version == 0 {
            return Err(missing("version"));
        }
        match parsed.success {
            None => return Err(missing("success")),
            Some(false) if parsed.code.is_empty() || parsed.message.is_empty() => {
                return Err(ParseError::Cacheable(
                    "credentials: response must include `error` and `message` fields when unsuccessful".to_owned(),
                ));
            }
            Some(false) => {
                return Err(ParseError::Cacheable(format!(
                    "credentials: response contains unsuccessful response: ({}) {}",
                    parsed.code, parsed.message
                )));
            }
            Some(true) => {}
        }
        if !(0..=SUPPORTED_MAX_VERSION).contains(&parsed.version) {
            return Err(ParseError::Reportable(format!(
                "credentials: {source} contains unsupported version: {}",
                parsed.version
            )));
        }
        if parsed.expiration_time == 0 && !self.output_file.is_empty() {
            return Err(missing("expiration_time"));
        }
        if parsed.token_type.is_empty() {
            return Err(missing("token_type"));
        }
        if parsed.expiration_time != 0 && parsed.expiration_time < now {
            return Err(ParseError::Cacheable(
                "credentials: the token returned by the executable is expired".to_owned(),
            ));
        }
        match parsed.token_type.as_str() {
            JWT_TYPE | ID_TOKEN_TYPE if parsed.id_token.is_empty() => Err(missing("id_token")),
            JWT_TYPE | ID_TOKEN_TYPE => Ok(parsed.id_token),
            SAML_TYPE if parsed.saml_response.is_empty() => Err(missing("saml_response")),
            SAML_TYPE => Ok(parsed.saml_response),
            _ => Err(ParseError::Reportable(format!(
                "credentials: {source} contains unsupported token type"
            ))),
        }
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(0))
}

fn impersonated_email(url: &str) -> Option<String> {
    url.match_indices(EMAIL_PREFIX)
        .find_map(|(start, _)| email_after(&url[start + EMAIL_PREFIX.len()..]))
}

fn email_after(rest: &str) -> Option<String> {
    let mut literals: Vec<usize> = rest.match_indices(EMAIL_PATH).map(|(index, _)| index).collect();
    literals.reverse();
    literals.into_iter().find_map(|index| {
        let host = &rest[..index];
        let account = &rest[index + EMAIL_PATH.len()..];
        (host.chars().count() >= 2 && !host.contains('\n'))
            .then(|| email_before_marker(account))
            .flatten()
    })
}

fn email_before_marker(account: &str) -> Option<String> {
    let first_at = account.find('@')?;
    let markers: Vec<usize> = account
        .match_indices(EMAIL_MARKER)
        .map(|(index, _)| index)
        .filter(|index| *index > first_at)
        .collect();
    markers
        .into_iter()
        .rev()
        .map(|index| &account[..index])
        .find(|email| !email.contains('\n'))
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subject(output_file: &str) -> ExecutableSubject {
        ExecutableSubject {
            command: "true".to_owned(),
            timeout: DEFAULT_TIMEOUT,
            output_file: output_file.to_owned(),
            audience: "aud".to_owned(),
            subject_token_type: JWT_TYPE.to_owned(),
            impersonated_email: None,
        }
    }

    fn error(subject: &ExecutableSubject, json: &str) -> String {
        subject
            .parse(json.as_bytes(), RESPONSE_SOURCE, 1000)
            .unwrap_err()
            .into_message()
    }

    #[test]
    fn successful_responses_yield_the_id_token_or_saml_response() {
        let subject = subject("");

        assert_eq!(
            subject
                .parse(
                    format!(r#"{{"version":1,"success":true,"token_type":"{JWT_TYPE}","id_token":"jwt"}}"#).as_bytes(),
                    RESPONSE_SOURCE,
                    1000
                )
                .unwrap(),
            "jwt"
        );
        assert_eq!(
            subject
                .parse(
                    format!(r#"{{"version":1,"success":true,"token_type":"{SAML_TYPE}","saml_response":"s"}}"#)
                        .as_bytes(),
                    RESPONSE_SOURCE,
                    1000
                )
                .unwrap(),
            "s"
        );
    }

    #[test]
    fn bad_responses_fail_with_the_sdk_texts() {
        let subject = subject("");

        assert_eq!(
            error(&subject, "nope"),
            "credentials: unable to parse \"response\": nope"
        );
        assert_eq!(
            error(&subject, "{}"),
            "credentials: \"response\" missing \"version\" field"
        );
        assert_eq!(
            error(&subject, r#"{"version":1}"#),
            "credentials: \"response\" missing \"success\" field"
        );
        assert_eq!(
            error(&subject, r#"{"version":1,"success":false}"#),
            "credentials: response must include `error` and `message` fields when unsuccessful"
        );
        assert_eq!(
            error(&subject, r#"{"version":1,"success":false,"code":"401","message":"no"}"#),
            "credentials: response contains unsuccessful response: (401) no"
        );
        assert_eq!(
            error(&subject, r#"{"version":2,"success":true}"#),
            "credentials: response contains unsupported version: 2"
        );
        assert_eq!(
            error(&subject, r#"{"version":1,"success":true,"token_type":"x"}"#),
            "credentials: response contains unsupported token type"
        );
        assert_eq!(
            error(
                &subject,
                &format!(r#"{{"version":1,"success":true,"token_type":"{JWT_TYPE}","expiration_time":5}}"#)
            ),
            "credentials: the token returned by the executable is expired"
        );
    }

    #[test]
    fn cached_output_files_are_used_until_they_expire() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.json");
        let subject = subject(path.to_str().unwrap());
        let future = now_unix() + 600;
        std::fs::write(
            &path,
            format!(r#"{{"version":1,"success":true,"token_type":"{JWT_TYPE}","id_token":"cached","expiration_time":{future}}}"#),
        )
        .unwrap();

        assert_eq!(subject.token_from_output_file().unwrap().as_deref(), Some("cached"));

        std::fs::write(
            &path,
            format!(r#"{{"version":1,"success":true,"token_type":"{JWT_TYPE}","id_token":"old","expiration_time":5}}"#),
        )
        .unwrap();
        assert_eq!(subject.token_from_output_file().unwrap(), None);
        assert_eq!(
            subject
                .parse(
                    b"{\"version\":1,\"success\":true,\"token_type\":\"x\"}",
                    OUTPUT_FILE_SOURCE,
                    1
                )
                .unwrap_err()
                .into_message(),
            "credentials: \"output file\" missing \"expiration_time\" field"
        );
    }

    #[test]
    fn construction_validates_the_command_and_timeout() {
        let account = |executable: &str| {
            ExternalAccountFile::parse(
                format!(
                    r#"{{"audience":"a","subject_token_type":"t","credential_source":{{"executable":{executable}}}}}"#
                )
                .as_bytes(),
            )
            .unwrap()
        };
        let build = |executable: &str| {
            let account = account(executable);
            ExecutableSubject::new(&account, account.credential_source.as_ref().unwrap())
        };

        assert_eq!(
            build(r#"{}"#).err().unwrap(),
            "credentials: missing `command` field — executable command must be provided"
        );
        assert_eq!(
            build(r#"{"command":"x","timeout_millis":1000}"#).err().unwrap(),
            "credentials: invalid `timeout_millis` field — executable timeout must be between 5 and 120 seconds"
        );
        assert_eq!(build(r#"{"command":"x"}"#).unwrap().timeout, DEFAULT_TIMEOUT);
    }

    #[test]
    fn the_impersonated_email_comes_from_the_url() {
        assert_eq!(
            impersonated_email("https://iamcredentials.googleapis.com/v1/projects/-/serviceAccounts/sa@p.iam.gserviceaccount.com:generateAccessToken").as_deref(),
            Some("sa@p.iam.gserviceaccount.com")
        );
        assert_eq!(impersonated_email("https://other/x"), None);
    }

    #[tokio::test]
    async fn commands_must_be_explicitly_allowed() {
        let subject = subject("");

        if std::env::var(ALLOW_ENV).ok().as_deref() != Some("1") {
            assert_eq!(
                subject.token_from_command().await.unwrap_err(),
                "credentials: executables need to be explicitly allowed (set GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES to '1') to run"
            );
        }
    }

    #[tokio::test]
    async fn the_command_runs_with_the_external_account_environment() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("emit.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\necho \"$GOOGLE_EXTERNAL_ACCOUNT_AUDIENCE:$GOOGLE_EXTERNAL_ACCOUNT_INTERACTIVE\"\n",
        )
        .unwrap();
        let failing = dir.path().join("fail.sh");
        std::fs::write(&failing, "#!/bin/sh\nexit 3\n").unwrap();
        let mut subject = subject("");

        subject.command = format!("sh {}", script.display());
        assert_eq!(subject.run().await.unwrap(), b"aud:0");

        subject.command = format!("sh {}", failing.display());
        assert_eq!(
            subject.run().await.unwrap_err(),
            "credentials: executable command failed with exit code 3: exit status 3"
        );
    }

    #[test]
    fn the_impersonated_email_is_found_like_the_unanchored_go_regexp() {
        let email = |url: &str| impersonated_email(url);
        let base = "https://iamcredentials.googleapis.com/v1/projects/-/serviceAccounts/";

        assert_eq!(
            email(&format!("{base}sa@p.iam.gserviceaccount.com:generateAccessToken")).as_deref(),
            Some("sa@p.iam.gserviceaccount.com")
        );
        assert_eq!(
            email(&format!(
                "{base}sa@p.iam.gserviceaccount.com:generateAccessToken?alt=json"
            ))
            .as_deref(),
            Some("sa@p.iam.gserviceaccount.com")
        );
        assert_eq!(
            email("prefix https://iamcredentials-x.example.com/v1/projects/-/serviceAccounts/a@b:generateAccessToken")
                .as_deref(),
            Some("a@b")
        );
        assert_eq!(
            email(&format!(
                "{base}a@b:generateAccessToken{}c@d:generateAccessToken",
                EMAIL_PATH
            ))
            .as_deref(),
            Some("c@d")
        );
        assert_eq!(email(&format!("{base}nomail:generateAccessToken")), None);
        assert_eq!(
            email("https://iamcredentials./v1/projects/-/serviceAccounts/a@b:generateAccessToken"),
            None
        );
        assert_eq!(
            email("https://iamcredentials.a/v1/projects/-/serviceAccounts/a@b:generateAccessToken").as_deref(),
            Some("a@b")
        );
        assert_eq!(
            email(&format!("{base}a@b:generateAccessToken:generateAccessToken")).as_deref(),
            Some("a@b:generateAccessToken")
        );
        assert_eq!(
            email(&format!("{base}a:generateAccessToken@b:generateAccessToken")).as_deref(),
            Some("a:generateAccessToken@b")
        );
    }

    fn script_subject(body: &str) -> (ExecutableSubject, tempfile::TempDir) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut subject = subject("");
        subject.command = path.display().to_string();
        (subject, dir)
    }

    #[tokio::test]
    async fn failing_commands_read_like_go_exec_errors() {
        let (exits, _keep_exits) = script_subject("exit 3");
        let (killed, _keep_killed) = script_subject("kill -9 $$");
        let mut missing = subject("");
        missing.command = "no-such-binary-e2b --flag".to_owned();

        assert_eq!(
            exits.run().await.unwrap_err(),
            "credentials: executable command failed with exit code 3: exit status 3"
        );
        assert_eq!(
            killed.run().await.unwrap_err(),
            "credentials: executable command failed with exit code -1: signal: killed"
        );
        assert_eq!(
            missing.run().await.unwrap_err(),
            "credentials: executable command failed: exec: \"no-such-binary-e2b\": executable file not found in $PATH"
        );
    }

    #[tokio::test]
    async fn stderr_answers_when_stdout_is_empty() {
        let (silent, _keep) = script_subject("echo '  from-stderr  ' >&2");
        let (loud, _keep_loud) = script_subject("echo ' from-stdout '; echo ignored >&2");

        assert_eq!(silent.run().await.unwrap(), b"from-stderr");
        assert_eq!(loud.run().await.unwrap(), b"from-stdout");
    }
}
