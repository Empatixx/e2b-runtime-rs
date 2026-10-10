use std::io;
use std::path::Path;

pub const MAX_VCPUS: i64 = 32;
pub const VCPU_THREAD_PREFIX: &str = "fc_vcpu";
const EXCEEDS_SNAPSHOT: &str = "requested vcpu exceeds snapshot capacity";

pub fn valid_machine_vcpus(vcpus: i64) -> bool {
    (1..=MAX_VCPUS).contains(&vcpus) && (std::env::consts::ARCH == "aarch64" || vcpus == 1 || vcpus % 2 == 0)
}

pub fn exceeds_snapshot_error(snapshot_vcpus: i64, requested_vcpus: i64) -> String {
    format!("{EXCEEDS_SNAPSHOT}: snapshot has {snapshot_vcpus} vcpus, request asks for {requested_vcpus}")
}

pub fn is_exceeds_snapshot(error: &str) -> bool {
    error.contains(EXCEEDS_SNAPSHOT)
}

pub fn vcpu_cap_resume_error(error: Option<String>, refuse_smaller_vm: bool) -> Result<(), String> {
    error
        .filter(|error| refuse_smaller_vm || !is_exceeds_snapshot(error))
        .map_or(Ok(()), Err)
}

pub fn vcpu_thread_count(process_dir: &Path) -> Result<i64, String> {
    let tasks = std::fs::read_dir(process_dir.join("task")).map_err(|e| format!("failed to list threads: {e}"))?;
    let count = tasks
        .map(|task| {
            let task = task.map_err(|e| format!("failed to list threads: {e}"))?;
            match std::fs::read_to_string(task.path().join("comm")) {
                Ok(name) => Ok(name.starts_with(VCPU_THREAD_PREFIX)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
                Err(e) => Err(format!("failed to read thread name: {e}")),
            }
        })
        .try_fold(0, |count, is_vcpu| is_vcpu.map(|is_vcpu| count + i64::from(is_vcpu)))?;
    match count {
        0 => Err("no firecracker vcpu threads".to_owned()),
        count => Ok(count),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_proc(comms: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (index, comm) in comms.iter().enumerate() {
            let task = dir.path().join("task").join(index.to_string());
            std::fs::create_dir_all(&task).unwrap();
            std::fs::write(task.join("comm"), format!("{comm}\n")).unwrap();
        }
        dir
    }

    #[test]
    fn machine_sizes_are_one_to_the_maximum_and_pairs_on_x86() {
        for vcpus in [1, 2, 4, MAX_VCPUS] {
            assert!(valid_machine_vcpus(vcpus), "VM size {vcpus}");
        }
        for vcpus in [0, -1, MAX_VCPUS + 1] {
            assert!(!valid_machine_vcpus(vcpus), "VM size {vcpus}");
        }
        assert_eq!(std::env::consts::ARCH == "aarch64", valid_machine_vcpus(3));
    }

    #[test]
    fn only_the_vcpu_threads_count() {
        let dir = fake_proc(&[
            "firecracker",
            "fc_api",
            "fc_vcpu 0",
            "fc_vcpu 1",
            "fc_vcpu 2",
            "fc_vcpu 3",
        ]);

        assert_eq!(vcpu_thread_count(dir.path()), Ok(4));
    }

    #[test]
    fn a_vm_without_vcpu_threads_is_not_a_size() {
        assert!(vcpu_thread_count(fake_proc(&["firecracker", "fc_api"]).path()).is_err());
        assert!(vcpu_thread_count(&std::env::temp_dir().join("gone-vm")).is_err());
    }

    #[test]
    fn a_smaller_vm_fails_the_cap_only_when_a_size_was_given() {
        let smaller = exceeds_snapshot_error(16, 20);

        assert_eq!(vcpu_cap_resume_error(Some(smaller.clone()), true), Err(smaller.clone()));
        assert_eq!(vcpu_cap_resume_error(Some(smaller), false), Ok(()));
        assert!(vcpu_cap_resume_error(Some("cgroup write failed".to_owned()), false).is_err());
        assert_eq!(vcpu_cap_resume_error(None, true), Ok(()));
    }

    #[test]
    fn the_refusal_names_both_sizes() {
        let error = exceeds_snapshot_error(2, 4);

        assert!(is_exceeds_snapshot(&error));
        assert!(error.contains("snapshot has 2 vcpus, request asks for 4"), "{error}");
    }
}
