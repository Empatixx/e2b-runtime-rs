use serde::Deserialize;

use super::body::truncate;

const HEADER_MAX_BYTES: usize = 512;
const REJECTED_MAX_RUNES: usize = 100;
const ENVD_CANNOT_SET_CPUS: &str = "envd sent no X-Envd-Cpus, so it cannot offline the spare vcpus";

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct EnvdCpus {
    #[allow(dead_code)]
    online: i64,
    possible: i64,
    target: i64,
    rejected: String,
}

pub fn verify_envd_cpu_count(header: &str, want: i64, vm_vcpus: i64) -> Result<(), String> {
    if header.is_empty() {
        return match vm_vcpus > want {
            true => Err(ENVD_CANNOT_SET_CPUS.to_owned()),
            false => Ok(()),
        };
    }
    let report = decode_header(header).map_err(|e| format!("unreadable X-Envd-Cpus: {e}"))?;
    match (report.rejected.is_empty(), report.target == want) {
        (false, _) => Err(format!(
            "cpu count {want} rejected, guest keeps {} of {}: {}",
            report.target,
            report.possible,
            truncate(&report.rejected, REJECTED_MAX_RUNES)
        )),
        (true, false) => Err(format!(
            "cpu count {want} sent, guest targets {} of {}",
            report.target, report.possible
        )),
        (true, true) => Ok(()),
    }
}

fn decode_header(header: &str) -> Result<EnvdCpus, String> {
    match header.len() > HEADER_MAX_BYTES {
        true => Err(format!(
            "header is {} bytes, over the {HEADER_MAX_BYTES}-byte cap",
            header.len()
        )),
        false => serde_json::from_str(header).map_err(|e| e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envd_answers_are_checked_against_the_count_sent() {
        let oversized = format!(
            r#"{{"online":2,"possible":16,"target":2,"rejected":"{}"}}"#,
            "x".repeat(600)
        );
        for (name, header, vm_vcpus, taken) in [
            ("taken", r#"{"online":2,"possible":16,"target":2}"#, 16, true),
            (
                "rejected",
                r#"{"online":2,"possible":2,"target":2,"rejected":"cpu target 4 outside 1..2"}"#,
                2,
                false,
            ),
            ("other", r#"{"online":2,"possible":16,"target":4}"#, 16, false),
            ("malformed", "{", 16, false),
            ("oversized", oversized.as_str(), 16, false),
            ("old envd, spare vcpus", "", 16, false),
            ("old envd, vm of vcpu size", "", 2, true),
            ("old envd, unknown size", "", 0, true),
        ] {
            assert_eq!(verify_envd_cpu_count(header, 2, vm_vcpus).is_ok(), taken, "{name}");
        }
    }

    #[test]
    fn a_refusal_names_what_the_guest_kept_and_bounds_the_guest_text() {
        let header = format!(
            r#"{{"online":2,"possible":2,"target":2,"rejected":"{}"}}"#,
            "x".repeat(300)
        );

        let error = verify_envd_cpu_count(&header, 4, 2).unwrap_err();

        assert_eq!(
            error,
            format!("cpu count 4 rejected, guest keeps 2 of 2: {}...", "x".repeat(97))
        );
        assert_eq!(
            verify_envd_cpu_count(r#"{"online":2,"possible":16,"target":4}"#, 2, 16),
            Err("cpu count 2 sent, guest targets 4 of 16".to_owned())
        );
        assert_eq!(
            verify_envd_cpu_count("", 2, 16),
            Err("envd sent no X-Envd-Cpus, so it cannot offline the spare vcpus".to_owned())
        );
    }
}
