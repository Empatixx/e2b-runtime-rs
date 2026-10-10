use tonic::Status;

use crate::firecracker::vcpus::{MAX_VCPUS, valid_machine_vcpus};
use crate::template::metadata::TemplateMetadata;

pub fn validate_vcpus(vcpu: i64, max_vcpus: i64) -> Result<(), Status> {
    match (vcpu, max_vcpus) {
        (vcpu, _) if vcpu < 1 => Err(Status::invalid_argument(format!("vcpu {vcpu} must be at least 1"))),
        (vcpu, _) if vcpu > MAX_VCPUS => Err(Status::invalid_argument(format!(
            "vcpu {vcpu} is above the {MAX_VCPUS} a VM can have"
        ))),
        (vcpu, max_vcpus) if max_vcpus != 0 && max_vcpus < vcpu => Err(Status::invalid_argument(format!(
            "vcpu {vcpu} must be 1..max_vcpus {max_vcpus}"
        ))),
        (_, max_vcpus) if max_vcpus > MAX_VCPUS => Err(Status::invalid_argument(format!(
            "max_vcpus {max_vcpus} is above the {MAX_VCPUS} a VM can have"
        ))),
        _ => Ok(()),
    }
}

pub fn validate_cold_boot_vcpus(vcpu: i64, max_vcpus: i64, recorded_vcpus: i64) -> Result<(), Status> {
    let max_vcpus = match max_vcpus {
        0 => recorded_vcpus,
        explicit => explicit,
    };
    let machine_vcpus = vcpu.max(max_vcpus);
    match valid_machine_vcpus(machine_vcpus) {
        true => Ok(()),
        false => Err(Status::invalid_argument(format!(
            "cannot cold-boot a VM with {machine_vcpus} vcpus on this host"
        ))),
    }
}

pub fn filesystem_boot(metadata: Option<&TemplateMetadata>, requested: bool) -> bool {
    metadata.is_some_and(|metadata| metadata.filesystem_only) || requested
}

#[cfg(test)]
mod tests {
    use tonic::Code;

    use super::*;

    #[test]
    fn invalid_vcpu_limits_are_refused_with_the_upstream_texts() {
        for (name, vcpu, max_vcpus, text) in [
            ("zero vcpu without maximum", 0, 0, "vcpu 0 must be at least 1"),
            ("negative vcpu without maximum", -1, 0, "vcpu -1 must be at least 1"),
            ("maximum below vcpu", 4, 2, "vcpu 4 must be 1..max_vcpus 2"),
            (
                "maximum above firecracker's",
                2,
                33,
                "max_vcpus 33 is above the 32 a VM can have",
            ),
            (
                "vcpu above firecracker's",
                33,
                0,
                "vcpu 33 is above the 32 a VM can have",
            ),
        ] {
            let refused = validate_vcpus(vcpu, max_vcpus).expect_err(name);

            assert_eq!(refused.code(), Code::InvalidArgument, "{name}");
            assert_eq!(refused.message(), text, "{name}");
        }
    }

    #[test]
    fn sizes_inside_the_limits_are_accepted() {
        for (vcpu, max_vcpus) in [(1, 0), (2, 2), (2, 32), (32, 0), (32, 32)] {
            assert!(validate_vcpus(vcpu, max_vcpus).is_ok(), "{vcpu} {max_vcpus}");
        }
    }

    #[test]
    fn the_active_target_may_be_odd_when_the_vm_is_even_sized() {
        assert!(validate_cold_boot_vcpus(3, 4, 0).is_ok());
        assert!(
            validate_cold_boot_vcpus(2, 2, 4).is_ok(),
            "an explicit maximum may shrink the VM"
        );
        assert!(
            validate_cold_boot_vcpus(2, 0, 4).is_ok(),
            "an omitted maximum inherits the recorded capacity"
        );
    }

    #[test]
    fn an_x86_host_refuses_an_odd_vm_and_arm_accepts_it() {
        if std::env::consts::ARCH == "aarch64" {
            return;
        }
        for (vcpu, max_vcpus, recorded) in [(2, 3, 4), (2, 3, 0), (3, 0, 0)] {
            let refused = validate_cold_boot_vcpus(vcpu, max_vcpus, recorded).unwrap_err();

            assert_eq!(refused.code(), Code::InvalidArgument, "{vcpu} {max_vcpus} {recorded}");
        }
        assert_eq!(
            validate_cold_boot_vcpus(3, 0, 0).unwrap_err().message(),
            "cannot cold-boot a VM with 3 vcpus on this host"
        );
    }

    #[test]
    fn the_snapshot_or_the_request_selects_the_filesystem_boot() {
        for (name, filesystem_only, requested, want) in [
            ("memory snapshot, field absent: memory resume", false, false, false),
            ("memory snapshot, request demands cold boot: reboot", false, true, true),
            ("fs-only snapshot, field absent: reboot", true, false, true),
            ("fs-only snapshot, request demands cold boot: reboot", true, true, true),
            ("memory snapshot, explicit false: memory resume", false, false, false),
            (
                "fs-only snapshot, explicit false cannot force a memory restore",
                true,
                false,
                true,
            ),
        ] {
            let metadata = TemplateMetadata {
                filesystem_only,
                ..TemplateMetadata::default()
            };

            assert_eq!(filesystem_boot(Some(&metadata), requested), want, "{name}");
        }
    }
}
