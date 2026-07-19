# Benchmark harness (M0)

Measures calod against Redis 7.4 and Valkey 8 at **equal cores**, using
`memtier_benchmark`. This is the harness behind the plan's headline claim
(≥1.5–2x Redis 7.4 throughput at equal cores) — no numbers exist until this
has been run on a suitable Linux host.

## Quick start

```bash
cd calod
./scripts/benchmark/run_benchmarks.sh
```

Results land in `calod/benchmark-results/<utc-timestamp>/`:

- one memtier JSON + text artifact per (server, workload cell),
- `run_meta.json` — exact run configuration,
- `summary.md` — throughput and p50/p99/p99.9 tables comparing all three
  servers per cell, with calod/redis and calod/valkey ratios.

Useful overrides (all env vars):

```bash
N_CORES=8 ./scripts/benchmark/run_benchmarks.sh      # 8 cores per server
PIPELINES="16" RATIOS="1:10" TEST_TIME=30 \
  ./scripts/benchmark/run_benchmarks.sh              # fast smoke matrix
SERVERS="calod redis" ./scripts/benchmark/run_benchmarks.sh
REPETITIONS=3 ./scripts/benchmark/run_benchmarks.sh  # repeat every cell
```

## Methodology

**Core parity.** Every server under test is pinned to the *same* cpuset of
`N_CORES` cores (default 4, `SERVER_CPUSET=0-3`); the memtier client is
pinned to a disjoint range of equal width (`CLIENT_CPUSET=4-7`) so client and
server never share a core. Pinning is done via `cpuset` in
`docker-compose.benchmark.yml`.

**Baselines.** Redis 7.4 (`redis:7.4.2-bookworm`) and Valkey 8
(`valkey/valkey:8.0.2-bookworm`) run with `--io-threads $((N_CORES-1))
--io-threads-do-reads yes` — the main thread counts separately, so total
threads equal `N_CORES`. Both run with persistence off
(`--save '' --appendonly no`); calod's persistence is off by default.

**Workload matrix** (per server, per repetition):

| dimension | values |
|---|---|
| pipeline | 1, 16 |
| ratio (SET:GET) | 1:10, 0:1 (pure GET), 1:0 (pure SET) |
| value size | 256 B, 512 B (`--random-data`) |
| key distribution | uniform (`R:R`), Gaussian (`G:G`, median = keyspace centre, 6σ spans keyspace) |
| clients | 50 × 4 threads (200 conns), plus a 650-connection point (50 × 13) at pipeline=16, ratio=1:10, 256 B |
| keyspace | 3,000,000 keys (`--key-minimum 1 --key-maximum 3000000`) |
| duration | 60 s per cell (`--test-time`), `--distinct-client-seed` |

Data is **preloaded** (sequential SETs covering the keyspace) before any
measurement cell runs, so GET workloads measure hits, not cold misses.

The default matrix is 25 cells per server (~26 min per server at 60 s each,
plus preload). Narrow dimensions with the env vars above for smoke runs.

## Machine requirements

- Linux host (cpusets are Linux-only), Docker with compose v2.
- **8+ cores** minimum for the default `N_CORES=4` (4 for the server, 4 for
  the client). Scale `N_CORES` to half the machine's cores.
- **Separate client host recommended** for publishable numbers: run the
  servers under the compose file on one machine and memtier from another
  (point `--server` at the server host) to remove client CPU contention
  entirely. The single-host cpuset split is the fallback, not the ideal.
- Host hygiene (checked with warnings by the script, needs root to fix):
  - `sysctl vm.overcommit_memory=1`
  - `echo never > /sys/kernel/mm/transparent_hugepage/enabled`
  - `sysctl net.core.somaxconn=65535` (containers also set it via compose
    `sysctls`; host value matters for the listener backlog)
  - `nofile` raised to 65536 (set via compose `ulimits`)

## How to interpret

- The gate from `docs/IMPLEMENTATION_PLAN.md` M0: **calod ops/sec ≥ 1.5x
  Redis 7.4 ops/sec at equal cores** for the mainstream cells. Read the
  `calod/redis` column in `summary.md`.
- Compare like-for-like cells only; pipeline=16 numbers are typically ~10x
  pipeline=1 and say nothing about per-connection latency.
- Check p99/p99.9 alongside throughput: a throughput win with a latency cliff
  (e.g. hot-key shard contention under the Gaussian cells) is a regression,
  not a win.
- Repetitions (`REPETITIONS=3`) matter: single 60 s runs on a shared host
  can swing several percent.

## Why a separate compose file?

`docker-compose.benchmark.yml` is separate from `docker-compose.stress.yml`
because the two harnesses conflict: stress uses fractional `cpus:` limits and
a Python workload runner, while the benchmark needs hard `cpuset` pinning,
three servers with distinct io-threads configs, and a memtier client service
that must never start on `up`. Keeping them apart leaves the stress harness
untouched.
