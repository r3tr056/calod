#!/usr/bin/env bash
set -euo pipefail

PROFILE="${1:-A}"
TARGET="${2:-calod}"
REPEAT_RUNS="${STRESS_REPEAT_RUNS:-1}"
SERVER_MEM_LIMIT="${CALOD_SERVER_MEM_LIMIT:-6g}"
RUNNER_MEM_LIMIT="${CALOD_RUNNER_MEM_LIMIT:-1g}"
SERVER_CPUS="${CALOD_SERVER_CPUS:-4}"
RUNNER_CPUS="${CALOD_RUNNER_CPUS:-2}"

to_mib() {
  local value="$1"
  if [[ "$value" =~ ^([0-9]+)([kKmMgG])$ ]]; then
    local num="${BASH_REMATCH[1]}"
    local unit="${BASH_REMATCH[2],,}"
    case "$unit" in
      k) echo $(((num + 1023) / 1024)) ;;
      m) echo "$num" ;;
      g) echo $((num * 1024)) ;;
    esac
    return 0
  fi
  echo "unsupported memory format: $value" >&2
  return 1
}

TOTAL_MIB=$(( $(to_mib "$SERVER_MEM_LIMIT") + $(to_mib "$RUNNER_MEM_LIMIT") ))
if (( TOTAL_MIB > 8192 )); then
  echo "combined Docker memory budget exceeds 8GB" >&2
  exit 1
fi

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ARTIFACTS_DIR="$ROOT_DIR/stress-artifacts"
mkdir -p "$ARTIFACTS_DIR"

export STRESS_PROFILE="$PROFILE"
export CALOD_SERVER_MEM_LIMIT="$SERVER_MEM_LIMIT"
export CALOD_RUNNER_MEM_LIMIT="$RUNNER_MEM_LIMIT"
export CALOD_SERVER_CPUS="$SERVER_CPUS"
export CALOD_RUNNER_CPUS="$RUNNER_CPUS"
export STRESS_REPEAT_RUNS="$REPEAT_RUNS"

cleanup() {
  docker compose -f "$ROOT_DIR/docker-compose.stress.yml" --profile redis down --remove-orphans
}
trap cleanup EXIT

docker compose -f "$ROOT_DIR/docker-compose.stress.yml" --profile redis build

run_target() {
  local target="$1"
  local service runner compose_args label
  case "${target,,}" in
    calod)
      service="calod"
      runner="stress-runner"
      compose_args=(-f "$ROOT_DIR/docker-compose.stress.yml")
      label="calod"
      ;;
    redis)
      service="redis-baseline"
      runner="stress-runner-redis"
      compose_args=(-f "$ROOT_DIR/docker-compose.stress.yml" --profile redis)
      label="redis"
      ;;
    *)
      echo "unsupported target: $target" >&2
      exit 1
      ;;
  esac

  docker compose "${compose_args[@]}" up -d "$service"
  docker compose "${compose_args[@]}" run --rm "$runner"

  CONTAINER_ID="$(docker compose "${compose_args[@]}" ps -q "$service")"
  if [[ -n "$CONTAINER_ID" ]]; then
    docker stats --no-stream --format '{{json .}}' "$CONTAINER_ID" > "$ARTIFACTS_DIR/$PROFILE-$label-docker-stats.json"
    docker inspect --format '{{json .HostConfig.Memory}}' "$CONTAINER_ID" > "$ARTIFACTS_DIR/$PROFILE-$label-memory-limit-bytes.json"
  fi
}

if [[ "${TARGET,,}" == "both" ]]; then
  run_target calod
  run_target redis
else
  run_target "$TARGET"
fi

echo "Stress test artifacts written to $ARTIFACTS_DIR"
