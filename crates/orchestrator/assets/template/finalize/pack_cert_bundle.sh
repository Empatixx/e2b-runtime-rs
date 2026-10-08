set -e
# Recreate dpkg-recorded paths under /etc/ssl/certs lost to this fresh boot's
# tmpfs re-seed (build-layer dpkg writes never reach the rootfs), then settle
# dpkg before update-ca-certificates fires the packages' triggers against them.
if command -v dpkg-query >/dev/null 2>&1; then
	dpkg-query -W -f='${db:Status-Abbrev} ${binary:Package}\n' 2>/dev/null \
		| awk '/^.[iUFHWt]/{print $2}' \
		| xargs -r dpkg -L 2>/dev/null \
		| grep '^/etc/ssl/certs/' | sort -u > /run/e2b-cert-paths
	# dpkg's file db records no entry types; md5sums+conffiles enumerate its files.
	{ sed -n 's|^[0-9a-f]\{32\}  |/|p' /var/lib/dpkg/info/*.md5sums; cat /var/lib/dpkg/info/*.conffiles; } 2>/dev/null \
		| grep '^/etc/ssl/certs/' | sort -u > /run/e2b-cert-files
	comm -23 /run/e2b-cert-paths /run/e2b-cert-files \
		| while IFS= read -r path; do
			[ -e "$path" ] || mkdir -p "$path"
		done
	rm -f /run/e2b-cert-paths /run/e2b-cert-files
	# start_cmd may still hold the dpkg lock; retry instead of failing instantly.
	n=0
	until dpkg --configure -a; do
		n=$((n+1))
		[ "$n" -ge 5 ] && exit 1
		sleep 5
	done
fi
if command -v update-ca-certificates >/dev/null 2>&1; then
	update-ca-certificates
fi
# update-ca-certificates ignores hook failures; never snapshot half-configured state.
if command -v dpkg-query >/dev/null 2>&1; then
	broken=$(dpkg-query -W -f='${db:Status-Abbrev} ${Package}\n' 2>/dev/null | awk '/^iF/{print $2}' | tr '\n' ' ')
	if [ -n "$broken" ]; then
		printf 'cert bundle: packages left half-configured: %s\n' "$broken" >&2
		exit 1
	fi
fi
mkdir -p /usr/local/share/e2b
tar -C /etc/ssl/certs -chf /usr/local/share/e2b/ssl-certs.tar .
