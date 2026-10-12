#!/usr/bin/env bash
set -euo pipefail

IMPL="${1:?usage: upstream-integration.sh <go|rust> [shard]}"
SHARD="${2:-all}"
UPSTREAM="${UPSTREAM:-/mnt/work/e2b-runtime}"
RUST_REPO="$(cd "$(dirname "$0")/../.." && pwd)"
RESULTS="${RESULTS:-/mnt/work/results}/$IMPL-$SHARD"
LOGS="$RESULTS/logs"
BUCKET="https://storage.googleapis.com/e2b-artifact-binaries"

export PATH="/usr/local/go/bin:$HOME/go/bin:$HOME/.cargo/bin:$PATH"
export RACE_FLAG="" DEBUG_GCFLAGS=""
export TESTS_E2B_API_KEY="e2b_5ec17bd3933af21f80dc10bba686691c4fcd7057"
export TESTS_SANDBOX_TEAM_ID="834777bd-9956-45ca-b088-9bac9290e2ac"
export TESTS_SANDBOX_USER_ID="2a5a9fc5-db8d-4af7-ac9e-d0f9272463bc"
export CLICKHOUSE_USERNAME="e2b" CLICKHOUSE_PASSWORD="clickity-clicky-click" CLICKHOUSE_PORT="9000" CLICKHOUSE_DATABASE="default"
export POSTGRES_CONNECTION_STRING="postgresql://postgres:local@localhost:5432/mydatabase?sslmode=disable"
export TEMPLATE_ID="2j6ly824owf4awgai1xo" KERNEL_VERSION="vmlinux-6.1.158-c1a568c" FIRECRACKER_VERSION="v1.14-0.2.0" MEMORY_MB="256"

case "$IMPL" in
  go) ORCHESTRATOR_MAKE_PATH="packages/orchestrator" ;;
  rust) ORCHESTRATOR_MAKE_PATH="$RUST_REPO/scripts/parity/orchestrator" ;;
  *) echo "unknown implementation $IMPL" >&2; exit 2 ;;
esac

log() { echo "[$(date +%T)] $*"; }

stop_everything() {
  log "stopping services from earlier runs"
  sudo pkill -f '/bin/orchestrator' || true
  pkill -f 'bin/api' || true
  pkill -f 'bin/client-proxy' || true
  pkill -f 'make -C packages/clickhouse run' || true
  pkill -f 'make -C .* run' || true
  docker rm -f postgres redis clickhouse otel-collector >/dev/null 2>&1 || true
  while docker ps -a --format '{{.Names}}' | grep -qE '^(postgres|redis|clickhouse|otel-collector)$'; do sleep 1; done
  sudo pkill -f firecracker || true
  sudo rm -rf /tmp/templates /tmp/build-cache /orchestrator /var/lib/e2b /fc-vm/* "$UPSTREAM/.e2b-chunk-cache" /mnt/snapshot-cache/* || true
}

start_databases() {
  log "starting databases"
  docker run -d --name postgres -e POSTGRES_USER=postgres -e POSTGRES_PASSWORD=local -e POSTGRES_DB=mydatabase -p 5432:5432 \
    --health-cmd="pg_isready -U postgres" --health-interval=5s --health-timeout=2s --health-retries=5 postgres:18 >/dev/null
  docker run -d --name redis -p 6379:6379 \
    --health-cmd="redis-cli ping" --health-interval=5s --health-timeout=2s --health-retries=5 redis:8 >/dev/null
  make -C packages/clickhouse run >"$LOGS/clickhouse-run.log" 2>&1 &
}

build_packages() {
  log "building upstream packages"
  local package
  for package in tests/integration packages/db packages/orchestrator packages/api packages/envd packages/client-proxy; do
    make -C "$package" build-debug >"$LOGS/build-${package//\//_}.log" 2>&1
  done
  if [ "$IMPL" = rust ]; then
    log "building the Rust orchestrator and envd"
    make -C "$ORCHESTRATOR_MAKE_PATH" build-debug >"$LOGS/build-rust-orchestrator.log" 2>&1
    (cd "$RUST_REPO" && cargo build --release --locked -p envd --target x86_64-unknown-linux-musl) >"$LOGS/build-rust-envd.log" 2>&1
  fi
}

envd_binary() {
  case "$IMPL" in
    go) echo "$UPSTREAM/packages/envd/bin/debug/envd" ;;
    rust) echo "$RUST_REPO/target/x86_64-unknown-linux-musl/release/envd" ;;
  esac
}

fetch_artifact() {
  local key="$1" dest="$2"
  [ -s "$dest" ] && return 0
  sudo mkdir -p "$(dirname "$dest")"
  sudo curl -fsSL --retry 3 -o "$dest" "$BUCKET/$key"
  sudo chmod 755 "$dest"
}

host_init() {
  log "initialising the host with $IMPL envd"
  sudo mkdir -p /orchestrator/sandbox /orchestrator/template /orchestrator/build /fc-vm /fc-envd /mnt/snapshot-cache
  if [ -z "$(sudo swapon --show --noheadings 2>/dev/null)" ]; then
    sudo fallocate -l 1G /swapfile && sudo chmod 600 /swapfile && sudo mkswap /swapfile >/dev/null && sudo swapon /swapfile
  fi
  sudo sysctl -q vm.swappiness=10 vm.vfs_cache_pressure=50 vm.max_map_count=1048576
  echo 'ACTION=="add|change", KERNEL=="nbd*", OPTIONS:="nowatch"' | sudo tee /etc/udev/rules.d/97-nbd-device.rules >/dev/null
  sudo udevadm control --reload-rules && sudo udevadm trigger
  [ -e /dev/nbd0 ] || sudo modprobe nbd nbds_max=128
  sudo install -m 755 "$(envd_binary)" /fc-envd/envd
  fetch_artifact "kernels/$KERNEL_VERSION/amd64/vmlinux.bin" "/fc-kernels/$KERNEL_VERSION/amd64/vmlinux.bin"
  fetch_artifact "firecrackers/$FIRECRACKER_VERSION/amd64/firecracker" "/fc-versions/$FIRECRACKER_VERSION/amd64/firecracker"
  fetch_artifact "busybox/1.36.1/amd64/busybox" "/fc-busybox/1.36.1/amd64/busybox"
  mountpoint -q /mnt/hugepages || { sudo mkdir -p /mnt/hugepages && sudo mount -t hugetlbfs none /mnt/hugepages; }
  local total reserved
  total=$(( $(awk '/MemTotal/ {print $2}' /proc/meminfo) / 1024 ))
  reserved=$(( total * 16 / 100 )); [ "$reserved" -lt 4096 ] && reserved=4096
  echo 0 | sudo tee /proc/sys/vm/nr_hugepages >/dev/null
  echo $(( (total - reserved) / 2 )) | sudo tee /proc/sys/vm/nr_overcommit_hugepages >/dev/null
}

write_env() {
  echo test > .last_used_env
  {
    echo "POSTGRES_CONNECTION_STRING=$POSTGRES_CONNECTION_STRING"
    echo "TESTS_E2B_API_KEY=$TESTS_E2B_API_KEY"
    echo "TESTS_SANDBOX_TEAM_ID=$TESTS_SANDBOX_TEAM_ID"
    echo "TESTS_SANDBOX_USER_ID=$TESTS_SANDBOX_USER_ID"
    echo "REDIS_URL=localhost:6379"
    echo "CLICKHOUSE_USERNAME=$CLICKHOUSE_USERNAME"
    echo "CLICKHOUSE_PORT=$CLICKHOUSE_PORT"
    echo "CLICKHOUSE_PASSWORD=$CLICKHOUSE_PASSWORD"
    echo "CLICKHOUSE_DATABASE=$CLICKHOUSE_DATABASE"
    echo "CLICKHOUSE_CONNECTION_STRING=clickhouse://$CLICKHOUSE_USERNAME:$CLICKHOUSE_PASSWORD@localhost:$CLICKHOUSE_PORT/$CLICKHOUSE_DATABASE"
    echo "TESTS_SANDBOX_TEMPLATE_ID=$TEMPLATE_ID"
    echo "TESTS_SANDBOX_BUILD_ID=$BUILD_ID"
    echo "TESTS_SANDBOX_RAM_MB=$MEMORY_MB"
    echo "COMPRESS_ENABLED=false"
  } > .env.test
}

wait_healthy() {
  local name="$1" i
  for ((i = 0; i < 120; i += 2)); do
    [ "$(docker inspect -f '{{.State.Health.Status}}' "$name" 2>/dev/null)" = healthy ] && return 0
    sleep 2
  done
  echo "$name did not become healthy" >&2
  return 1
}

migrate() {
  log "migrating and seeding"
  wait_healthy postgres
  wait_healthy clickhouse
  wait_healthy redis
  docker exec postgres psql -U postgres -d mydatabase -c "CREATE SCHEMA extensions; CREATE EXTENSION IF NOT EXISTS pgcrypto SCHEMA extensions;" >/dev/null
  make migrate >"$LOGS/migrate.log" 2>&1
  make -C tests/integration seed >"$LOGS/seed.log" 2>&1
  make -C packages/clickhouse migrate-host >"$LOGS/clickhouse-migrate.log" 2>&1
}

build_template() {
  log "building the base template with Go create-build and $IMPL envd"
  sudo -E env PATH="$PATH" make -C packages/orchestrator build-template \
    ARTIFACTS_REGISTRY_PROVIDER=Local STORAGE_PROVIDER=Local \
    TEMPLATE_ID="$TEMPLATE_ID" BUILD_ID="$BUILD_ID" KERNEL_VERSION="$KERNEL_VERSION" \
    FIRECRACKER_VERSION="$FIRECRACKER_VERSION" MEMORY_MB="$MEMORY_MB" >"$LOGS/build-template.log" 2>&1
}

start_services() {
  log "starting services with the $IMPL orchestrator"
  export ENVD_TIMEOUT="60s" ORCHESTRATOR_SERVICES="orchestrator,template-manager" SANDBOX_ACCESS_TOKEN_HASH_SEED="abcdefghijklmnopqrstuvwxyz"
  export ARTIFACTS_REGISTRY_PROVIDER="Local" STORAGE_PROVIDER="Local" ENVIRONMENT="local" OTEL_COLLECTOR_GRPC_ENDPOINT="localhost:4317"
  export MAX_PARALLEL_MEMFILE_SNAPSHOTTING="2" SHARED_CHUNK_CACHE_PATH="./.e2b-chunk-cache" REDIS_URL="localhost:6379"
  export API_INTERNAL_GRPC_ADDRESS="localhost:5009" DEFAULT_PERSISTENT_VOLUME_TYPE="test-volume-type"
  export COMPRESS_ENABLED="false" TESTS_MEMFILE_DIFF_DEDUP_MODE="default" TESTS_DISABLE_MEMFD="true" E2B_DEBUG="true"
  export DEFAULT_KERNEL_VERSION="$KERNEL_VERSION" DEFAULT_FIRECRACKER_VERSION="$FIRECRACKER_VERSION"
  mkdir -p "$SHARED_CHUNK_CACHE_PATH" /mnt/work/test-volumes
  export PERSISTENT_VOLUME_MOUNTS="test-volume-type:/mnt/work/test-volumes"
  STARTUP_TIMEOUT=120 ./scripts/start-service.sh OtelCollector packages/otel-collector run "$LOGS/otel-collector.log" http://localhost:13133/healthz
  STARTUP_TIMEOUT=120 ./scripts/start-service.sh Orchestrator "$ORCHESTRATOR_MAKE_PATH" run-debug "$LOGS/orchestrator.log" http://localhost:5008/health
  STARTUP_TIMEOUT=60 ./scripts/start-service.sh API packages/api run "$LOGS/api.log" http://localhost:3000/health
  ./scripts/start-service.sh ClientProxy packages/client-proxy run "$LOGS/client-proxy.log" http://localhost:3003
  if [ "$IMPL" = rust ]; then
    sudo pgrep -af "$ORCHESTRATOR_MAKE_PATH/bin/" >/dev/null || { echo "the Rust orchestrator is not running" >&2; exit 1; }
  fi
}

run_tests() {
  log "running the integration suite, shard $SHARD"
  TESTS_API_SERVER_URL="http://localhost:3000" TESTS_API_INTERNAL_GRPC_ADDRESS="localhost:5009" \
  TESTS_ORCHESTRATOR_HOST="localhost:5008" TESTS_ENVD_PROXY="http://localhost:3002" TESTS_CLIENT_PROXY="http://localhost:3002" \
  TEST_PARALLELISM="${TEST_PARALLELISM:-4}" TEST_SHARD="$SHARD" \
    make test-integration >"$LOGS/tests.log" 2>&1 || true
  cp tests/integration/test-results.xml "$RESULTS/test-results.xml" || true
}

main() {
  rm -rf "$RESULTS" && mkdir -p "$LOGS"
  cd "$UPSTREAM"
  stop_everything
  start_databases
  build_packages
  host_init
  BUILD_ID="$(uuidgen)"
  export BUILD_ID
  write_env
  migrate
  build_template
  start_services
  run_tests
  [ -n "${KEEP_RUNNING:-}" ] || stop_everything
  log "done: $RESULTS/test-results.xml"
}

main
