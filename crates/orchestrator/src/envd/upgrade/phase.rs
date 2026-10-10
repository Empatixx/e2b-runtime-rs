use super::deliver::DeliveryFailure;

pub const RESOLVE: &str = "resolve";
pub const DELIVER: &str = "deliver";
pub const READY: &str = "ready";

pub fn delivery_result(delivery: &Result<bool, DeliveryFailure>) -> &'static str {
    match delivery {
        Err(_) => "failed",
        Ok(false) => "unconfirmed",
        Ok(true) => "success",
    }
}

pub fn phase_result<T, E>(outcome: &Result<T, E>) -> &'static str {
    match outcome {
        Ok(_) => "success",
        Err(_) => "failed",
    }
}

pub fn resolve_phase_result(resolved: bool, reason: &str) -> Option<String> {
    match (resolved, reason) {
        (true, _) => Some("success".to_owned()),
        (false, "off" | "not_staged" | "invalid_target") => None,
        (false, reason) => Some(reason.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envd::upgrade::deliver::DeliveryFailure;

    fn failure() -> DeliveryFailure {
        DeliveryFailure {
            message: "connection refused".to_owned(),
            source_missing: false,
        }
    }

    #[test]
    fn delivery_distinguishes_an_unconfirmed_delivery() {
        assert_eq!(delivery_result(&Ok(true)), "success");
        assert_eq!(delivery_result(&Err(failure())), "failed");
        assert_eq!(delivery_result(&Ok(false)), "unconfirmed");
    }

    #[test]
    fn a_phase_result_is_binary() {
        assert_eq!(phase_result(&Ok::<(), String>(())), "success");
        assert_eq!(phase_result(&Err::<(), _>("boom")), "failed");
    }

    #[test]
    fn only_resolutions_that_probed_are_recorded() {
        let cases = [
            ("an upgrade resolved", true, "", Some("success")),
            ("flag off", false, "off", None),
            ("target not staged", false, "not_staged", None),
            ("invalid target", false, "invalid_target", None),
            ("already on the target", false, "same_version", Some("same_version")),
            ("downgrade refused", false, "downgrade", Some("downgrade")),
            ("probe failed", false, "getversion_failed", Some("getversion_failed")),
            (
                "deferred, not cached",
                false,
                "binary_not_cached",
                Some("binary_not_cached"),
            ),
        ];
        for (name, resolved, reason, want) in cases {
            assert_eq!(resolve_phase_result(resolved, reason).as_deref(), want, "{name}");
        }
    }
}
