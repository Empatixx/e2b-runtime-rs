use std::time::Duration;

const CONFINEMENT: [&str; 36] = [
    "--property=KillSignal=SIGKILL",
    "--property=TimeoutStopSec=10s",
    "--property=DynamicUser=yes",
    "--property=SupplementaryGroups=disk",
    "--property=ProtectProc=invisible",
    "--property=ProcSubset=pid",
    "--property=PrivateNetwork=yes",
    "--property=PrivateIPC=yes",
    "--property=ProtectHome=yes",
    "--property=NoNewPrivileges=yes",
    "--property=CapabilityBoundingSet=",
    "--property=AmbientCapabilities=",
    "--property=RestrictNamespaces=yes",
    "--property=SystemCallFilter=@system-service",
    "--property=SystemCallFilter=~@privileged @resources",
    "--property=SystemCallArchitectures=native",
    "--property=RestrictAddressFamilies=AF_UNIX",
    "--property=LockPersonality=yes",
    "--property=ProtectClock=yes",
    "--property=ProtectKernelTunables=yes",
    "--property=ProtectKernelModules=yes",
    "--property=ProtectKernelLogs=yes",
    "--property=ProtectControlGroups=yes",
    "--property=ProtectHostname=yes",
    "--property=RestrictRealtime=yes",
    "--property=MemoryDenyWriteExecute=yes",
    "--property=IPAddressDeny=any",
    "--property=Environment=",
    "--property=TemporaryFileSystem=/",
    "--property=BindReadOnlyPaths=/usr",
    "--property=BindReadOnlyPaths=/usr/lib64:/lib64",
    "--property=BindReadOnlyPaths=/usr/lib:/lib",
    "--property=BindReadOnlyPaths=/usr/bin:/bin",
    "--property=BindReadOnlyPaths=/usr/sbin:/sbin",
    "--property=MountAPIVFS=yes",
    "--property=PrivateDevices=yes",
];

pub fn jail_properties(unit: &str, runtime_max: Duration, device: &str, extra_binds: &[&str]) -> Vec<String> {
    let head = [
        "--wait".to_owned(),
        "--pipe".to_owned(),
        "--collect".to_owned(),
        "--quiet".to_owned(),
        format!("--unit={unit}"),
        format!("--property=RuntimeMaxSec={}", runtime_max.as_secs()),
    ];
    let device_access = [
        format!("--property=DeviceAllow={device} rw"),
        format!("--property=BindPaths={device}"),
    ];
    head.into_iter()
        .chain(CONFINEMENT.iter().map(|property| (*property).to_owned()))
        .chain(device_access)
        .chain(extra_binds.iter().map(|bind| format!("--property=BindPaths={bind}")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_jail_matches_the_upstream_properties_in_order() {
        let properties = jail_properties("fs-recover-1", Duration::from_secs(15), "/dev/nbd3", &["/stage"]);

        assert_eq!(
            properties[..6],
            [
                "--wait",
                "--pipe",
                "--collect",
                "--quiet",
                "--unit=fs-recover-1",
                "--property=RuntimeMaxSec=15"
            ]
        );
        assert_eq!(properties[6], "--property=KillSignal=SIGKILL");
        assert_eq!(properties[41], "--property=PrivateDevices=yes");
        assert_eq!(
            properties[42..],
            [
                "--property=DeviceAllow=/dev/nbd3 rw",
                "--property=BindPaths=/dev/nbd3",
                "--property=BindPaths=/stage"
            ]
        );
    }
}
