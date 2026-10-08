use tokio::time::Instant;

#[derive(Debug, Clone)]
pub struct PhaseTimer {
    started: Instant,
    last: Instant,
    phases: Vec<Phase>,
}

#[derive(Debug, Clone)]
struct Phase {
    name: &'static str,
    value: String,
}

impl PhaseTimer {
    pub fn start() -> Self {
        let now = Instant::now();
        Self {
            started: now,
            last: now,
            phases: Vec::new(),
        }
    }

    pub fn mark(&mut self, name: &'static str) {
        let now = Instant::now();
        self.phases.push(Phase {
            name,
            value: format!("{}ms", (now - self.last).as_millis()),
        });
        self.last = now;
    }

    pub fn note(&mut self, name: &'static str, value: impl std::fmt::Display) {
        self.phases.push(Phase {
            name,
            value: value.to_string(),
        });
    }

    pub fn render(&self) -> String {
        self.phases
            .iter()
            .map(|phase| format!("{}={}", phase.name, phase.value))
            .chain(std::iter::once(format!(
                "total={}ms",
                (self.last - self.started).as_millis()
            )))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn renders_named_durations_in_order() {
        let mut timer = PhaseTimer::start();
        tokio::time::sleep(Duration::from_millis(12)).await;
        timer.mark("slot");
        tokio::time::sleep(Duration::from_millis(30)).await;
        timer.mark("rootfs");
        timer.note("faults", 7);

        assert_eq!(timer.render(), "slot=12ms rootfs=30ms faults=7 total=42ms");
    }
}
