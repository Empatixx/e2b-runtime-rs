pub const NO_DEFAULT_USER: &str = "no_default_user";
pub const UNPROVABLE_WORKDIR: &str = "unprovable_workdir";
pub const OLD_ENVD: &str = "old_envd";

pub fn decline_reason(sent_user: &str, workdir_withheld: bool, envd_preserves: bool) -> Option<&'static str> {
    [
        (sent_user.is_empty() && !envd_preserves, NO_DEFAULT_USER),
        (workdir_withheld && !envd_preserves, UNPROVABLE_WORKDIR),
    ]
    .into_iter()
    .find_map(|(declined, reason)| declined.then_some(reason))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Case {
        name: &'static str,
        sent_user: &'static str,
        workdir_withheld: bool,
        envd_reports_defaults: bool,
        want: Option<&'static str>,
    }

    fn case(
        name: &'static str,
        sent_user: &'static str,
        workdir_withheld: bool,
        envd_reports_defaults: bool,
        want: Option<&'static str>,
    ) -> Case {
        Case {
            name,
            sent_user,
            workdir_withheld,
            envd_reports_defaults,
            want,
        }
    }

    #[test]
    fn the_decline_policy_matches_upstream() {
        let cases = [
            case("user established, nothing withheld", "user", false, false, None),
            case("any established user proceeds", "app", false, false, None),
            case(
                "nothing established, envd is silent",
                "",
                false,
                false,
                Some("no_default_user"),
            ),
            case("nothing established but envd carries it", "", false, true, None),
            case("nothing established, envd carries both", "", true, true, None),
            case(
                "no user outranks a withheld workdir",
                "",
                true,
                false,
                Some("no_default_user"),
            ),
            case(
                "withheld workdir and envd cannot carry it",
                "user",
                true,
                false,
                Some("unprovable_workdir"),
            ),
            case("withheld workdir but envd carries it", "user", true, true, None),
            case("no workdir at risk, envd reports", "user", false, true, None),
            case("no workdir at risk, envd is silent", "user", false, false, None),
        ];
        for case in cases {
            assert_eq!(
                decline_reason(case.sent_user, case.workdir_withheld, case.envd_reports_defaults),
                case.want,
                "{}",
                case.name
            );
        }
    }

    #[test]
    fn the_reasons_are_the_dashboard_values() {
        assert_eq!(NO_DEFAULT_USER, "no_default_user");
        assert_eq!(UNPROVABLE_WORKDIR, "unprovable_workdir");
        assert_eq!(OLD_ENVD, "old_envd");
    }
}
