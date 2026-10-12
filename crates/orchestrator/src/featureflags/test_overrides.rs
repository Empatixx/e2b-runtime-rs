use serde_json::{Value, json};

use super::{FeatureFlags, USE_MEMFD, memfile_diff_dedup};

pub fn apply_test_overrides(flags: &FeatureFlags, var: impl Fn(&str) -> Option<String>) {
    if let Some(dedup) = var("TESTS_MEMFILE_DIFF_DEDUP_MODE")
        .filter(|mode| !mode.is_empty() && mode != "default")
        .map(|mode| dedup_override(&mode))
    {
        flags.set_override(memfile_diff_dedup().key, dedup);
    }
    if var("TESTS_DISABLE_MEMFD").is_some_and(|value| value == "true") {
        flags.set_override(USE_MEMFD.key, Value::Bool(false));
    }
}

fn dedup_override(mode: &str) -> Value {
    let defrag = i64::from(mode == "direct_io");
    json!({
        "enabled": true,
        "bestEffort": mode == "best_effort",
        "directIO": mode == "direct_io",
        "maxFetchWindowsPerBlock": 2 * defrag,
        "maxPromotedParentPagesPerBlock": 64 * defrag,
        "maxPagesPerPromotedFrame": 8 * defrag,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn flags_after(vars: &[(&str, &str)]) -> FeatureFlags {
        let vars: HashMap<String, String> = vars.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        let flags = FeatureFlags::offline();
        apply_test_overrides(&flags, |name| vars.get(name).cloned());
        flags
    }

    #[test]
    fn disabling_memfd_turns_the_flag_off() {
        assert!(!flags_after(&[("TESTS_DISABLE_MEMFD", "true")]).get(&USE_MEMFD, &[]));
        assert!(flags_after(&[("TESTS_DISABLE_MEMFD", "1")]).get(&USE_MEMFD, &[]));
    }

    #[test]
    fn dedup_modes_set_the_go_override() {
        let direct = flags_after(&[("TESTS_MEMFILE_DIFF_DEDUP_MODE", "direct_io")]).get(&memfile_diff_dedup(), &[]);
        assert_eq!(
            direct,
            json!({
                "enabled": true,
                "bestEffort": false,
                "directIO": true,
                "maxFetchWindowsPerBlock": 2,
                "maxPromotedParentPagesPerBlock": 64,
                "maxPagesPerPromotedFrame": 8,
            })
        );
        let default = flags_after(&[("TESTS_MEMFILE_DIFF_DEDUP_MODE", "default")]).get(&memfile_diff_dedup(), &[]);
        assert_eq!(default["enabled"], false);
    }
}
