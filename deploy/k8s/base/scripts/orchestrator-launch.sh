#!/usr/bin/env bash
set -euo pipefail

log() { echo "orchestrator-launch: $*"; }

resolve_service_names() {
  local name value host ip
  for name in $(compgen -e); do
    value="${!name}"
    for host in $(grep -oE '[a-z0-9-]+\.[a-z0-9-]+\.svc\.cluster\.local' <<<"$value" | sort -u); do
      ip="$(getent hosts "$host" | awk '{print $1; exit}')"
      [ -n "$ip" ] || { log "cannot resolve $host"; exit 1; }
      value="${value//$host/$ip}"
    done
    export "$name=$value"
  done
}

resolve_service_names
nsenter -t 1 -m -u -i -n -C -w/ -- \
  /usr/bin/env PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
  /var/lib/e2b/bin/orchestrator &
child=$!
trap 'log "SIGTERM: draining the orchestrator (pid $child)"; kill -TERM "$child" 2>/dev/null || true' TERM INT
log "started the orchestrator (pid $child)"
status=0
while kill -0 "$child" 2>/dev/null; do
  wait "$child" || status=$?
done
log "orchestrator exited with status $status"
exit "$status"
