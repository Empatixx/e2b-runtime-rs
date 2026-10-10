use std::path::Path;

use opentelemetry::KeyValue;

use crate::cgroup::Cgroup;
use crate::cgroup::cpu_limit::{confine_vcpu_threads, set_sandbox_cpu_limit};
use crate::firecracker::vcpus::{exceeds_snapshot_error, vcpu_thread_count};
use crate::telemetry::fc::record_vcpu_limit;
use crate::telemetry::report::report_error;

const PATH_BOOT: &str = "boot";
const PATH_RESUME: &str = "resume";
const OUTCOME_CONFINED: &str = "confined";
const OUTCOME_CGROUP_FALLBACK: &str = "cgroup_fallback";
const OUTCOME_FAILED: &str = "failed";

pub fn boot_cpu_quota(target_online_vcpus: i64, firecracker_vcpu_count: i64) -> i64 {
    match firecracker_vcpu_count > target_online_vcpus {
        true => target_online_vcpus,
        false => 0,
    }
}

pub fn resume_vm_vcpus(
    target_online_vcpus: i64,
    configured_vm_vcpus: i64,
    recorded_vm_vcpus: i64,
) -> Result<i64, String> {
    let resolved = match recorded_vm_vcpus {
        0 => target_online_vcpus.max(configured_vm_vcpus),
        recorded => recorded,
    };
    match target_online_vcpus > resolved && configured_vm_vcpus > 0 {
        true => Err(exceeds_snapshot_error(resolved, target_online_vcpus)),
        false => Ok(resolved),
    }
}

pub fn guest_cpu_count(resolved_vm_vcpus: i64, vcpus: i64) -> i64 {
    match resolved_vm_vcpus > 0 && resolved_vm_vcpus < vcpus {
        true => resolved_vm_vcpus,
        false => vcpus,
    }
}

pub fn resolve_vm_vcpus(firecracker_pid: u32, sandbox_id: &str, guess: i64, recorded: bool) -> i64 {
    if recorded {
        return guess;
    }
    vcpu_thread_count(&Path::new("/proc").join(firecracker_pid.to_string())).unwrap_or_else(|e| {
        report_error(
            "failed to count the snapshot's vcpu threads, using the requested size",
            Some(&e),
            &sandbox_attribute(sandbox_id),
        );
        guess
    })
}

pub fn limit_vcpus_before_resume(
    cgroup: Option<&mut Cgroup>,
    sandbox_id: &str,
    target_online_vcpus: i64,
    snapshot_vcpus: i64,
) -> Result<(), String> {
    if snapshot_vcpus < target_online_vcpus {
        return Err(exceeds_snapshot_error(snapshot_vcpus, target_online_vcpus));
    }
    if snapshot_vcpus == target_online_vcpus {
        return Ok(());
    }
    tracing::info_span!("limit-vcpus-before-resume").in_scope(|| {
        let limited = confine_or_limit_cgroup(cgroup, sandbox_id, target_online_vcpus, snapshot_vcpus);
        record_vcpu_limit(PATH_RESUME, limited.as_ref().map_or(OUTCOME_FAILED, |outcome| outcome));
        limited.map(|_| ())
    })
}

fn confine_or_limit_cgroup(
    mut cgroup: Option<&mut Cgroup>,
    sandbox_id: &str,
    target_online_vcpus: i64,
    snapshot_vcpus: i64,
) -> Result<&'static str, String> {
    let confined = confine_vcpu_threads(cgroup.as_deref_mut(), target_online_vcpus, snapshot_vcpus);
    let Err(e) = confined else {
        return Ok(OUTCOME_CONFINED);
    };
    report_error(
        "failed to confine vcpu threads, limiting the whole sandbox cgroup",
        Some(&e),
        &sandbox_attribute(sandbox_id),
    );
    set_sandbox_cpu_limit(cgroup.as_deref(), target_online_vcpus)
        .map(|()| OUTCOME_CGROUP_FALLBACK)
        .map_err(|e| format!("error limiting sandbox cgroup before resume: {e}"))
}

pub fn move_limit_to_vcpu_threads(
    cgroup: Option<&mut Cgroup>,
    sandbox_id: &str,
    cpu_quota: i64,
    firecracker_vcpu_count: i64,
) {
    if cpu_quota <= 0 {
        return;
    }
    tracing::info_span!("move-limit-to-vcpu-threads").in_scope(|| {
        let outcome = confine_then_lift(cgroup, sandbox_id, cpu_quota, firecracker_vcpu_count)
            .map_or(OUTCOME_CGROUP_FALLBACK, |()| OUTCOME_CONFINED);
        record_vcpu_limit(PATH_BOOT, outcome);
    });
}

fn confine_then_lift(
    mut cgroup: Option<&mut Cgroup>,
    sandbox_id: &str,
    cpu_quota: i64,
    firecracker_vcpu_count: i64,
) -> Result<(), String> {
    confine_vcpu_threads(cgroup.as_deref_mut(), cpu_quota, firecracker_vcpu_count)
        .inspect_err(|e| {
            report_error(
                "failed to confine vcpu threads, vmm threads share the cpu limit",
                Some(e),
                &sandbox_attribute(sandbox_id),
            )
        })
        .and_then(|()| {
            set_sandbox_cpu_limit(cgroup.as_deref(), 0).inspect_err(|e| {
                report_error(
                    "failed to lift the cgroup-wide cpu limit, vmm threads stay limited",
                    Some(e),
                    &sandbox_attribute(sandbox_id),
                )
            })
        })
}

pub fn record_boot_failure() {
    record_vcpu_limit(PATH_BOOT, OUTCOME_FAILED);
}

fn sandbox_attribute(sandbox_id: &str) -> [KeyValue; 1] {
    [KeyValue::new("sandbox.id", sandbox_id.to_owned())]
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use opentelemetry::global;
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

    use super::*;

    fn vcpu_limit_counts(exporter: &InMemoryMetricExporter) -> BTreeMap<(String, String), u64> {
        let mut counts = BTreeMap::new();
        exporter
            .get_finished_metrics()
            .unwrap()
            .iter()
            .flat_map(|resource| resource.scope_metrics())
            .flat_map(|scope| scope.metrics())
            .filter(|metric| metric.name() == "orchestrator.sandbox.vcpu_limit")
            .filter_map(|metric| match metric.data() {
                AggregatedMetrics::U64(MetricData::Sum(sum)) => Some(sum.data_points().collect::<Vec<_>>()),
                _ => None,
            })
            .flatten()
            .for_each(|point| {
                let attribute = |key: &str| {
                    point
                        .attributes()
                        .find(|attribute| attribute.key.as_str() == key)
                        .map(|attribute| attribute.value.as_str().into_owned())
                        .unwrap_or_default()
                };
                *counts.entry((attribute("path"), attribute("outcome"))).or_default() += point.value();
            });
        counts
    }

    #[test]
    fn a_missing_cgroup_drives_both_paths_to_their_worst_outcome_and_is_counted() {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .build();
        global::set_meter_provider(provider.clone());

        assert_eq!(
            limit_vcpus_before_resume(None, "test", 2, 2),
            Ok(()),
            "a VM no bigger than the target needs no limit"
        );
        assert!(
            limit_vcpus_before_resume(None, "test", 2, 16).is_err(),
            "no confine and no fallback aborts the resume"
        );
        move_limit_to_vcpu_threads(None, "test", 2, 16);
        provider.force_flush().unwrap();

        assert_eq!(
            vcpu_limit_counts(&exporter),
            BTreeMap::from([
                (("resume".to_owned(), "failed".to_owned()), 1),
                (("boot".to_owned(), "cgroup_fallback".to_owned()), 1),
            ])
        );
    }

    #[test]
    fn a_smaller_vm_than_the_request_is_refused_before_any_limit() {
        let error = limit_vcpus_before_resume(None, "test", 4, 2).unwrap_err();

        assert!(error.contains("requested vcpu exceeds snapshot capacity"), "{error}");
        assert!(error.contains("snapshot has 2 vcpus, request asks for 4"), "{error}");
    }

    #[test]
    fn a_recorded_size_is_used_as_is_without_counting_threads() {
        assert_eq!(resolve_vm_vcpus(u32::MAX, "test", 16, true), 16);
    }

    #[test]
    fn boot_quota_is_the_target_only_for_a_larger_vm() {
        assert_eq!(boot_cpu_quota(2, 4), 2);
        assert_eq!(boot_cpu_quota(2, 2), 0);
    }

    #[test]
    fn resume_sizes_follow_the_snapshot_and_refuse_an_impossible_upsize() {
        for (name, vcpu, max_vcpus, recorded, want) in [
            ("recorded capacity", 2, 0, 16, Some(16)),
            ("recorded capacity is authoritative", 2, 32, 16, Some(16)),
            ("legacy stored capacity", 2, 16, 0, Some(16)),
            ("legacy unchanged request", 2, 0, 0, Some(2)),
            ("impossible upsize while resizing", 8, 8, 4, None),
            ("mismatch without resizing resumes", 8, 0, 4, Some(4)),
        ] {
            let resolved = resume_vm_vcpus(vcpu, max_vcpus, recorded);
            match want {
                Some(want) => assert_eq!(resolved, Ok(want), "{name}"),
                None => assert!(
                    crate::firecracker::vcpus::is_exceeds_snapshot(&resolved.unwrap_err()),
                    "{name}"
                ),
            }
        }
    }

    #[test]
    fn the_guest_is_asked_for_the_vm_size_when_a_resume_allowed_a_smaller_vm() {
        assert_eq!(guest_cpu_count(0, 2), 2);
        assert_eq!(guest_cpu_count(4, 2), 2);
        assert_eq!(guest_cpu_count(4, 4), 4);
        assert_eq!(guest_cpu_count(16, 20), 16);
    }
}
