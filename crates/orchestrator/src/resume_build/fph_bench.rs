use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tonic::Request;
use uuid::Uuid;

use super::options::FphBenchOptions;
use super::runner::Runner;
use crate::proto::SandboxPauseRequest;
use crate::proto::sandbox_service_server::SandboxService;
use crate::server::uploads::Outcome;
use crate::storage::paths::BuildPaths;
use crate::storage::spec::Provider;

const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const HINTING_FLAG: &str = "free-page-hinting-config";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Sample {
    memfile_bytes: u64,
    hint_bytes: u64,
    report_bytes: u64,
    pause: Duration,
    error: Option<String>,
}

pub async fn fph_bench(runner: &Runner, options: &FphBenchOptions) -> Result<(), String> {
    if options.iterations <= 0 {
        return Err("fph-bench: iterations must be > 0".to_owned());
    }
    println!(
        "FPH bench ({} iterations per arm, workload={:?})",
        options.iterations, options.workload
    );
    let without = arm(runner, options, false, "FPR-only ").await;
    let with = arm(runner, options, true, "FPR + FPH").await;
    for (label, samples) in [("FPR-only ", &without), ("FPR + FPH", &with)] {
        println!(
            "{label}: memfile {}  pause {:?}",
            mib(average(samples, |sample| sample.memfile_bytes as u128) as u64),
            Duration::from_nanos(average(samples, |sample| sample.pause.as_nanos()) as u64)
        );
    }
    without
        .iter()
        .chain(&with)
        .find_map(|sample| sample.error.clone())
        .map_or(Ok(()), Err)
}

async fn arm(runner: &Runner, options: &FphBenchOptions, with_hinting: bool, label: &str) -> Vec<Sample> {
    let mut samples = Vec::new();
    for iteration in 1..=options.iterations {
        if runner.cancel.is_cancelled() {
            break;
        }
        let sample = once(runner, options, with_hinting).await;
        match &sample.error {
            Some(error) => println!("[{iteration}/{}] {label}: {error}", options.iterations),
            None => println!(
                "[{iteration}/{}] {label}: memfile {}  fpr_freed {}  fph_freed {}  pause {:?}",
                options.iterations,
                mib(sample.memfile_bytes),
                mib(sample.report_bytes),
                mib(sample.hint_bytes),
                Duration::from_millis(sample.pause.as_millis() as u64)
            ),
        }
        samples.push(sample);
    }
    samples
}

async fn once(runner: &Runner, options: &FphBenchOptions, with_hinting: bool) -> Sample {
    let hinting = match with_hinting {
        true => json!({"enabled": true, "pause": DRAIN_TIMEOUT.as_millis() as u64}),
        false => Value::Null,
    };
    runner.node.feature_flags.set_override(HINTING_FLAG, hinting);
    let build_id = Uuid::new_v4();
    let measured = measure(runner, options, build_id).await;
    if let Some(dir) = local_build_dir(runner, build_id) {
        let _ = std::fs::remove_dir_all(dir);
    }
    measured.unwrap_or_else(|(pause, error)| Sample {
        pause,
        error: Some(error),
        ..Sample::default()
    })
}

async fn measure(runner: &Runner, options: &FphBenchOptions, build_id: Uuid) -> Result<Sample, (Duration, String)> {
    let id = format!("fph-bench-{}", now_nanos());
    let started = runner
        .start_from(&runner.build_id, id, Duration::from_secs(24 * 60 * 60))
        .await
        .map_err(|e| (Duration::ZERO, format!("resume: {e}")))?;
    let paused = pause_after_workload(runner, options, &started.id, build_id).await;
    if paused.is_err() {
        runner.close(&started.id).await;
    }
    let (pause, balloon) = paused?;
    if runner.node.uploads.wait(build_id).await == Outcome::Failed {
        return Err((pause, "upload: snapshot upload did not land".to_owned()));
    }
    let memfile_bytes = local_memfile_size(runner, build_id).map_err(|e| (pause, format!("memfile size: {e}")))?;
    Ok(Sample {
        memfile_bytes,
        hint_bytes: balloon.map_or(0, |totals| totals.hint_freed),
        report_bytes: balloon.map_or(0, |totals| totals.report_freed),
        pause,
        error: None,
    })
}

type PauseResult = Result<(Duration, Option<crate::firecracker::metrics::BalloonTotals>), (Duration, String)>;

async fn pause_after_workload(runner: &Runner, options: &FphBenchOptions, id: &str, build_id: Uuid) -> PauseResult {
    let handle = runner
        .node
        .registry
        .get(id)
        .ok_or_else(|| (Duration::ZERO, format!("resume: sandbox {id} is gone")))?;
    let guest = runner
        .guest_of(id)
        .ok_or_else(|| (Duration::ZERO, format!("resume: sandbox {id} is gone")))?;
    guest
        .run(&options.workload)
        .await
        .map_err(|e| (Duration::ZERO, format!("workload: {e}")))?;
    if !options.delay.is_zero() {
        tokio::select! {
            () = runner.cancel.cancelled() => return Err((Duration::ZERO, "context canceled".to_owned())),
            () = tokio::time::sleep(options.delay) => {}
        }
    }
    handle.record_balloon_after_pause();
    let started_at = Instant::now();
    let paused = runner
        .server
        .pause(Request::new(SandboxPauseRequest {
            sandbox_id: id.to_owned(),
            template_id: runner.build_id.clone(),
            build_id: build_id.to_string(),
            filesystem_only: false,
        }))
        .await;
    let pause = started_at.elapsed();
    paused.map_err(|status| (pause, format!("pause: {}", status.message())))?;
    Ok((pause, handle.balloon_after_pause()))
}

fn local_build_dir(runner: &Runner, build_id: Uuid) -> Option<PathBuf> {
    (runner.template_storage.provider == Provider::Local)
        .then(|| PathBuf::from(&runner.template_storage.base_path).join(build_id.to_string()))
}

fn local_memfile_size(runner: &Runner, build_id: Uuid) -> Result<u64, String> {
    let storage = &runner.template_storage;
    if storage.provider != Provider::Local {
        return Err(format!(
            "-fph-bench requires local storage: template storage provider is {:?}, not local",
            storage.provider
        ));
    }
    let path = PathBuf::from(&storage.base_path).join(BuildPaths::new(build_id).memfile());
    std::fs::metadata(&path)
        .map(|metadata| metadata.blocks() * 512)
        .map_err(|e| format!("stat {}: {e}", path.display()))
}

fn average(samples: &[Sample], value: impl Fn(&Sample) -> u128) -> u128 {
    let succeeded: Vec<u128> = samples
        .iter()
        .filter(|sample| sample.error.is_none())
        .map(value)
        .collect();
    match succeeded.len() {
        0 => 0,
        count => succeeded.iter().sum::<u128>() / count as u128,
    }
}

fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1 << 20) as f64)
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn averages_skip_failed_samples_and_sizes_print_in_mib() {
        let samples = [
            Sample {
                memfile_bytes: 2 << 20,
                ..Sample::default()
            },
            Sample {
                memfile_bytes: 4 << 20,
                ..Sample::default()
            },
            Sample {
                memfile_bytes: 100 << 20,
                error: Some("boom".to_owned()),
                ..Sample::default()
            },
        ];

        assert_eq!(average(&samples, |sample| sample.memfile_bytes as u128), 3 << 20);
        assert_eq!(mib(3 << 20), "3.0 MiB");
        assert_eq!(average(&[], |sample| sample.memfile_bytes as u128), 0);
    }
}
