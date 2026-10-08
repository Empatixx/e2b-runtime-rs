pub mod incoming;
pub mod outgoing;

use serde::Serialize;

use crate::app::Defaults;
use crate::proto::upgrade::{HandoverDefaults, HandoverState};

pub const HANDOVER_PATH: &str = "/run/e2b/envd-handover.pb";
pub const DEFAULT_UPGRADE_BIN: &str = "/usr/bin/envd.next";
const BUILTIN_DEFAULT_USER: &str = "root";
const SCHEMA_BASE: u32 = 1;
const SCHEMA_GUEST_FROZEN: u32 = 2;
const SCHEMA_DEFAULTS: u32 = 3;
const MAX_SCHEMA: u32 = SCHEMA_DEFAULTS;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct HandoverReport {
    pub failed: bool,
    pub procs: usize,
    pub procs_failed: usize,
    pub retained: usize,
    pub retained_failed: usize,
    pub watchers: usize,
    pub watchers_failed: usize,
}

fn schema_for(state: &HandoverState) -> u32 {
    let defaults = state.defaults.as_option();
    let carries_defaults = defaults.is_some_and(|defaults| {
        (!defaults.user.is_empty() && defaults.user != BUILTIN_DEFAULT_USER) || defaults.has_workdir
    });
    match (carries_defaults, state.guest_frozen_cgroups.is_empty()) {
        (true, _) => SCHEMA_DEFAULTS,
        (false, false) => SCHEMA_GUEST_FROZEN,
        (false, true) => SCHEMA_BASE,
    }
}

fn handover_defaults(defaults: &Defaults) -> Option<HandoverDefaults> {
    let carried = HandoverDefaults {
        user: if defaults.user_delivered {
            defaults.user.clone()
        } else {
            String::new()
        },
        workdir: defaults.workdir.clone().unwrap_or_default(),
        has_workdir: defaults.workdir.is_some(),
        ..Default::default()
    };
    (!carried.user.is_empty() || carried.has_workdir).then_some(carried)
}

fn apply_defaults(carried: Option<&HandoverDefaults>, defaults: &mut Defaults) -> (bool, bool) {
    let Some(carried) = carried else {
        return (false, false);
    };
    let user = !carried.user.is_empty();
    if user {
        defaults.user = carried.user.clone();
        defaults.user_delivered = true;
    }
    if carried.has_workdir {
        defaults.workdir = Some(carried.workdir.clone());
    }
    (user, carried.has_workdir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_travel_only_when_delivered_and_pick_the_schema() {
        let fallback = Defaults::default();
        assert_eq!(handover_defaults(&fallback), None);
        let delivered = Defaults {
            user: "user".to_owned(),
            workdir: Some(String::new()),
            user_delivered: true,
        };
        let carried = handover_defaults(&delivered).unwrap();
        assert_eq!((carried.user.as_str(), carried.has_workdir), ("user", true));

        let mut state = HandoverState::default();
        assert_eq!(schema_for(&state), 1);
        state.guest_frozen_cgroups = vec!["a".to_owned()];
        assert_eq!(schema_for(&state), 2);
        state.defaults = carried.clone().into();
        assert_eq!(schema_for(&state), 3);

        let mut restored = Defaults::default();
        assert_eq!(apply_defaults(Some(&carried), &mut restored), (true, true));
        assert_eq!(
            (
                restored.user.as_str(),
                restored.workdir.as_deref(),
                restored.user_delivered
            ),
            ("user", Some(""), true)
        );
    }
}
