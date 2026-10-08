use serde::Deserialize;

pub const MAX_PROCESS_LEN: usize = 64;
pub const MAX_KILLS_LOGGED: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct OomKill {
    pub process: String,
    pub seq: i64,
}

#[derive(Debug, Deserialize)]
pub struct EnvdMetrics {
    pub oom_kills: Option<Vec<OomKill>>,
}

#[derive(Debug, Default)]
pub struct Watermark {
    seq: i64,
    started: bool,
    seed_failed: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Unseen {
    pub kills: Vec<OomKill>,
    pub more: usize,
}

impl Watermark {
    pub fn seed(&mut self, kills: &[OomKill]) {
        if self.started || self.seed_failed {
            return;
        }
        self.started = true;
        self.seq = kills.iter().map(|kill| kill.seq).max().unwrap_or(self.seq);
    }

    pub fn seed_done(&self) -> bool {
        self.started || self.seed_failed
    }

    pub fn seed_failed(&mut self) {
        self.seed_failed = true;
    }

    pub fn unseen(&mut self, kills: &[OomKill]) -> Unseen {
        let empty = Unseen {
            kills: Vec::new(),
            more: 0,
        };
        if !self.started && !self.seed_failed {
            return empty;
        }
        let max = kills
            .iter()
            .map(|kill| kill.seq)
            .max()
            .unwrap_or(self.seq)
            .max(self.seq);
        if !self.started {
            self.started = true;
            self.seq = max;
            return empty;
        }
        let fresh: Vec<OomKill> = kills.iter().filter(|kill| kill.seq > self.seq).cloned().collect();
        self.seq = max;
        let more = fresh.len().saturating_sub(MAX_KILLS_LOGGED);
        Unseen {
            kills: fresh
                .into_iter()
                .take(MAX_KILLS_LOGGED)
                .map(|kill| OomKill {
                    process: kill.process.chars().take(MAX_PROCESS_LEN).collect(),
                    seq: kill.seq,
                })
                .collect(),
            more,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kill(process: &str, seq: i64) -> OomKill {
        OomKill {
            process: process.to_owned(),
            seq,
        }
    }

    #[test]
    fn seeded_kills_are_never_reported_and_new_ones_are() {
        let mut watermark = Watermark::default();
        assert_eq!(watermark.unseen(&[kill("a", 1)]).kills, Vec::new());

        watermark.seed(&[kill("a", 1), kill("b", 2)]);
        let unseen = watermark.unseen(&[kill("a", 1), kill("b", 2), kill("c", 3)]);

        assert_eq!(
            unseen,
            Unseen {
                kills: vec![kill("c", 3)],
                more: 0
            }
        );
        assert!(watermark.unseen(&[kill("c", 3)]).kills.is_empty());
    }

    #[test]
    fn a_failed_seed_lets_the_first_poll_set_the_watermark() {
        let mut watermark = Watermark::default();
        watermark.seed_failed();

        assert!(watermark.unseen(&[kill("a", 5)]).kills.is_empty());
        assert_eq!(
            watermark.unseen(&[kill("a", 5), kill("b", 6)]).kills,
            vec![kill("b", 6)]
        );
    }

    #[test]
    fn at_most_ten_kills_are_logged_and_names_are_truncated() {
        let mut watermark = Watermark::default();
        watermark.seed(&[]);
        let long = "x".repeat(100);
        let kills: Vec<OomKill> = (1..=13).map(|seq| kill(&long, seq)).collect();

        let unseen = watermark.unseen(&kills);

        assert_eq!((unseen.kills.len(), unseen.more), (10, 3));
        assert_eq!(unseen.kills[0].process.len(), 64);
    }

    #[test]
    fn metrics_without_oom_kills_mean_unknown() {
        let unknown: EnvdMetrics = serde_json::from_str(r#"{"cpu_count":2}"#).unwrap();
        let known: EnvdMetrics = serde_json::from_str(r#"{"oom_kills":[{"process":"x","seq":4}]}"#).unwrap();

        assert!(unknown.oom_kills.is_none());
        assert_eq!(known.oom_kills.unwrap(), vec![kill("x", 4)]);
    }
}
