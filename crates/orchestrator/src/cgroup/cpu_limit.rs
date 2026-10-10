use std::path::{Path, PathBuf};

use super::Cgroup;
use crate::firecracker::vcpus::VCPU_THREAD_PREFIX;

pub const NO_CGROUP: &str = "sandbox has no cgroup to limit";
const VCPU_CGROUP_NAME: &str = "vcpu";
const CPU_PERIOD_USEC: i64 = 100_000;

pub fn set_sandbox_cpu_limit(cgroup: Option<&Cgroup>, limit_cpus: i64) -> Result<(), String> {
    match cgroup {
        Some(cgroup) => cgroup.write_sandbox_cpu_limit(limit_cpus),
        None if limit_cpus > 0 => Err(NO_CGROUP.to_owned()),
        None => Ok(()),
    }
}

pub fn confine_vcpu_threads(
    cgroup: Option<&mut Cgroup>,
    target_online_vcpus: i64,
    firecracker_vcpu_count: i64,
) -> Result<(), String> {
    if target_online_vcpus <= 0 || firecracker_vcpu_count <= target_online_vcpus {
        return Ok(());
    }
    cgroup
        .ok_or_else(|| NO_CGROUP.to_owned())?
        .confine_vcpu_threads(target_online_vcpus, firecracker_vcpu_count)
}

pub fn cpu_max_value(limit_cpus: i64) -> String {
    match limit_cpus <= 0 {
        true => "max".to_owned(),
        false => format!("{} {CPU_PERIOD_USEC}", limit_cpus * CPU_PERIOD_USEC),
    }
}

pub fn parse_unique_thread_ids(text: &str) -> Vec<u32> {
    let mut thread_ids: Vec<u32> = Vec::new();
    text.split_whitespace()
        .filter_map(|field| field.parse().ok())
        .for_each(|thread_id| {
            if !thread_ids.contains(&thread_id) {
                thread_ids.push(thread_id);
            }
        });
    thread_ids
}

impl Cgroup {
    fn write_sandbox_cpu_limit(&self, limit_cpus: i64) -> Result<(), String> {
        std::fs::write(self.path.join("cpu.max"), cpu_max_value(limit_cpus))
            .map_err(|e| format!("failed to write cpu.max: {e}"))?;
        tracing::debug!(cgroup_path = %self.path.display(), cpus = limit_cpus, "limited sandbox cgroup");
        Ok(())
    }

    fn confine_vcpu_threads(&mut self, target_online_vcpus: i64, firecracker_vcpu_count: i64) -> Result<(), String> {
        if self.vcpu_threads_confined {
            return self.write_vcpu_cpu_limit(target_online_vcpus);
        }
        let thread_ids = self.vcpu_thread_ids()?;
        if thread_ids.len() as i64 != firecracker_vcpu_count {
            return Err(format!(
                "vcpu threads in sandbox cgroup do not match the vm: found {} of {firecracker_vcpu_count}",
                thread_ids.len()
            ));
        }
        self.create_vcpu_cgroup()?;
        self.write_vcpu_cpu_limit(target_online_vcpus)?;
        let moved = self.move_vcpu_threads(&thread_ids)?;
        if moved as i64 != firecracker_vcpu_count {
            return Err(format!(
                "vcpu threads moved to child cgroup do not match the vm: moved {moved} of {firecracker_vcpu_count}"
            ));
        }
        self.vcpu_threads_confined = true;
        tracing::debug!(cgroup_path = %self.path.display(), cpus = target_online_vcpus, threads_moved = moved, "confined vcpu threads");
        Ok(())
    }

    fn vcpu_cgroup_path(&self) -> PathBuf {
        self.path.join(VCPU_CGROUP_NAME)
    }

    fn create_vcpu_cgroup(&self) -> Result<(), String> {
        let path = self.vcpu_cgroup_path();
        std::fs::create_dir(&path).map_err(|e| format!("failed to create vcpu cgroup: {e}"))?;
        self.make_vcpu_cgroup_threaded(&path).inspect_err(|_| {
            let _ = std::fs::remove_dir(&path);
        })
    }

    fn make_vcpu_cgroup_threaded(&self, vcpu_path: &Path) -> Result<(), String> {
        std::fs::write(vcpu_path.join("cgroup.type"), "threaded")
            .map_err(|e| format!("failed to make vcpu cgroup threaded: {e}"))?;
        std::fs::write(self.path.join("cgroup.subtree_control"), "+cpu")
            .map_err(|e| format!("failed to enable cpu controller for vcpu cgroup: {e}"))
    }

    fn write_vcpu_cpu_limit(&self, limit_cpus: i64) -> Result<(), String> {
        std::fs::write(self.vcpu_cgroup_path().join("cpu.max"), cpu_max_value(limit_cpus))
            .map_err(|e| format!("failed to write vcpu cpu.max: {e}"))
    }

    fn move_vcpu_threads(&self, thread_ids: &[u32]) -> Result<usize, String> {
        let threads_path = self.vcpu_cgroup_path().join("cgroup.threads");
        thread_ids.iter().try_fold(0, |moved, thread_id| {
            match std::fs::write(&threads_path, thread_id.to_string()) {
                Ok(()) => Ok(moved + 1),
                Err(e) if e.raw_os_error() == Some(nix::errno::Errno::ESRCH as i32) => Ok(moved),
                Err(e) => Err(format!("failed to move thread {thread_id} into vcpu cgroup: {e}")),
            }
        })
    }

    fn vcpu_thread_ids(&self) -> Result<Vec<u32>, String> {
        let threads = std::fs::read_to_string(self.path.join("cgroup.threads"))
            .map_err(|e| format!("failed to read cgroup.threads: {e}"))?;
        Ok(parse_unique_thread_ids(&threads)
            .into_iter()
            .filter(|thread_id| self.is_vcpu_thread(*thread_id))
            .collect())
    }

    fn is_vcpu_thread(&self, thread_id: u32) -> bool {
        std::fs::read_to_string(self.proc_root.join(thread_id.to_string()).join("comm"))
            .is_ok_and(|name| name.starts_with(VCPU_THREAD_PREFIX))
    }

    pub(super) fn remove_vcpu_cgroup(&self) -> std::io::Result<()> {
        match std::fs::remove_dir(self.vcpu_cgroup_path()) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cgroup_at(path: &Path, proc_root: &Path) -> Cgroup {
        Cgroup {
            path: path.to_owned(),
            peak: None,
            proc_root: proc_root.to_owned(),
            vcpu_threads_confined: false,
        }
    }

    fn file(dir: &Path, name: &str) -> String {
        std::fs::read_to_string(dir.join(name)).unwrap().trim().to_owned()
    }

    fn fake_vm(comms: &[&str]) -> (tempfile::TempDir, tempfile::TempDir) {
        let cgroup = tempfile::tempdir().unwrap();
        let proc = tempfile::tempdir().unwrap();
        let mut threads = String::new();
        for (index, comm) in comms.iter().enumerate() {
            let thread_id = 100 + index;
            std::fs::create_dir(proc.path().join(thread_id.to_string())).unwrap();
            std::fs::write(
                proc.path().join(thread_id.to_string()).join("comm"),
                format!("{comm}\n"),
            )
            .unwrap();
            threads.push_str(&format!("{thread_id}\n"));
        }
        std::fs::write(cgroup.path().join("cgroup.threads"), threads).unwrap();
        (cgroup, proc)
    }

    #[test]
    fn limits_without_a_cgroup_are_refused_unless_they_lift_or_cannot_bind() {
        assert_eq!(set_sandbox_cpu_limit(None, 2), Err(NO_CGROUP.to_owned()));
        assert_eq!(confine_vcpu_threads(None, 2, 4), Err(NO_CGROUP.to_owned()));
        assert_eq!(set_sandbox_cpu_limit(None, 0), Ok(()));
        assert_eq!(confine_vcpu_threads(None, 0, 4), Ok(()));
        assert_eq!(confine_vcpu_threads(None, 2, 2), Ok(()));
    }

    #[test]
    fn the_sandbox_limit_is_a_quota_over_a_period_and_zero_lifts_it() {
        let dir = tempfile::tempdir().unwrap();
        let cgroup = cgroup_at(dir.path(), dir.path());

        set_sandbox_cpu_limit(Some(&cgroup), 2).unwrap();
        assert_eq!(file(dir.path(), "cpu.max"), "200000 100000");

        set_sandbox_cpu_limit(Some(&cgroup), 0).unwrap();
        assert_eq!(file(dir.path(), "cpu.max"), "max");
    }

    #[test]
    fn a_completed_move_only_changes_the_quota() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(VCPU_CGROUP_NAME)).unwrap();
        let mut cgroup = cgroup_at(dir.path(), dir.path());
        cgroup.vcpu_threads_confined = true;

        confine_vcpu_threads(Some(&mut cgroup), 1, 2).unwrap();

        assert_eq!(file(&cgroup.vcpu_cgroup_path(), "cpu.max"), "100000 100000");
        assert!(cgroup.vcpu_threads_confined);
    }

    #[test]
    fn thread_ids_are_deduplicated_in_order() {
        assert_eq!(parse_unique_thread_ids("12\n7\n12\ninvalid\n7\n"), [12, 7]);
    }

    #[test]
    fn only_the_vcpu_threads_of_a_vm_that_matches_are_confined() {
        let (dir, proc) = fake_vm(&["fc_vcpu 0", "fc_vcpu 1", "fc_api"]);
        let mut cgroup = cgroup_at(dir.path(), proc.path());
        assert!(
            !cgroup.vcpu_cgroup_path().exists(),
            "creating the cgroup leaves it without a quota"
        );

        confine_vcpu_threads(Some(&mut cgroup), 2, 2).unwrap();
        assert!(
            !cgroup.vcpu_cgroup_path().exists(),
            "two vCPU threads cannot exceed two CPUs"
        );
        assert!(!cgroup.vcpu_threads_confined);

        set_sandbox_cpu_limit(Some(&cgroup), 1).unwrap();
        let error = confine_vcpu_threads(Some(&mut cgroup), 1, 4).unwrap_err();
        assert!(error.ends_with("found 2 of 4"), "{error}");
        assert!(!cgroup.vcpu_cgroup_path().exists());
        assert!(!cgroup.vcpu_threads_confined);

        confine_vcpu_threads(Some(&mut cgroup), 1, 2).unwrap();
        assert!(cgroup.vcpu_threads_confined);
        let vcpu = cgroup.vcpu_cgroup_path();
        assert_eq!(file(&vcpu, "cgroup.type"), "threaded");
        assert_eq!(file(&vcpu, "cpu.max"), "100000 100000");
        assert_eq!(file(&vcpu, "cgroup.threads"), "101");
        assert_eq!(file(dir.path(), "cgroup.subtree_control"), "+cpu");
        assert_eq!(
            file(dir.path(), "cpu.max"),
            "100000 100000",
            "confining alone leaves the cgroup-wide limit"
        );

        set_sandbox_cpu_limit(Some(&cgroup), 0).unwrap();
        assert_eq!(file(dir.path(), "cpu.max"), "max");
        confine_vcpu_threads(Some(&mut cgroup), 1, 2).unwrap();
    }

    #[test]
    fn a_child_left_by_an_incomplete_move_keeps_the_whole_sandbox_fallback() {
        let (dir, proc) = fake_vm(&["fc_vcpu 0", "fc_vcpu 1"]);
        let mut cgroup = cgroup_at(dir.path(), proc.path());
        set_sandbox_cpu_limit(Some(&cgroup), 1).unwrap();
        std::fs::create_dir(cgroup.vcpu_cgroup_path()).unwrap();

        let error = confine_vcpu_threads(Some(&mut cgroup), 1, 2).unwrap_err();

        assert!(error.starts_with("failed to create vcpu cgroup"), "{error}");
        assert!(!cgroup.vcpu_threads_confined);
        assert_eq!(file(dir.path(), "cpu.max"), "100000 100000");
    }

    #[test]
    fn a_cgroup_without_threads_keeps_the_cgroup_wide_limit() {
        let (dir, proc) = fake_vm(&[]);
        let mut cgroup = cgroup_at(dir.path(), proc.path());
        set_sandbox_cpu_limit(Some(&cgroup), 1).unwrap();

        let error = confine_vcpu_threads(Some(&mut cgroup), 1, 4).unwrap_err();

        assert!(error.ends_with("found 0 of 4"), "{error}");
        assert_eq!(file(dir.path(), "cpu.max"), "100000 100000");
    }

    #[test]
    fn removing_deletes_the_vcpu_child_before_the_parent() {
        let parent = tempfile::tempdir().unwrap().keep();
        std::fs::create_dir(parent.join(VCPU_CGROUP_NAME)).unwrap();

        cgroup_at(&parent, &parent).remove().unwrap();

        assert!(!parent.exists());
    }
}
