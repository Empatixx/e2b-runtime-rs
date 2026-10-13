use std::collections::BTreeMap;

use crate::network::slot::{GUEST_IP, TAP_IP, TAP_NAME};

const GUEST_NETMASK: &str = "255.255.255.252";
const GUEST_DEVICE: &str = "eth0";
const EXT4_ROOT_FLAGS: &str = "discard";
const RESERVED: [&str; 15] = [
    "maxcpus",
    "nr_cpus",
    "possible_cpus",
    "nosmp",
    "init",
    "clocksource",
    "root",
    "ip",
    "console",
    "rootflags",
    "panic",
    "reboot",
    "loglevel",
    "quiet",
    "selinux",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootOptions {
    pub init: String,
    pub kernel_logs: bool,
    pub systemd_to_kernel_logs: bool,
    pub kvm_clock: bool,
    pub boot_cpus: i64,
    pub cmdline_args: BTreeMap<String, String>,
}

impl BootOptions {
    pub fn new(init: &str) -> Self {
        Self {
            init: init.to_owned(),
            kernel_logs: false,
            systemd_to_kernel_logs: false,
            kvm_clock: true,
            boot_cpus: 0,
            cmdline_args: BTreeMap::new(),
        }
    }

    pub fn kernel_args(&self, arch: &str) -> String {
        let mut args: BTreeMap<String, String> = [
            ("quiet", ""),
            ("loglevel", "1"),
            ("init", self.init.as_str()),
            ("ip", ip_arg().as_str()),
            ("ipv6.disable", "0"),
            ("ipv6.autoconf", "1"),
            ("panic", "1"),
            ("reboot", "k"),
            ("pci", "off"),
            ("random.trust_cpu", "on"),
            ("rootflags", EXT4_ROOT_FLAGS),
            ("selinux", "0"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect();
        if arch == "amd64" {
            args.insert("i8042.nokbd".to_owned(), String::new());
            args.insert("i8042.noaux".to_owned(), String::new());
            if self.kvm_clock {
                args.insert("clocksource".to_owned(), "kvm-clock".to_owned());
            }
        }
        if self.systemd_to_kernel_logs {
            args.insert("systemd.journald.forward_to_console".to_owned(), String::new());
        }
        if self.boot_cpus > 0 {
            args.insert("maxcpus".to_owned(), self.boot_cpus.to_string());
        }
        if self.kernel_logs || self.systemd_to_kernel_logs {
            args.remove("quiet");
            args.insert("console".to_owned(), "ttyS0".to_owned());
            args.insert("loglevel".to_owned(), "5".to_owned());
        }
        if validate_cmdline_args(&self.cmdline_args).is_ok() {
            args.extend(self.cmdline_args.clone());
        }
        render(&args)
    }
}

pub fn parse_cmdline_args(fragment: &str) -> Result<BTreeMap<String, String>, String> {
    fragment
        .split_whitespace()
        .map(|field| {
            let (name, value) = field.split_once('=').unwrap_or((field, ""));
            match name.is_empty() {
                true => Err(format!("guest kernel cmdline parameter {field:?} has an empty name")),
                false => Ok((name.to_owned(), value.to_owned())),
            }
        })
        .collect()
}

pub fn validate_cmdline_args(args: &BTreeMap<String, String>) -> Result<(), String> {
    args.keys()
        .find(|name| RESERVED.contains(&name.as_str()))
        .map_or(Ok(()), |name| {
            Err(format!(
                "guest kernel cmdline parameter {name:?} is reserved by the orchestrator"
            ))
        })
}

pub fn render(args: &BTreeMap<String, String>) -> String {
    let mut parts: Vec<String> = args
        .iter()
        .map(|(key, value)| match value.is_empty() {
            true => key.clone(),
            false => format!("{key}={value}"),
        })
        .collect();
    parts.sort();
    parts.join(" ")
}

fn ip_arg() -> String {
    format!("{GUEST_IP}::{TAP_IP}:{GUEST_NETMASK}:instance:{GUEST_DEVICE}:off:{TAP_NAME}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provisioning_args_match_upstream() {
        let options = BootOptions {
            kernel_logs: true,
            ..BootOptions::new("usr/bin/init")
        };

        assert_eq!(
            options.kernel_args("amd64"),
            "clocksource=kvm-clock console=ttyS0 i8042.noaux i8042.nokbd init=usr/bin/init ip=169.254.0.21::169.254.0.22:255.255.255.252:instance:eth0:off:tap0 ipv6.autoconf=1 ipv6.disable=0 loglevel=5 panic=1 pci=off random.trust_cpu=on reboot=k rootflags=discard selinux=0"
        );
    }

    #[test]
    fn quiet_by_default_and_reserved_overrides_are_ignored() {
        let options = BootOptions {
            kvm_clock: false,
            cmdline_args: BTreeMap::from([("init".to_owned(), "/bin/sh".to_owned())]),
            ..BootOptions::new("/sbin/init")
        };

        assert_eq!(
            options.kernel_args("arm64"),
            "init=/sbin/init ip=169.254.0.21::169.254.0.22:255.255.255.252:instance:eth0:off:tap0 ipv6.autoconf=1 ipv6.disable=0 loglevel=1 panic=1 pci=off quiet random.trust_cpu=on reboot=k rootflags=discard selinux=0"
        );
        assert_eq!(
            validate_cmdline_args(&options.cmdline_args),
            Err("guest kernel cmdline parameter \"init\" is reserved by the orchestrator".to_owned())
        );
    }

    #[test]
    fn a_fragment_parses_like_go() {
        let args = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
            pairs
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect()
        };
        let cases = [
            ("", args(&[])),
            ("   \t ", args(&[])),
            ("psi=1", args(&[("psi", "1")])),
            ("psi=1 nokaslr", args(&[("psi", "1"), ("nokaslr", "")])),
            ("  psi=1   nokaslr  ", args(&[("psi", "1"), ("nokaslr", "")])),
            ("foo=a=b", args(&[("foo", "a=b")])),
            ("init=/evil", args(&[("init", "/evil")])),
        ];
        for (fragment, want) in cases {
            assert_eq!(parse_cmdline_args(fragment), Ok(want), "fragment {fragment:?}");
        }
        assert_eq!(
            parse_cmdline_args("=x"),
            Err("guest kernel cmdline parameter \"=x\" has an empty name".to_owned())
        );
    }

    #[test]
    fn parsing_then_validating_rejects_reserved_parameters() {
        for fragment in ["init=/evil", "psi=1 init=/evil", "clocksource=tsc"] {
            let parsed = parse_cmdline_args(fragment).unwrap();
            assert!(validate_cmdline_args(&parsed).is_err(), "fragment {fragment:?}");
        }
    }

    #[test]
    fn every_cpu_parameter_the_orchestrator_owns_is_reserved() {
        for name in ["maxcpus", "nr_cpus", "possible_cpus", "nosmp"] {
            let args = BTreeMap::from([(name.to_owned(), "1".to_owned())]);

            assert_eq!(
                validate_cmdline_args(&args),
                Err(format!(
                    "guest kernel cmdline parameter \"{name}\" is reserved by the orchestrator"
                ))
            );
        }
    }

    #[test]
    fn boot_cpus_cap_the_cpus_the_guest_brings_up() {
        let options = BootOptions {
            boot_cpus: 2,
            cmdline_args: BTreeMap::from([("maxcpus".to_owned(), "16".to_owned())]),
            ..BootOptions::new("/sbin/init")
        };

        assert!(
            options.kernel_args("arm64").contains(" maxcpus=2 "),
            "{}",
            options.kernel_args("arm64")
        );
        assert!(!BootOptions::new("/sbin/init").kernel_args("arm64").contains("maxcpus"));
    }
}
