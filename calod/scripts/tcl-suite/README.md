# Redis tcl-suite runner (M0)

Runs the **client-portable subset** of the official Redis test suite
(`redis/redis`, pinned tag **7.4.2**) against calod, and records a per-suite
pass/fail baseline. This defines "done" for command-parity work in later
milestones.

## Quick start

```bash
cd calod
./scripts/tcl-suite/run_tcl_suite.sh
```

Reports land in `calod/tcl-results/`:

- `summary.md` — pass/fail table with per-suite failing-assertion counts,
- `<suite>.log` — full runtest output per suite,
- `<suite>-failures.txt` — just the `[err]` / `[exception]` lines,
- `calod-server.log` — calod stdout/stderr for the run.

Overrides:

```bash
SUITES="unit/type/string unit/type/incr" ./scripts/tcl-suite/run_tcl_suite.sh
STRICT=1 ./scripts/tcl-suite/run_tcl_suite.sh   # exit 1 if any suite fails
TCL_TAGS="slow" ./scripts/tcl-suite/run_tcl_suite.sh
REDIS_TAG=7.4.2 ./scripts/tcl-suite/run_tcl_suite.sh
```

Default `TCL_TAGS` is `-slow` (skip slow tests) to keep a full baseline run
to minutes rather than an hour.

## How it works

`docker/tcl-suite/Dockerfile` builds calod in release mode, shallow-clones
`redis/redis` at the pinned tag, and installs `tcl`/`tcl-tls`. The container
entrypoint (`run_suites.sh`) starts calod on `127.0.0.1:6379`, waits for a
RESP `PING`, then runs `./runtest --host 127.0.0.1 --port 6379 --single
<suite>` per suite.

The runner exits 0 even when suites fail — the M0 goal is to **record the
baseline**, not gate on it. Use `STRICT=1` once a suite is expected to pass.

## Suites run (client-portable subset)

`unit/type/string`, `unit/type/incr`, `unit/type/list`, `unit/type/hash`,
`unit/type/set`, `unit/type/zset`, `unit/expire`, `unit/scan`, `unit/bitops`,
`unit/geo`, `unit/keyspace`, `unit/protocol`, `unit/pubsub`, `unit/multi`,
`unit/type/stream`, `unit/type/stream-cgroups`, `unit/hyperloglog`.

Known caveats for these suites when pointed at an external server:

- Tests that call `start_server` (auxiliary server for replication, DEBUG,
  restart scenarios) fail because there is no `redis-server` binary in the
  image — by design; those tests are not client-portable even though their
  suite is listed. Their `[err]` lines are expected in the baseline.
- `unit/type/list` in 7.4 is split into `list`, `list-2`, `list-3`; only
  `list` is in the default subset. Add `list-2`/`list-3` via `SUITES=` once
  `unit/type/list` is green.

## Suites NOT run, and why

| suite(s) | why inapplicable | milestone that makes it pass |
|---|---|---|
| `unit/type/list-2`, `list-3`, extended zset/hash coverage | full type parity | M2 |
| `unit/bitfield` | BITFIELD/BITFIELD_RO not yet implemented | M2 |
| `unit/expire`, `unit/maxmemory` (policy parts) | eviction policies, active-expire cycles, maxmemory enforcement | M4 |
| `integration/aof*` | MP-AOF, rewrite, multi-DB persistence | M3 |
| `unit/other` (persistence parts), `unit/dump` | BGSAVE/SAVE, DEBUG JMAP-equivalents, DEBUG RELOAD | M3 |
| `unit/acl`, `unit/auth` | ACL engine, AUTH user/pass | M5 |
| `unit/tls` | TLS listener | M5 |
| `unit/functions`, `unit/scripting` | FUNCTION family, Lua hardening | M6 |
| `unit/pubsub`, `unit/pubsubshard` | pub/sub + sharded pub/sub | M6 |
| `unit/slowlog`, `unit/latency-monitor`, `unit/info` (full) | SLOWLOG/LATENCY, INFO completeness | M7 |
| `integration/replication*`, `unit/wait`, `integration/failover` | replication, partial resync, WAIT | M8 |
| `integration/*` (remainder), `unit/cluster*` | need server-internal control planes calod does not expose | M3/M8 |
| `unit/querybuf`, `unit/pending-querybuf`, `unit/memefficiency`, `unit/introspection*` | poke Redis internals (memory layout, DEBUG) with no calod equivalent | not planned |
