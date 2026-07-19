#!/usr/bin/env bash
#
# run_suites.sh — in-container entrypoint for the Redis tcl-suite runner.
#
# Starts calod (release binary) on 127.0.0.1:6379, then runs the
# client-portable subset of the Redis test suite against it via
# ./runtest --host 127.0.0.1 --port 6379 --single <suite>, collecting
# per-suite logs, failing-assertion extracts, and a summary.md into /results.
#
# Env overrides:
#   SUITES        space-separated suite list (default: the client-portable subset)
#   TCL_TAGS      runtest --tags value (default: "-slow", skip slow tests)
#   STRICT=1      exit non-zero if any suite fails (default: always exit 0,
#                 so baseline runs with expected failures still complete)
#
set -uo pipefail

HOST=127.0.0.1
PORT=6379
RESULTS_DIR="${RESULTS_DIR:-/results}"
REDIS_TESTS_DIR="${REDIS_TESTS_DIR:-/opt/redis/tests}"
TCL_TAGS="${TCL_TAGS:--slow}"

SUITES="${SUITES:-unit/type/string unit/type/incr unit/type/list unit/type/hash unit/type/set unit/type/zset unit/expire unit/scan unit/bitops unit/geo unit/keyspace unit/protocol unit/pubsub unit/multi unit/type/stream unit/type/stream-cgroups unit/hyperloglog}"

mkdir -p "$RESULTS_DIR"
log() { echo "[run_suites $(date -u +%H:%M:%S)] $*"; }

wait_for_calod() {
  local tries=60 reply
  while (( tries-- > 0 )); do
    reply="$(
      exec 3<>"/dev/tcp/$HOST/$PORT" 2>/dev/null && {
        printf '*1\r\n$4\r\nPING\r\n' >&3
        head -c 16 <&3
        exec 3>&- 3<&-
      }
    )"
    if [[ "$reply" == *PONG* ]]; then
      return 0
    fi
    sleep 1
  done
  log "ERROR: calod did not answer PING on $HOST:$PORT"
  return 1
}

log "starting calod on $HOST:$PORT"
calod --address "$HOST" --port "$PORT" --reuse-address \
  > "$RESULTS_DIR/calod-server.log" 2>&1 &
CALOD_PID=$!
trap 'kill "$CALOD_PID" 2>/dev/null || true' EXIT

wait_for_calod || { log "aborting; calod log follows"; cat "$RESULTS_DIR/calod-server.log"; exit 1; }

cd "$REDIS_TESTS_DIR"
declare -a RESULT_LINES=()
FAILED_SUITES=()
PASSED_SUITES=()

for suite in $SUITES; do
  slug="${suite//\//_}"
  log "running suite: $suite"
  if ./runtest --host "$HOST" --port "$PORT" --tags "$TCL_TAGS" --single "$suite" \
      > "$RESULTS_DIR/${slug}.log" 2>&1; then
    verdict="PASS"
    PASSED_SUITES+=("$suite")
  else
    verdict="FAIL"
    FAILED_SUITES+=("$suite")
  fi
  # Extract failing assertions / exceptions for quick triage.
  grep -E '^\[(err|exception)\]' "$RESULTS_DIR/${slug}.log" \
    > "$RESULTS_DIR/${slug}-failures.txt" || true
  n_fail="$(wc -l < "$RESULTS_DIR/${slug}-failures.txt")"
  RESULT_LINES+=("| \`$suite\` | $verdict | $n_fail | [log](${slug}.log) |")
  log "suite $suite: $verdict ($n_fail failing assertions/exceptions)"
done

{
  echo "# Redis tcl-suite baseline report"
  echo
  echo "- Redis test suite tag: \`${REDIS_TAG:-unknown}\`"
  echo "- runtest tags: \`$TCL_TAGS\`"
  echo "- date (UTC): $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "- passed: ${#PASSED_SUITES[@]} / $(( ${#PASSED_SUITES[@]} + ${#FAILED_SUITES[@]} ))"
  echo
  echo "| suite | result | failing assertions | log |"
  echo "|---|---|---|---|"
  printf '%s\n' "${RESULT_LINES[@]}"
  if ((${#FAILED_SUITES[@]} > 0)); then
    echo
    echo "## Failed suites"
    printf -- '- `%s`\n' "${FAILED_SUITES[@]}"
  fi
} > "$RESULTS_DIR/summary.md"

log "summary written to $RESULTS_DIR/summary.md"
log "passed: ${#PASSED_SUITES[@]}, failed: ${#FAILED_SUITES[@]}"

if [[ "${STRICT:-0}" == "1" && ${#FAILED_SUITES[@]} -gt 0 ]]; then
  exit 1
fi
exit 0
