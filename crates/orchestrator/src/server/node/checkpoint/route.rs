pub const IN_PLACE: &str = "in_place";
pub const SYNC_WP_OFF: &str = "sync_wp_off";
pub const FLAG_OFF: &str = "flag_off";
pub const FC_UNSUPPORTED: &str = "fc_unsupported";
pub const BALLOON_REPORTING: &str = "balloon_reporting";
pub const BALLOON_UNKNOWN: &str = "balloon_unknown";

pub use crate::memory::balloon_mode::BalloonMode;

pub fn early_route(sync_wp: bool, flag_on: bool, firecracker_supported: impl FnOnce() -> bool) -> Option<&'static str> {
    if !sync_wp {
        return Some(SYNC_WP_OFF);
    }
    if !flag_on {
        return Some(FLAG_OFF);
    }
    (!firecracker_supported()).then_some(FC_UNSUPPORTED)
}

pub fn balloon_route(
    mode: BalloonMode,
    deferred_export: impl FnOnce() -> bool,
    admit_reporting: impl FnOnce() -> bool,
) -> &'static str {
    match mode {
        BalloonMode::Hinting | BalloonMode::None => IN_PLACE,
        BalloonMode::Reporting => reporting_route(BALLOON_REPORTING, deferred_export, admit_reporting),
        BalloonMode::Unknown => reporting_route(BALLOON_UNKNOWN, deferred_export, admit_reporting),
    }
}

fn reporting_route(
    refused: &'static str,
    deferred_export: impl FnOnce() -> bool,
    admit_reporting: impl FnOnce() -> bool,
) -> &'static str {
    match !deferred_export() || admit_reporting() {
        true => IN_PLACE,
        false => refused,
    }
}

#[cfg(test)]
mod tests {
    use crate::template::metadata::Balloon;
    use std::cell::Cell;

    use super::*;

    #[test]
    fn the_early_half_names_the_first_failing_condition_in_order() {
        let cases = [
            ("all clear, balloon decides", true, true, true, None, true),
            ("sync-wp off first", false, true, true, Some(SYNC_WP_OFF), false),
            ("flag off", true, false, true, Some(FLAG_OFF), false),
            ("firecracker predates it", true, true, false, Some(FC_UNSUPPORTED), true),
        ];

        for (name, sync_wp, flag_on, firecracker_ok, want, firecracker_asked) in cases {
            let asked = Cell::new(false);

            let route = early_route(sync_wp, flag_on, || {
                asked.set(true);
                firecracker_ok
            });

            assert_eq!(route, want, "{name}");
            assert_eq!(asked.get(), firecracker_asked, "{name}");
        }
    }

    #[test]
    fn the_balloon_half_admits_an_allow_list_and_asks_each_flag_only_where_it_decides() {
        let cases = [
            ("hinting", BalloonMode::Hinting, true, false, IN_PLACE, false, false),
            ("no balloon", BalloonMode::None, true, false, IN_PLACE, false, false),
            (
                "reporting, deferred export",
                BalloonMode::Reporting,
                true,
                false,
                BALLOON_REPORTING,
                true,
                true,
            ),
            (
                "reporting, synchronous export",
                BalloonMode::Reporting,
                false,
                false,
                IN_PLACE,
                true,
                false,
            ),
            (
                "reporting re-admitted",
                BalloonMode::Reporting,
                true,
                true,
                IN_PLACE,
                true,
                true,
            ),
            (
                "unknown fails closed",
                BalloonMode::Unknown,
                true,
                false,
                BALLOON_UNKNOWN,
                true,
                true,
            ),
            (
                "unknown, synchronous export",
                BalloonMode::Unknown,
                false,
                false,
                IN_PLACE,
                true,
                false,
            ),
            (
                "unknown re-admitted",
                BalloonMode::Unknown,
                true,
                true,
                IN_PLACE,
                true,
                true,
            ),
        ];

        for (name, mode, deferred, admit, want, deferred_asked, admit_asked) in cases {
            let (deferred_seen, admit_seen) = (Cell::new(false), Cell::new(false));

            let route = balloon_route(
                mode,
                || {
                    deferred_seen.set(true);
                    deferred
                },
                || {
                    admit_seen.set(true);
                    admit
                },
            );

            assert_eq!(route, want, "{name}");
            assert_eq!(deferred_seen.get(), deferred_asked, "{name}: defer-memory-export asked");
            assert_eq!(admit_seen.get(), admit_asked, "{name}: re-admit flag asked");
        }
    }

    #[test]
    fn the_template_stamp_names_the_balloon_like_go() {
        let stamp = |reporting, hinting| BalloonMode::stamped(Some(Balloon { reporting, hinting }));

        assert_eq!(BalloonMode::stamped(None), BalloonMode::Unknown);
        assert_eq!(stamp(false, false), BalloonMode::None);
        assert_eq!(stamp(true, false), BalloonMode::Reporting);
        assert_eq!(stamp(false, true), BalloonMode::Hinting);
        assert_eq!(stamp(true, true), BalloonMode::Reporting);
    }

    #[test]
    fn balloon_modes_read_like_the_go_labels() {
        let labels = [
            (BalloonMode::Unknown, "unknown"),
            (BalloonMode::None, "none"),
            (BalloonMode::Reporting, "reporting"),
            (BalloonMode::Hinting, "hinting"),
        ];

        for (mode, label) in labels {
            assert_eq!(mode.as_str(), label);
        }
    }
}
