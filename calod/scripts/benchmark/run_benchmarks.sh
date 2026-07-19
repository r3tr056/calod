#!/usr/bin/env bash
#
# run_benchmarks.sh — M0 measurement harness: calod vs Redis 7.4 vs Valkey 8.
#
# Runs memtier_benchmark against all three servers, each pinned to the SAME
# N cores (cpuset via docker-compose.benchmark.yml), over a workload matrix of
# pipeline x ratio x value size x key distribution, at two client points.
# Writes one memtier JSON artifact per (server, cell) plus a summary.md.
#
# Linux-only (taskset/cpuset semantics). Intended for a dedicated benchmark
# host; see README.md in this directory for methodology.
#
# Usage:
#   ./run_benchmarks.sh                 # full default matrix
#   N_CORES=8 ./run_benchmarks.sh       # 8 cores per server
#   SERVERS="calod redis" ./run_benchmarks.sh
#   PIPELINES="16" RATIOS="1:10" ./run_benchmarks.sh   # quick smoke matrix
#
set -euo pipefail

# ---------------------------------------------------------------------------
# Tunables (env-overridable)
# ---------------------------------------------------------------------------

# Cores pinned to EACH server under test. The memtier client is pinned to a
# disjoint range of equal size so client/server never share cores.
N_CORES="${N_CORES:-4}"
SERVER_CPUSET="${SERVER_CPUSET:-0-$((N_CORES - 1))}"
CLIENT_CPUSET="${CLIENT_CPUSET:-$((N_CORES))-$((2 * N_CORES - 1))}"

TEST_TIME="${TEST_TIME:-60}"            # seconds per measurement cell (>= 60)
KEY_MIN="${KEY_MIN:-1}"
KEY_MAX="${KEY_MAX:-3000000}"           # >= 3M keyspace

# Workload matrix dimensions (space-separated lists).
PIPELINES="${PIPELINES:-1 16}"
RATIOS="${RATIOS:-1:10 0:1 1:0}"        # SET:GET; 0:1 = pure GET, 1:0 = pure SET
DATA_SIZES="${DATA_SIZES:-256 512}"
KEY_DISTS="${KEY_DISTS:-uniform gaussian}"

# Client points: 50 clients x 4 threads (200 conns) and a 650-connection point
# (50 clients/thread x 13 threads).
BASE_CLIENTS="${BASE_CLIENTS:-50}"
BASE_THREADS="${BASE_THREADS:-4}"
HIGH_CLIENTS="${HIGH_CLIENTS:-50}"
HIGH_THREADS="${HIGH_THREADS:-13}"      # 50*13 = 650 simultaneous connections

# High-connection point runs a single representative cell per distribution.
HIGH_PIPELINE="${HIGH_PIPELINE:-16}"
HIGH_RATIO="${HIGH_RATIO:-1:10}"
HIGH_DATA_SIZE="${HIGH_DATA_SIZE:-256}"

SERVERS="${SERVERS:-calod redis valkey}"
REPETITIONS="${REPETITIONS:-1}"

# Gaussian access pattern: median at keyspace centre, 6-sigma spanning it.
GAUSS_MEDIAN="${GAUSS_MEDIAN:-$(( (KEY_MIN + KEY_MAX) / 2 ))}"
GAUSS_STDDEV="${GAUSS_STDDEV:-$(( (KEY_MAX - KEY_MIN) / 6 ))}"

# Preload: sequential writes so the keyspace is populated before measurement.
# memtier's --requests counts per client *thread*, so total SETs issued is
# requests x threads x clients. distinct seeds spread each thread's sequence
# start across the range; 2x factor covers gaps left by random start offsets.
# Any residual misses are identical for all three servers, so comparisons
# stay fair.
PRELOAD_REQUESTS="${PRELOAD_REQUESTS:-$(( (2 * KEY_MAX) / (BASE_CLIENTS * BASE_THREADS) ))}"

# ---------------------------------------------------------------------------

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
COMPOSE_FILE="$ROOT_DIR/docker-compose.benchmark.yml"
TIMESTAMP="$(date -u +%Y%m%dT%H%M%SZ)"
RESULTS_DIR="$ROOT_DIR/benchmark-results/$TIMESTAMP"
mkdir -p "$RESULTS_DIR"

export N_CORES SERVER_CPUSET CLIENT_CPUSET
export IO_THREADS=$((N_CORES - 1))      # redis/valkey io-threads; main thread counts separately
export BENCH_RESULTS_DIR="$RESULTS_DIR" # memtier container mounts this at /results

log() { echo "[run_benchmarks $(date -u +%H:%M:%S)] $*"; }

cleanup() {
  docker compose -f "$COMPOSE_FILE" down --remove-orphans >/dev/null 2>&1 || true
}
trap cleanup EXIT

check_host_hygiene() {
  # These are host-level settings; the script only warns, never fails.
  if [[ -r /sys/kernel/mm/transparent_hugepage/enabled ]]; then
    if grep -q '\[always\]' /sys/kernel/mm/transparent_hugepage/enabled; then
      log "WARNING: THP is 'always'; recommended: echo madvise|never > /sys/kernel/mm/transparent_hugepage/enabled"
    fi
  fi
  if [[ -r /proc/sys/vm/overcommit_memory ]] && [[ "$(cat /proc/sys/vm/overcommit_memory)" != "1" ]]; then
    log "WARNING: vm.overcommit_memory != 1; recommended: sysctl vm.overcommit_memory=1"
  fi
  if [[ -r /proc/sys/net/core/somaxconn ]]; then
    local sc; sc="$(cat /proc/sys/net/core/somaxconn)"
    if (( sc < 1024 )); then
      log "WARNING: net.core.somaxconn=$sc is low; recommended: sysctl net.core.somaxconn=65535"
    fi
  fi
}

key_pattern_args() {
  local dist="$1"
  case "$dist" in
    uniform)
      echo "--key-pattern R:R"
      ;;
    gaussian)
      echo "--key-pattern G:G --key-stddev $GAUSS_STDDEV --key-median $GAUSS_MEDIAN"
      ;;
    *)
      log "unknown key distribution: $dist" >&2
      return 1
      ;;
  esac
}

# run_memtier <server> <json-name> <extra args...>
run_memtier() {
  local server="$1" name="$2"
  shift 2
  docker compose -f "$COMPOSE_FILE" run --rm \
    memtier \
      --server "$server" --port 6379 \
      --key-minimum "$KEY_MIN" --key-maximum "$KEY_MAX" \
      --distinct-client-seed --random-data \
      --json-out-file "/results/$name.json" \
      --out-file "/results/$name.txt" \
      "$@"
}

# run_cell <server> <pipeline> <ratio> <data-size> <dist> <clients> <threads> <tag>
run_cell() {
  local server="$1" pipeline="$2" ratio="$3" size="$4" dist="$5" \
        clients="$6" threads="$7" tag="$8"
  local ratio_label="set${ratio%%:*}get${ratio##*:}"
  local conns=$((clients * threads))
  local name="${tag}__pl${pipeline}__${ratio_label}__${size}b__${dist}__c${conns}__${server}"
  log "cell: server=$server pipeline=$pipeline ratio=$ratio size=$size dist=$dist conns=$conns"
  # shellcheck disable=SC2046
  run_memtier "$server" "$name" \
    --pipeline "$pipeline" \
    --ratio "$ratio" \
    --data-size "$size" \
    --clients "$clients" --threads "$threads" \
    --test-time "$TEST_TIME" \
    $(key_pattern_args "$dist")
}

preload() {
  local server="$1"
  log "preloading $server (target keyspace $KEY_MIN..$KEY_MAX)"
  run_memtier "$server" "preload__${server}" \
    --pipeline 16 \
    --ratio 1:0 \
    --data-size 256 \
    --clients "$BASE_CLIENTS" --threads "$BASE_THREADS" \
    --requests "$PRELOAD_REQUESTS" \
    --key-pattern S:S
}

bench_server() {
  local server="$1"
  log "=== server: $server ==="
  docker compose -f "$COMPOSE_FILE" up -d --wait "$server"
  preload "$server"
  local rep pl ratio size dist
  for rep in $(seq 1 "$REPETITIONS"); do
    for pl in $PIPELINES; do
      for ratio in $RATIOS; do
        for size in $DATA_SIZES; do
          for dist in $KEY_DISTS; do
            run_cell "$server" "$pl" "$ratio" "$size" "$dist" \
              "$BASE_CLIENTS" "$BASE_THREADS" "run${rep}"
          done
        done
      done
    done
    # 650-connection point
    for dist in $KEY_DISTS; do
      run_cell "$server" "$HIGH_PIPELINE" "$HIGH_RATIO" "$HIGH_DATA_SIZE" "$dist" \
        "$HIGH_CLIENTS" "$HIGH_THREADS" "run${rep}-highconn"
    done
  done
  docker compose -f "$COMPOSE_FILE" stop "$server"
}

write_run_meta() {
  cat > "$RESULTS_DIR/run_meta.json" <<EOF
{
  "timestamp_utc": "$TIMESTAMP",
  "n_cores_per_server": $N_CORES,
  "server_cpuset": "$SERVER_CPUSET",
  "client_cpuset": "$CLIENT_CPUSET",
  "io_threads_baselines": $IO_THREADS,
  "test_time_secs": $TEST_TIME,
  "key_range": [$KEY_MIN, $KEY_MAX],
  "gaussian": {"stddev": $GAUSS_STDDEV, "median": $GAUSS_MEDIAN},
  "matrix": {
    "pipelines": "$PIPELINES",
    "ratios_set_get": "$RATIOS",
    "data_sizes": "$DATA_SIZES",
    "key_distributions": "$KEY_DISTS",
    "base_clients_x_threads": "${BASE_CLIENTS}x${BASE_THREADS}",
    "high_conn_clients_x_threads": "${HIGH_CLIENTS}x${HIGH_THREADS}"
  },
  "repetitions": $REPETITIONS,
  "images": {
    "calod": "calod-benchmark:local (built from repo Dockerfile)",
    "redis": "redis:7.4.2-bookworm",
    "valkey": "valkey/valkey:8.0.2-bookworm",
    "memtier": "redislabs/memtier_benchmark:latest"
  }
}
EOF
}

main() {
  log "results dir: $RESULTS_DIR"
  log "cores per server: $N_CORES (server cpuset $SERVER_CPUSET, client cpuset $CLIENT_CPUSET)"
  check_host_hygiene
  write_run_meta
  docker compose -f "$COMPOSE_FILE" build calod
  local server
  for server in $SERVERS; do
    bench_server "$server"
  done
  if command -v python3 >/dev/null 2>&1; then
    python3 "$ROOT_DIR/scripts/benchmark/summarize.py" "$RESULTS_DIR"
    log "summary written to $RESULTS_DIR/summary.md"
  else
    log "WARNING: python3 not found; skipping summary.md (raw JSON artifacts are in $RESULTS_DIR)"
  fi
  log "done."
}

main "$@"
