if [ -f /usr/local/share/e2b/distro.env ]; then
    . /usr/local/share/e2b/distro.env
fi
if [ -n "${E2B_ADMIN_GROUP:-}" ]; then
    if ! getent group "$E2B_ADMIN_GROUP" >/dev/null; then
        echo "the '$E2B_ADMIN_GROUP' group from the distro profile does not exist on this image" >&2
        exit 1
    fi
    usermod -aG "$E2B_ADMIN_GROUP" {{ .User }}
elif getent group sudo >/dev/null; then
    usermod -aG sudo {{ .User }}
elif getent group wheel >/dev/null; then
    usermod -aG wheel {{ .User }}
else
    echo "neither the sudo nor the wheel group exists on this image" >&2
    exit 1
fi