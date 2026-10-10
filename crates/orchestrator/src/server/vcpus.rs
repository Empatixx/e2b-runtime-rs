use tonic::Status;

use crate::firecracker::vcpus::MAX_VCPUS;

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
}
