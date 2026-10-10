use std::time::Duration;

const RESET: &str = "\x1b[0m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";

pub fn fmt_dur(duration: Duration) -> String {
    format!("{:.1}ms", duration.as_secs_f64() * 1000.0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub duration: Duration,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Timings {
    pub resume: Duration,
    pub work: Duration,
    pub total: Duration,
    pub error: Option<String>,
}

pub struct Labels {
    pub header: &'static str,
    pub short: &'static str,
    pub summary: &'static str,
    pub resume_summary: &'static str,
    pub total_summary: &'static str,
}

pub const COMMAND_LABELS: Labels = Labels {
    header: "resume / command / total",
    short: "cmd",
    summary: "Command:",
    resume_summary: "Resume: ",
    total_summary: "Total:  ",
};

pub const PAUSE_LABELS: Labels = Labels {
    header: "resume / pause / total",
    short: "pause",
    summary: "Pause: ",
    resume_summary: "Resume:",
    total_summary: "Total: ",
};

pub fn print_results(results: &[Run]) {
    if results.is_empty() {
        return;
    }
    let mut durations: Vec<Duration> = results
        .iter()
        .filter(|run| run.error.is_none())
        .map(|run| run.duration)
        .collect();
    if durations.is_empty() {
        println!("\n❌ All runs failed");
        return;
    }
    let average = average(&durations);
    println!("\n📋 Run times:");
    results.iter().enumerate().for_each(|(index, run)| match &run.error {
        Some(error) => println!("   [{:2}] ❌ Failed: {error}", index + 1),
        None => {
            let diff = signed_nanos(run.duration) - signed_nanos(average);
            let percent = diff as f64 / average.as_nanos() as f64 * 100.0;
            let color = match diff {
                diff if diff < 0 => GREEN,
                diff if diff > 0 => RED,
                _ => YELLOW,
            };
            println!(
                "   [{:2}] {}  {color}{percent:+.1}%{RESET}",
                index + 1,
                fmt_dur(run.duration)
            );
        }
    });
    let deviation = standard_deviation(&durations, average);
    durations.sort();
    let count = durations.len();
    println!("\n📊 Summary ({count} runs):");
    println!(
        "   Min: {} | Max: {} | Avg: {} | StdDev: {}",
        fmt_dur(durations[0]),
        fmt_dur(durations[count - 1]),
        fmt_dur(average),
        fmt_dur(deviation)
    );
    if count > 1 {
        let p95 = ((count - 1) as f64 * 0.95) as usize;
        let p99 = ((count - 1) as f64 * 0.99) as usize;
        println!("   P95: {} | P99: {}", fmt_dur(durations[p95]), fmt_dur(durations[p99]));
    }
}

pub fn print_timings(results: &[Timings], labels: &Labels) {
    if results.is_empty() {
        return;
    }
    let successful: Vec<&Timings> = results.iter().filter(|timings| timings.error.is_none()).collect();
    if successful.is_empty() {
        println!("\n❌ All runs failed");
        return;
    }
    let resumes: Vec<Duration> = successful.iter().map(|timings| timings.resume).collect();
    let works: Vec<Duration> = successful.iter().map(|timings| timings.work).collect();
    let totals: Vec<Duration> = successful.iter().map(|timings| timings.total).collect();
    let (average_resume, average_work) = (average(&resumes), average(&works));
    println!("\n📋 Run times ({}):", labels.header);
    results
        .iter()
        .enumerate()
        .for_each(|(index, timings)| match &timings.error {
            Some(error) => println!("   [{:2}] ❌ Failed: {error}", index + 1),
            None => {
                let resume_diff = percent(timings.resume, average_resume);
                let work_diff = percent(timings.work, average_work);
                println!(
                    "   [{:2}] {} / {} / {}  (resume: {}{resume_diff:+.1}%{RESET}, {}: {}{work_diff:+.1}%{RESET})",
                    index + 1,
                    fmt_dur(timings.resume),
                    fmt_dur(timings.work),
                    fmt_dur(timings.total),
                    color_for(resume_diff),
                    labels.short,
                    color_for(work_diff),
                );
            }
        });
    println!("\n📊 Summary ({} runs):", successful.len());
    println!("   {} Avg {}", labels.resume_summary, fmt_dur(average_resume));
    println!("   {} Avg {}", labels.summary, fmt_dur(average_work));
    println!("   {} Avg {}", labels.total_summary, fmt_dur(average(&totals)));
    if successful.len() > 1 {
        println!(
            "   {} Min {} | Max {}",
            labels.resume_summary,
            fmt_dur(*resumes.iter().min().unwrap_or(&Duration::ZERO)),
            fmt_dur(*resumes.iter().max().unwrap_or(&Duration::ZERO))
        );
        println!(
            "   {} Min {} | Max {}",
            labels.summary,
            fmt_dur(*works.iter().min().unwrap_or(&Duration::ZERO)),
            fmt_dur(*works.iter().max().unwrap_or(&Duration::ZERO))
        );
    }
}

fn average(durations: &[Duration]) -> Duration {
    durations.iter().sum::<Duration>() / durations.len().max(1) as u32
}

fn standard_deviation(durations: &[Duration], average: Duration) -> Duration {
    let variance = durations
        .iter()
        .map(|duration| (signed_nanos(*duration) - signed_nanos(average)) as f64)
        .map(|diff| diff * diff)
        .sum::<f64>()
        / durations.len() as f64;
    Duration::from_nanos(variance.sqrt() as u64)
}

fn signed_nanos(duration: Duration) -> i128 {
    duration.as_nanos() as i128
}

fn percent(value: Duration, average: Duration) -> f64 {
    (signed_nanos(value) - signed_nanos(average)) as f64 / average.as_nanos().max(1) as f64 * 100.0
}

fn color_for(diff: f64) -> &'static str {
    match diff {
        diff if diff < -5.0 => GREEN,
        diff if diff > 5.0 => RED,
        _ => YELLOW,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_print_like_go_milliseconds() {
        assert_eq!(fmt_dur(Duration::from_micros(12_345)), "12.3ms");
        assert_eq!(fmt_dur(Duration::ZERO), "0.0ms");
    }

    #[test]
    fn the_deviation_and_percentages_follow_the_average() {
        let durations = [Duration::from_millis(10), Duration::from_millis(30)];
        let average = average(&durations);

        assert_eq!(average, Duration::from_millis(20));
        assert_eq!(standard_deviation(&durations, average), Duration::from_millis(10));
        assert_eq!(percent(Duration::from_millis(30), average), 50.0);
        assert_eq!(color_for(-6.0), GREEN);
        assert_eq!(color_for(0.0), YELLOW);
    }
}
