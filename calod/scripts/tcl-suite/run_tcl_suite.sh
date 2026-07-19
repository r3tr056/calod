#!/usr/bin/env bash
#
# run_tcl_suite.sh — host-side wrapper for the Redis tcl-suite runner.
#
# Builds the runner image (calod release binary + tcl + redis/redis tests at
# the pinned tag) and runs the client-portable suite subset against calod.
# Reports land in calod/tcl-results/.
#
# Usage:
#   ./run_tcl_suite.sh                          # full client-portable subset
#   SUITES="unit/type/string" ./run_tcl_suite.sh
#   STRICT=1 ./run_tcl_suite.sh                 # non-zero exit on any failure
#   REDIS_TAG=7.4.2 ./run_tcl_suite.sh          # pin a different tag
#
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
IMAGE="calod-tcl-suite:${REDIS_TAG:-7.4.2}"
RESULTS_DIR="$ROOT_DIR/tcl-results"
mkdir -p "$RESULTS_DIR"

echo "[run_tcl_suite] building $IMAGE"
docker build \
  --build-arg "REDIS_TAG=${REDIS_TAG:-7.4.2}" \
  -f "$ROOT_DIR/docker/tcl-suite/Dockerfile" \
  -t "$IMAGE" \
  "$ROOT_DIR"

echo "[run_tcl_suite] running suites (results -> $RESULTS_DIR)"
docker run --rm \
  -e "SUITES=${SUITES:-}" \
  -e "TCL_TAGS=${TCL_TAGS:--slow}" \
  -e "STRICT=${STRICT:-0}" \
  -v "$RESULTS_DIR:/results" \
  "$IMAGE"

echo "[run_tcl_suite] done; see $RESULTS_DIR/summary.md"
