use serde_json::Value;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DedupBudget {
    pub max_fetch_windows_per_block: i64,
    pub max_promoted_parent_pages_per_block: i64,
    pub max_pages_per_promoted_frame: i64,
    pub block_fault_pct: i64,
    pub fetch_run_window_pages: i64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DedupConfig {
    pub enabled: bool,
    pub best_effort: bool,
    pub direct_io: bool,
    pub budget: DedupBudget,
}

impl DedupConfig {
    pub fn from_flag(value: &Value) -> Self {
        let flag = |key: &str| value.get(key).and_then(Value::as_bool).unwrap_or(false);
        let number = |key: &str| value.get(key).and_then(Value::as_i64).unwrap_or(0);
        Self {
            enabled: flag("enabled"),
            best_effort: flag("bestEffort"),
            direct_io: flag("directIO"),
            budget: DedupBudget {
                max_fetch_windows_per_block: number("maxFetchWindowsPerBlock"),
                max_promoted_parent_pages_per_block: number("maxPromotedParentPagesPerBlock"),
                max_pages_per_promoted_frame: number("maxPagesPerPromotedFrame"),
                block_fault_pct: number("blockFaultPct"),
                fetch_run_window_pages: number("fetchRunWindowPages"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_flag_default_is_off_with_every_budget_disabled() {
        assert_eq!(DedupConfig::from_flag(&Value::Null), DedupConfig::default());
        assert!(!DedupConfig::from_flag(&json!({"enabled": false})).enabled);
    }

    #[test]
    fn the_flag_keys_follow_upstream() {
        let config = DedupConfig::from_flag(&json!({
            "enabled": true,
            "bestEffort": true,
            "directIO": true,
            "maxFetchWindowsPerBlock": 2,
            "maxPromotedParentPagesPerBlock": 64,
            "maxPagesPerPromotedFrame": 8,
            "blockFaultPct": 50,
            "fetchRunWindowPages": 512,
        }));

        assert_eq!(
            config,
            DedupConfig {
                enabled: true,
                best_effort: true,
                direct_io: true,
                budget: DedupBudget {
                    max_fetch_windows_per_block: 2,
                    max_promoted_parent_pages_per_block: 64,
                    max_pages_per_promoted_frame: 8,
                    block_fault_pct: 50,
                    fetch_run_window_pages: 512,
                },
            }
        );
    }
}
