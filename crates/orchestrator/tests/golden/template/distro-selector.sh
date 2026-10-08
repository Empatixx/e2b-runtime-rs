e2b_select_profile() {
  e2b_profile_matched=
  case "$1" in
  debian|ubuntu)
    E2B_PACKAGES="systemd systemd-sysv openssh-server sudo chrony socat curl ca-certificates fuse3 iptables git nfs-common less nftables iputils-ping jq iproute2"
    e2b_pkg_query() { dpkg-query -W -f='${Status}' "$1" 2>/dev/null | grep -q "install ok installed"; }
    e2b_pkg_install() { apt-get -q update && DEBIAN_FRONTEND=noninteractive DEBCONF_NOWARNINGS=yes apt-get -qq -o=Dpkg::Use-Pty=0 install -y --no-install-recommends "$@"; }
    E2B_INIT_BIN="/lib/systemd/systemd"
    E2B_TIMESYNC_UNIT="chrony"
    E2B_SSH_UNIT="ssh"
    E2B_ADMIN_GROUP="sudo"
    E2B_CA_BUNDLE="/etc/ssl/certs/ca-certificates.crt"
    e2b_ca_refresh() { update-ca-certificates; }
    E2B_INIT_SYSTEM="systemd"
    e2b_init_setup() {
        echo "Don't wait for ttyS0 (serial console kernel logs)"
        # This is required when the Firecracker kernel args has specified console=ttyS0
        systemctl mask serial-getty@ttyS0.service

        echo "Disable network online wait"
        systemctl mask systemd-networkd-wait-online.service

        echo "Disable system first boot wizard"
        # This was problem with Ubuntu 24.04, that differently calculate wizard should be called
        # and Linux boot was stuck in wizard until envd wait timeout
        systemctl mask systemd-firstboot.service

        echo "Enable time synchronization ($E2B_TIMESYNC_UNIT)"
        # Distro-correct chrony unit (chrony on Debian, chronyd on RHEL/Arch).
        systemctl enable "$E2B_TIMESYNC_UNIT"

        echo "Pull the boot-time time-source selector into $E2B_TIMESYNC_UNIT"
        # e2b-chrony-source.service writes the source line chrony.conf includes; the
        # unit that must pull it in is only known here (the name differs per family).
        mkdir -p "/etc/systemd/system/$E2B_TIMESYNC_UNIT.service.d"
        printf '[Unit]\nRequires=e2b-chrony-source.service\nAfter=e2b-chrony-source.service\n' \
            >"/etc/systemd/system/$E2B_TIMESYNC_UNIT.service.d/e2b-chrony-source.conf"

        echo "Enable SSH ($E2B_SSH_UNIT)"
        # provision.sh writes the sandbox sshd_config on every family, but nothing was
        # turning the unit on: Debian's postinst and the RHEL RPM scriptlet enable it
        # themselves, Arch does not, so Arch sandboxes shipped with SSH configured and
        # dead. Enabling is idempotent where the packaging already did it.
        systemctl enable "$E2B_SSH_UNIT.service"

        echo "Enable envd autostart"
        # Belt-and-suspenders with the baked 00-e2b.preset: on the RHEL family the
        # package transaction above runs systemd's RPM scriptlet 'systemctl preset-all'
        # (policy 'disable *'), which deletes the baked wants-symlink.
        systemctl enable envd.service

        echo "Disable chrony-wait"
        # chrony-wait blocks multi-user.target until the first clock sync (~8s);
        # chrony still syncs in the background, nothing needs to wait for it.
        # masking a unit that doesn't exist on this distro still succeeds (systemctl
        # mask just writes the /dev/null symlink), so a failure here is real.
        systemctl mask chrony-wait.service

        echo "Disable slow boot units not needed in the sandbox"
        # binfmt registrations (foreign-arch exec) take ~1s of CPU early in boot and
        # compete with envd start; e2scrub is for LVM-backed ext4 only.
        systemctl mask systemd-binfmt.service
        systemctl mask e2scrub_reap.service
    }
    e2b_profile_matched=1
    ;;
  fedora|centos|rocky|almalinux)
    E2B_PACKAGES="systemd shadow-utils passwd openssh-server sudo chrony socat curl ca-certificates fuse3 iptables git nfs-utils less nftables iputils jq bash tar iproute findutils"
    e2b_pkg_query() { rpm -q "$1" >/dev/null 2>&1; }
    e2b_pkg_install() { dnf -y install --allowerasing "$@" || microdnf -y install "$@" || yum -y install "$@"; }
    E2B_INIT_BIN="/usr/lib/systemd/systemd"
    E2B_TIMESYNC_UNIT="chronyd"
    E2B_SSH_UNIT="sshd"
    E2B_ADMIN_GROUP="wheel"
    E2B_CA_BUNDLE="/etc/ssl/certs/ca-certificates.crt"
    e2b_ca_refresh() { update-ca-trust extract && ln -sf /etc/pki/tls/certs/ca-bundle.crt "$E2B_CA_BUNDLE"; }
    E2B_INIT_SYSTEM="systemd"
    e2b_init_setup() {
        echo "Don't wait for ttyS0 (serial console kernel logs)"
        # This is required when the Firecracker kernel args has specified console=ttyS0
        systemctl mask serial-getty@ttyS0.service

        echo "Disable network online wait"
        systemctl mask systemd-networkd-wait-online.service

        echo "Disable system first boot wizard"
        # This was problem with Ubuntu 24.04, that differently calculate wizard should be called
        # and Linux boot was stuck in wizard until envd wait timeout
        systemctl mask systemd-firstboot.service

        echo "Enable time synchronization ($E2B_TIMESYNC_UNIT)"
        # Distro-correct chrony unit (chrony on Debian, chronyd on RHEL/Arch).
        systemctl enable "$E2B_TIMESYNC_UNIT"

        echo "Pull the boot-time time-source selector into $E2B_TIMESYNC_UNIT"
        # e2b-chrony-source.service writes the source line chrony.conf includes; the
        # unit that must pull it in is only known here (the name differs per family).
        mkdir -p "/etc/systemd/system/$E2B_TIMESYNC_UNIT.service.d"
        printf '[Unit]\nRequires=e2b-chrony-source.service\nAfter=e2b-chrony-source.service\n' \
            >"/etc/systemd/system/$E2B_TIMESYNC_UNIT.service.d/e2b-chrony-source.conf"

        echo "Enable SSH ($E2B_SSH_UNIT)"
        # provision.sh writes the sandbox sshd_config on every family, but nothing was
        # turning the unit on: Debian's postinst and the RHEL RPM scriptlet enable it
        # themselves, Arch does not, so Arch sandboxes shipped with SSH configured and
        # dead. Enabling is idempotent where the packaging already did it.
        systemctl enable "$E2B_SSH_UNIT.service"

        echo "Enable envd autostart"
        # Belt-and-suspenders with the baked 00-e2b.preset: on the RHEL family the
        # package transaction above runs systemd's RPM scriptlet 'systemctl preset-all'
        # (policy 'disable *'), which deletes the baked wants-symlink.
        systemctl enable envd.service

        echo "Disable chrony-wait"
        # chrony-wait blocks multi-user.target until the first clock sync (~8s);
        # chrony still syncs in the background, nothing needs to wait for it.
        # masking a unit that doesn't exist on this distro still succeeds (systemctl
        # mask just writes the /dev/null symlink), so a failure here is real.
        systemctl mask chrony-wait.service

        echo "Disable slow boot units not needed in the sandbox"
        # binfmt registrations (foreign-arch exec) take ~1s of CPU early in boot and
        # compete with envd start; e2scrub is for LVM-backed ext4 only.
        systemctl mask systemd-binfmt.service
        systemctl mask e2scrub_reap.service
    }
    e2b_profile_matched=1
    ;;
  arch|archarm)
    E2B_PACKAGES="systemd shadow openssh sudo chrony socat curl ca-certificates fuse3 iptables git nfs-utils less nftables iputils jq bash"
    e2b_pkg_query() { pacman -Q "$1" >/dev/null 2>&1; }
    e2b_pkg_install() { pacman -Syu --noconfirm --needed "$@"; }
    E2B_INIT_BIN="/usr/lib/systemd/systemd"
    E2B_TIMESYNC_UNIT="chronyd"
    E2B_SSH_UNIT="sshd"
    E2B_ADMIN_GROUP="wheel"
    E2B_CA_BUNDLE="/etc/ssl/certs/ca-certificates.crt"
    e2b_ca_refresh() { update-ca-trust extract; }
    E2B_INIT_SYSTEM="systemd"
    e2b_init_setup() {
        echo "Don't wait for ttyS0 (serial console kernel logs)"
        # This is required when the Firecracker kernel args has specified console=ttyS0
        systemctl mask serial-getty@ttyS0.service

        echo "Disable network online wait"
        systemctl mask systemd-networkd-wait-online.service

        echo "Disable system first boot wizard"
        # This was problem with Ubuntu 24.04, that differently calculate wizard should be called
        # and Linux boot was stuck in wizard until envd wait timeout
        systemctl mask systemd-firstboot.service

        echo "Enable time synchronization ($E2B_TIMESYNC_UNIT)"
        # Distro-correct chrony unit (chrony on Debian, chronyd on RHEL/Arch).
        systemctl enable "$E2B_TIMESYNC_UNIT"

        echo "Pull the boot-time time-source selector into $E2B_TIMESYNC_UNIT"
        # e2b-chrony-source.service writes the source line chrony.conf includes; the
        # unit that must pull it in is only known here (the name differs per family).
        mkdir -p "/etc/systemd/system/$E2B_TIMESYNC_UNIT.service.d"
        printf '[Unit]\nRequires=e2b-chrony-source.service\nAfter=e2b-chrony-source.service\n' \
            >"/etc/systemd/system/$E2B_TIMESYNC_UNIT.service.d/e2b-chrony-source.conf"

        echo "Enable SSH ($E2B_SSH_UNIT)"
        # provision.sh writes the sandbox sshd_config on every family, but nothing was
        # turning the unit on: Debian's postinst and the RHEL RPM scriptlet enable it
        # themselves, Arch does not, so Arch sandboxes shipped with SSH configured and
        # dead. Enabling is idempotent where the packaging already did it.
        systemctl enable "$E2B_SSH_UNIT.service"

        echo "Enable envd autostart"
        # Belt-and-suspenders with the baked 00-e2b.preset: on the RHEL family the
        # package transaction above runs systemd's RPM scriptlet 'systemctl preset-all'
        # (policy 'disable *'), which deletes the baked wants-symlink.
        systemctl enable envd.service

        echo "Disable chrony-wait"
        # chrony-wait blocks multi-user.target until the first clock sync (~8s);
        # chrony still syncs in the background, nothing needs to wait for it.
        # masking a unit that doesn't exist on this distro still succeeds (systemctl
        # mask just writes the /dev/null symlink), so a failure here is real.
        systemctl mask chrony-wait.service

        echo "Disable slow boot units not needed in the sandbox"
        # binfmt registrations (foreign-arch exec) take ~1s of CPU early in boot and
        # compete with envd start; e2scrub is for LVM-backed ext4 only.
        systemctl mask systemd-binfmt.service
        systemctl mask e2scrub_reap.service
    }
    e2b_profile_matched=1
    ;;
  alpine)
    E2B_PACKAGES="openrc shadow openssh sudo chrony socat curl ca-certificates fuse3 iptables git nfs-utils less nftables iputils jq bash util-linux-misc iproute2"
    e2b_pkg_query() { apk info -e "$1" >/dev/null 2>&1; }
    e2b_pkg_install() { apk add --no-cache "$@"; }
    E2B_INIT_BIN="/bin/busybox"
    E2B_TIMESYNC_UNIT="chronyd"
    E2B_SSH_UNIT="sshd"
    E2B_ADMIN_GROUP="wheel"
    E2B_CA_BUNDLE="/etc/ssl/certs/ca-certificates.crt"
    e2b_ca_refresh() { update-ca-certificates; }
    E2B_INIT_SYSTEM="openrc"
    e2b_init_setup() {
        echo "Installing boot inittab (busybox init -> OpenRC runlevels)"
        printf '%s\n' \
            '::sysinit:/sbin/openrc sysinit' \
            '::sysinit:/sbin/openrc boot' \
            '::wait:/sbin/openrc default' \
            '::shutdown:/sbin/openrc shutdown' \
            '::ctrlaltdel:/sbin/reboot' \
            > /etc/inittab

        echo "Registering base OpenRC services"
        # Container images carry no runlevel wiring at all (setup-alpine does this on
        # real installs): kernel filesystems in sysinit, system prep in boot. bootmisc
        # also wipes /tmp in the boot runlevel, so envd (default runlevel) can never
        # race the wipe — the ordering systemd needs After= for is inherent here.
        # Which scripts exist varies by image (mdev vs udev, procfs presence) — check
        # and say so instead of swallowing rc-update errors.
        for svc in devfs sysfs procfs dmesg mdev; do
            if [ -e "/etc/init.d/$svc" ]; then
                rc-update add "$svc" sysinit
            else
                echo "OpenRC service $svc not present on this image; skipping"
            fi
        done
        for svc in localmount sysctl hostname bootmisc; do
            if [ -e "/etc/init.d/$svc" ]; then
                rc-update add "$svc" boot
            else
                echo "OpenRC service $svc not present on this image; skipping"
            fi
        done

        # The FC guest's eth0 is configured by the kernel (ip=), but OpenRC services
        # declaring a "need net" dependency (chronyd) trigger the networking service,
        # which errors out on a missing /etc/network/interfaces and takes chronyd
        # down with it. A loopback-only interfaces file lets networking start (and
        # provide "net") without touching the kernel-managed eth0.
        printf 'auto lo\niface lo inet loopback\n' > /etc/network/interfaces
        if [ -e /etc/init.d/networking ]; then
            rc-update add networking boot
        else
            # openrc ships this script and the profile installs openrc, so this is
            # unreachable on the images we support. If it ever fires, nothing provides
            # "net": chronyd is still enabled below so the failure shows up in the boot
            # log rather than the sandbox silently running without time sync.
            echo "OpenRC networking service not present on this image; nothing provides 'net', so time sync will fail to start"
        fi

        echo "Enable time synchronization ($E2B_TIMESYNC_UNIT)"
        rc-update add "$E2B_TIMESYNC_UNIT" default

        echo "Install the boot-time time-source selector"
        # Writes the source line chrony.conf includes, in the boot runlevel so it is
        # done before chronyd starts in default. Baked outside /etc/init.d for the same
        # reason as the envd service script.
        cp /usr/local/share/e2b/chrony-source.openrc /etc/init.d/e2b-chrony-source
        chmod 0755 /etc/init.d/e2b-chrony-source
        rc-update add e2b-chrony-source boot

        echo "Disabling the chronyd seccomp filter"
        # Alpine's OpenRC init script hardcodes '-F 1', which loads chronyd's seccomp
        # filter, and Alpine's chrony build (-NTS -SECHASH -DEBUG) takes a SIGSYS — "Bad
        # system call" right after "Loaded seccomp filter (level 1)" — as soon as the PHC
        # refclock is driven, leaving the service in OpenRC's "crashed" state with the
        # clock unsynced. Unconditional, NOT gated on the PHC being present: which source
        # chronyd drives is decided at boot by e2b-chrony-source, so provisioning cannot
        # know. It costs nothing when the pool branch is taken, and the systemd families
        # pass no -F at all, so this is parity rather than a new hole. The init script
        # splices $command_args in after its own -F 1 and chronyd honours the last -F.
        # conf.d must be named for the init script OpenRC sources it for.
        mkdir -p /etc/conf.d
        echo 'command_args="-F 0"' >>"/etc/conf.d/$E2B_TIMESYNC_UNIT"

        echo "Enable envd autostart"
        # The service script is baked at a neutral path (envd.openrc.tpl) so the
        # Debian family's update-rc.d never sees it; install it for OpenRC here.
        cp /usr/local/share/e2b/envd.openrc /etc/init.d/envd
        chmod 0755 /etc/init.d/envd
        rc-update add envd default

        echo "Enable sshd"
        if [ -e /etc/init.d/sshd ]; then
            rc-update add sshd default
        else
            echo "sshd service not present on this image; skipping"
        fi
    }
    e2b_profile_matched=1
    ;;
  nixos)
    E2B_BB_DIR=/run/e2b-tools
    /usr/bin/busybox mkdir -p "$E2B_BB_DIR"
    /usr/bin/busybox --install -s "$E2B_BB_DIR"
    export PATH="$E2B_BB_DIR:$PATH"
    E2B_PACKAGES=""
    e2b_pkg_query() { true; }
    e2b_pkg_install() { echo "[provision] ERROR: NixOS images are premade — packages must be declared in the image's NixOS configuration" >&2; exit 1; }
    E2B_INIT_BIN="/sbin/e2b-nixos-init"
    E2B_TIMESYNC_UNIT="chronyd"
    E2B_SSH_UNIT=""
    E2B_ADMIN_GROUP="wheel"
    E2B_CA_BUNDLE="/etc/ssl/certs/ca-certificates.crt"
    e2b_ca_refresh() { echo "NixOS: the CA bundle is provided by the image configuration at first activation; nothing to refresh at provision time"; }
    E2B_INIT_SYSTEM="nixos"
    e2b_init_setup() {
        echo "NixOS is declaratively configured; removing the baked systemd drop-ins"
        rm -rf /etc/systemd/system
    }
    e2b_profile_matched=1
    ;;
  *)
    ;;
  esac
}

e2b_select_profile "$E2B_DISTRO_ID"
if [ -z "$e2b_profile_matched" ]; then
  case "$E2B_DISTRO_ID" in
  rhel|ol|amzn)
    echo "[provision] ERROR: base image distribution ID='$E2B_DISTRO_ID' is not supported." >&2
    echo "[provision] Sandboxes boot E2B's kernel, so the kABI, signed modules and SELinux these images are chosen for are unavailable." >&2
    exit 1
    ;;
  esac
  e2b_like_match=
  for e2b_like in $E2B_ID_LIKE; do
    e2b_select_profile "$e2b_like"
    if [ -n "$e2b_profile_matched" ]; then
      e2b_like_match=$e2b_like
      break
    fi
  done
  if [ -z "$e2b_like_match" ]; then
    echo "[provision] ERROR: unsupported base image distribution: ID='${E2B_DISTRO_ID:-unknown}'." >&2
    echo "[provision] E2B template builds support: debian, ubuntu, fedora, centos, rocky, almalinux, arch, archarm, alpine, nixos." >&2
    exit 1
  fi
  echo "[provision] WARNING: base image distribution ID='$E2B_DISTRO_ID' is not officially supported; provisioning it as '$e2b_like_match' from ID_LIKE. This is best effort and untested." >&2
fi
