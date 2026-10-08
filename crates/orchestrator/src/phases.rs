use std::time::Duration;

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
    took: Duration,
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
            took: now - self.last,
        });
        self.last = now;
    }

    pub fn render(&self) -> String {
        self.phases
            .iter()
            .map(|phase| format!("{}={}ms", phase.name, phase.took.as_millis()))
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
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn renders_named_durations_in_order() {
        let mut timer = PhaseTimer::start();
        tokio::time::sleep(Duration::from_millis(12)).await;
        timer.mark("slot");
        tokio::time::sleep(Duration::from_millis(30)).await;
        timer.mark("rootfs");

        assert_eq!(timer.render(), "slot=12ms rootfs=30ms total=42ms");
    }
}
