#!/bin/sh
set -eu

BUSYBOX="/usr/bin/busybox"
RESULT_PATH="/provision.result"

echo "Starting provisioning script"


# GCP Specific logic


echo "Making configuration immutable"
$BUSYBOX chattr +i /etc/resolv.conf

# Identify the base image by its DECLARED /etc/os-release ID, never by probing
# for a package manager. The selector (generated from the distro profile
# registry) defines the E2B_* vars and pkg/CA shell functions, or exits 1 on an
# unsupported distribution. Images with no os-release (distroless, scratch) are
# rejected by name rather than guessed.
echo "Detecting base image distribution"
if [ -r /etc/os-release ]; then
    . /etc/os-release
    E2B_DISTRO_ID="${ID:-unknown}"
    # Derivative-to-parent pointer, e.g. Kali declares ID=kali ID_LIKE=debian.
    # Only consulted when the declared ID matches no profile.
    E2B_ID_LIKE="${ID_LIKE:-}"
else
    E2B_DISTRO_ID="unknown (image has no /etc/os-release)"
    E2B_ID_LIKE=""
fi

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


echo "Provisioning for distro '$E2B_DISTRO_ID' (init=$E2B_INIT_BIN, timesync=$E2B_TIMESYNC_UNIT, admin-group=$E2B_ADMIN_GROUP)"

# Persist the resolved identity for later build phases (finalize sources it),
# so the profile's values are defined once, here.
mkdir -p /usr/local/share/e2b
{
    echo "E2B_DISTRO_ID='$E2B_DISTRO_ID'"
    echo "E2B_INIT_SYSTEM='$E2B_INIT_SYSTEM'"
    echo "E2B_ADMIN_GROUP='$E2B_ADMIN_GROUP'"
} > /usr/local/share/e2b/distro.env

# Helper function to check if a package is installed (distro-specific query)
is_package_installed() {
    e2b_pkg_query "$1"
}

# Install required packages if not already installed
PACKAGES="$E2B_PACKAGES"
echo "Checking presence of the following packages: $PACKAGES"

MISSING=""
for pkg in $PACKAGES; do
    if ! is_package_installed "$pkg"; then
        echo "Package $pkg is missing, will install it."
        MISSING="$MISSING $pkg"
    fi
done

if [ -n "$MISSING" ]; then
    echo "Missing packages detected, installing:$MISSING"
    # shellcheck disable=SC2086
    e2b_pkg_install $MISSING
else
    echo "All required packages are already installed."
fi

# Ensure the CA trust bundle exists where envd expects it; e2b_ca_refresh
# regenerates it per family. A refresh failure fails provisioning (set -e) — a
# silently broken trust store is worse than a legible build error.
if [ ! -s "$E2B_CA_BUNDLE" ]; then
    echo "CA trust bundle missing at $E2B_CA_BUNDLE — running the profile's refresh"
    e2b_ca_refresh
fi

# Set /dev/fuse permissions to 666 for non-root access
# Use systemd-tmpfiles to set permissions at boot
mkdir -p /etc/tmpfiles.d
echo 'z /dev/fuse 0666 root root -' > /etc/tmpfiles.d/fuse.conf

echo "Setting up shell"
# Not every base image ships /etc/profile.d or /root; create them so the
# drop-ins below always have a home.
mkdir -p /etc/profile.d /root
echo "export SHELL='/bin/bash'" >/etc/profile.d/shell.sh
echo "export PS1='\w \$ '" >/etc/profile.d/prompt.sh
echo "export PS1='\w \$ '" >>"/etc/profile"
echo "export PS1='\w \$ '" >>"/root/.bashrc"

echo "Use .bashrc and .profile"
echo "if [ -f ~/.bashrc ]; then source ~/.bashrc; fi; if [ -f ~/.profile ]; then source ~/.profile; fi" >>/etc/profile

echo "Remove root password"
# A minimal image may not ship /etc/passwd; only clear the root password when it
# exists rather than failing provisioning.
if [ -f /etc/passwd ]; then
    passwd -d root
else
    echo "No /etc/passwd on this image; skipping root password removal"
fi

echo "Setting up chrony"
mkdir -p /etc/chrony
{
    # The source line (PHC refclock vs NTP pool) is chosen at BOOT by
    # e2b-chrony-source, not here: a template provisioned on a node with
    # /dev/ptp0 can cold-boot on a node without it, and a refclock line for a
    # missing PHC is FATAL to chronyd.
    echo "include /run/chrony-e2b/source.conf"
    # Step (jump) the clock instead of slewing when the offset exceeds 1s, but
    # only for the first 3 updates after chronyd starts. chronyd restarts on
    # every cold boot/reboot, so this corrects a large boot-time offset fast
    # (TLS needs a correct clock) without risking a backward jump under a
    # running workload. Needed because chrony-wait is masked, so boot no
    # longer blocks on first sync.
    echo "makestep 1.0 3"
} >/etc/chrony/chrony.conf

# Add a proxy config, as some environments expects it there (e.g. timemaster in Node Dockerimage)
echo "include /etc/chrony/chrony.conf" >/etc/chrony.conf

echo "Setting up SSH"
mkdir -p /etc/ssh
# A drop-in leaves the package's sshd_config untouched: appending to it marks the conffile
# modified, and a later openssh upgrade in a customer build step then stops on dpkg's
# "modified configuration file" prompt until the build times out. Images whose sshd_config
# has no Include line (stock upstream config) still get the append.
if grep -qE '^Include[[:space:]]+/etc/ssh/sshd_config\.d/' /etc/ssh/sshd_config 2>/dev/null; then
  E2B_SSHD_CONF=/etc/ssh/sshd_config.d/00-e2b.conf
  mkdir -p /etc/ssh/sshd_config.d
else
  E2B_SSHD_CONF=/etc/ssh/sshd_config
fi
cat <<EOF >>"$E2B_SSHD_CONF"
PermitRootLogin yes
PermitEmptyPasswords yes
PasswordAuthentication yes
EOF

echo "Increasing inotify watch limit"
echo 'fs.inotify.max_user_watches=65536' | tee -a /etc/sysctl.conf

# Disable kcompactd background page migration. With 2 MiB host-side hugepage
# backing of guest RAM, every migration dirties a destination hugepage from
# the host UFFD's perspective and lands in the next memfile diff, with no
# corresponding workload benefit between snapshots. We trigger compaction
# explicitly pre-pause instead.
echo "Disabling proactive memory compaction"
echo 'vm.compaction_proactiveness=0' | tee -a /etc/sysctl.conf

# Init-system-specific boot arrangement (autostarts, boot-noise silencing, the
# OpenRC inittab swap), rendered per family by the distro selector (distro/init.go).
e2b_init_setup

# Clean machine-id from Docker
rm -rf /etc/machine-id

echo "Linking $E2B_INIT_BIN to init"
# Not every image ships /usr/sbin; create it before linking init.
mkdir -p /usr/sbin
ln -sf "$E2B_INIT_BIN" /usr/sbin/init
# /sbin is a real directory on non-usr-merged distros (Alpine) where the line
# above doesn't reach the /sbin/init the kernel is pointed at; link it too.
[ -L /sbin ] || ln -sf "$E2B_INIT_BIN" /sbin/init

echo "Unlocking immutable configuration"
$BUSYBOX chattr -i /etc/resolv.conf

echo "Finished provisioning script"

# Delete itself
rm -rf /etc/init.d/rcS
rm -rf /usr/local/bin/provision.sh

# Report successful provisioning
printf "0" > "$RESULT_PATH"
