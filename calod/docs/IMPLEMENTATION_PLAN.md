# Calod v1.0 Implementation Plan

> **ARCHIVED — 2026-07-23.** This document planned Calod as "a faster Redis."
> That positioning is rejected. The current product direction is in
> `PRODUCT_STRATEGY_V2_REFINED.md` (durable action ledger for AI agents).
> This file is kept only as a technical reference: its hygiene, durability,
> security, and observability engineering work remains useful and is being
> reused. Do not plan against this document.

Status: finalized after codebase audit + requirements interview + technical research (2026-07-19).

## 1. North Star

**Calod is a faster Redis**: a drop-in replacement for Redis 7.4 that wins on
**throughput at equal core count** (target ≥1.5–2x vs Redis 7.4, p99 ≤ Redis,
proven with reproducible memtier_benchmark results vs Redis 7.4 *and* Valkey 8
with io-threads), with production-grade durability, replication + failover,
security, and observability.

Confirmed scope decisions (from requirements interview):

| Area | Decision |
|---|---|
| Topology | Single node + async replication with failover. No horizontal cluster in v1. |
| Compatibility | Near-total Redis 7.4 parity, incl. FUNCTION/FCALL and sharded pub/sub. |
| Durability | Redis-grade (AOF + rewrite, scheduled snapshots, all 16 DBs), own snapshot format. No RDB file compat. |
| Security | Real ACL engine + TLS (rustls). |
| Observability | Real SLOWLOG/LATENCY/MEMORY STATS + Prometheus endpoint + honest INFO. |
| Eviction | All 8 maxmemory policies incl. real LFU + active expiry. |
| Ops UX | redis.conf-style config file; CONFIG SET effectual; real CONFIG REWRITE. |
| AI extras | Frozen: graph module stays as-is, vector search off the roadmap, JSON stays. |
| Experiments | Keep `rdma` feature flag + `store/directory.rs` scaffolding; delete all other dead code/deps. |

## 2. Architecture Decisions (from research)

### A1. Threading: keep tokio, gate on benchmarks
Stay on tokio + sharded DashMap for v1. Redis 7.x executes commands on a single
thread, so 2x at equal cores is reachable without a rewrite. **Decision gate in
M0**: if the pinned baseline benchmark shows <1.5x vs Redis 7.4 at 4–8 cores,
adopt shard-affine dispatch (pinned worker per shard, cross-thread message
passing, Dragonfly-style) as a dedicated milestone before M8. Do NOT adopt
tokio-uring; io_uring, if ever, is isolated to the persistence layer.
Benchmark skewed-key (Gaussian) workloads from day one — DashMap hot-key shard
locks are the known p99 cliff.

### A2. Replication: per-shard journals, Redis-shaped management API
- **Not** wire-compatible PSYNC in v1 (RDB-format cost buys nothing). Custom
  internal sync protocol; operator surface stays Redis-shaped: `REPLICAOF`,
  `REPLICAOF NO ONE`, `ROLE`, Redis-compatible `INFO replication` fields.
- Per-shard in-memory **journal** (ring buffer) with per-shard monotonic LSNs;
  N replication flows per replica; resume token = LSN vector; lag = max over
  shards (Dragonfly's proven sharded model; calod writes are shard-local).
- **Two-generation replid semantics from day one** (replid/replid2 + offsets,
  conceptually identical to PSYNC2) so promotions allow partial resync.
  Retrofitting this later is painful.
- Full sync: per-shard version/epoch consistent cut over the existing bincode
  snapshot machinery; journal tail covers post-cut writes.
- **Invariant: the journal contains only deterministic effects.** Expiry and
  eviction synthesize `DEL`s on the primary; SPOP/SRANDMEMBER-class commands
  journal their concrete effects; blocking commands journal effects; Lua
  journals effects (MULTI/EXEC-wrapped writes), never the script text;
  replicas hide logically-expired keys on read but never expire independently.
- `WAIT numreplicas timeout` implemented against per-flow LSN acks (1 Hz).
- Failover: **external** (documented `REPLICAOF NO ONE` promotion with replid2
  handoff + Sentinel-protocol surface or sidecar health contract). No Raft, no
  Sentinel clone — no major Redis-compatible ships built-in consensus failover
  for this topology.

### A3. Durability: MP-AOF model
Follow Redis 7's multi-part AOF: `appendonlydir/` with a manifest, one base
file (calod's bincode snapshot format, not RDB preamble) + incremental RESP
files; atomic manifest swap on rewrite; open new incr file at rewrite start
(no in-memory rewrite buffer). `BGREWRITEAOF` becomes real; auto-rewrite
triggers `auto-aof-rewrite-min-size` (64mb) / `-percentage` (100); exponential
backoff after 3 consecutive failures. Scheduled snapshots honor
`snapshot_interval_secs`/`snapshot_min_changes` (already-present config
fields). All 16 DBs persisted (AOF must log `SELECT` frames; snapshots cover
every DB).

### A4. Security
- TLS: `tokio-rustls 0.26` / `rustls 0.23` with the `ring` provider (skip
  aws-lc-rs build pain), `rustls-pemfile`, `WebPkiClientVerifier` for
  `tls-auth-clients`, hot cert reload via `ResolvesServerCert` + `arc-swap` +
  `notify`. Separate listeners (`port 0` + `tls-port` model); no STARTTLS.
- ACL: build in-house (no usable crate exists). Rule-token parser per ACL
  SETUSER grammar (`>pass`, `#hash`, `~pat`, `%R~/%W~`, `&chan`, `+@cat`,
  `+cmd|sub`, `on/off/nopass/reset`), SHA-256 password hashes (Redis-compat),
  per-command category flags + key-spec metadata in the dispatch table,
  NOPERM + ACL LOG ring buffer. Selectors deferred to v1.1. Ship SETUSER /
  GETUSER / LIST / DELUSER / USERS / WHOAMI / CAT / GENPASS / LOG / LOAD /
  SAVE + `aclfile`.

### A5. Observability
- `metrics` + `metrics-exporter-prometheus` on a dedicated HTTP port (default
  9121) with per-command latency **histograms** (`calod_command_duration_seconds_bucket{cmd}`);
  the same histograms feed `LATENCY HISTOGRAM` and INFO
  `latency_percentiles_usec_*`. Keep INFO `commandstats` field-compatible so
  oliver006/redis_exporter sidecars keep working.
- SLOWLOG: copy Redis semantics verbatim (µs threshold `slowlog-log-slower-than`
  default 10000, 128-entry ring `slowlog-max-len`, argv truncated to 32
  args × 128 bytes, id/timestamp/duration/argv/client-addr/client-name).
- LATENCY: 160-entry per-event (second, max-ms) series; events `command`,
  `fast-command`, `aof-write`, `aof-fsync`, `expire-cycle`, `eviction-cycle`;
  LATEST/HISTORY/RESET/GRAPH real, DOCTOR as minimal heuristic.
- INFO honesty: fix `allocator`, `process_id`, `executable`, `os`; keep
  `redis_version` impersonation (compat requirement) but add `calod_version`.

### A6. Config: redis.conf-compatible
Small `sdssplitargs`-compatible tokenizer (quotes, `\x` escapes, `#` comments,
no line continuation), directive dispatch table with three outcomes: **map**,
**warn-and-ignore** (e.g. `daemonize`, `supervised`, `tls-ciphers`,
`io-threads`, `hz`, `activedefrag`), **fatal-unknown** (Redis behavior).
`include` with last-wins ordering. The same directive namespace backs CONFIG
GET/SET/REWRITE so file and runtime config never diverge — this replaces the
current write-only overlay in `store/config_registry.rs` and kills the dead
`calod/src/config.rs`. Accept-and-map minimum: port/bind/tls-*/requirepass/
aclfile/user/maxmemory*/databases/save/appendonly/appendfsync/replicaof/
loglevel/slowlog-*/latency-monitor-threshold/timeout/tcp-*/maxclients/
rename-command/dir/protected-mode.

### A7. Lua/functions
Lua 5.1 (mlua vendored) is correct — matches Redis. Add `cjson`, `cmsgpack`,
`struct`, LuaJIT-`bit` modules. FUNCTION/FCALL per Redis 7.x spec: shebang
library loading, `redis.register_function` (positional + named with flags),
flags `no-writes/allow-oom/allow-stale/no-cluster/allow-cross-slot-keys`,
FCALL/FCALL_RO, FUNCTION LIST/DELETE/FLUSH/KILL/DUMP/RESTORE/STATS,
**effects-only replication**, functions persisted (AOF/snapshot) and
replicated. Harden scripting generally: Lua VM pool (kill the per-invocation
`Lua::new()`), execution time limit + SCRIPT KILL/FUNCTION KILL, deterministic
sorting of set-returning replies inside scripts.

### A8. Sharded pub/sub
Implement SSUBSCRIBE/SUNSUBSCRIBE/SPUBLISH + PUBSUB SHARDCHANNELS/SHARDNUMSUB
as a second channel table sharing delivery logic with classic pub/sub —
`ssubscribe`/`smessage`/`sunsubscribe` verbs, node-local SPUBLISH counts, no
pattern variant. On a single node there is no slot routing; semantics are
otherwise identical.

## 3. Milestones (dependency-ordered)

Each milestone is independently shippable. Exit criteria are verifiable.

### M0 — Hygiene & measurement infrastructure
- Remove dead code/deps: `async-std`, `aes-gcm`, `haphazard`, `atomic_refcell`,
  `rng`, unused `calod/src/config.rs`, `mget_vectorized`/`keys_vectorized`,
  empty `cluster/`, `extensions/fastgraphdb/`, `io/memory/` dirs. **Keep**
  `rdma` feature + `store/directory.rs` + `tiered-memory` flag (scaffolding).
- Honest INFO fixes (allocator/process_id/executable/os, `calod_version`).
- Benchmark harness: scripted memtier setup (separate client host or
  cpuset-isolated, server pinned to N cores, Redis 7.4 pinned to same N cores
  with io-threads, Valkey 8 as second baseline; pipeline=1 and 16; 256–512B
  values; ≥3M keys; ratio 1:10 + pure SET/GET; uniform AND Gaussian key
  distributions; p50/p99/p99.9; ≥60s runs, multiple repetitions). Extend
  `calod/scripts/stress_runner.py` + `docker-compose.stress.yml`.
- Tcl suite runner: Redis 7.4 `tests/` with `./runtest --host/--port --single`
  for client-portable suites; record baseline pass/fail.
- **Exit**: clean `cargo clippy`, green existing integration suite, published
  baseline numbers, tcl subset baseline report. **Perf gate**: ≥1.5x vs Redis
  7.4 at equal cores → continue; else insert shard-affine-dispatch milestone.

### M1 — Config system (A6)
- redis.conf tokenizer + directive table; `include`; CLI flags merged with
  identical grammar; `CalodConfig::default()` replaced by real loader in
  `main.rs`; CONFIG SET effectual for every supported param (eviction policy,
  maxmemory, notify-keyspace-events, timeouts…); real CONFIG REWRITE;
  `rename-command` honored at dispatch.
- **Exit**: boot from unmodified sample redis.conf (mapped subset);
  `integration_config.rs` extended: every SET takes measurable effect.

### M2 — Command parity, batch 1 (mainstream gaps)
Implement per Redis 7.4 semantics (specs verified against redis.io; error
strings verified against a live server):
- Sets: SINTER/SUNION/SDIFF + STORE variants, SINTERCARD, SMOVE, SMISMEMBER,
  SSCAN; SRANDMEMBER negative-count repeats.
- Hashes: HRANDFIELD (incl. negative + WITHVALUES), HSCAN (+NOVALUES).
- Lists: LPOS, LMOVE/BLMOVE, LMPOP/BLMPOP, LPUSHX/RPUSHX.
- Zsets: ZUNIONSTORE/ZINTERSTORE/ZDIFFSTORE, ZRANGESTORE, ZMPOP/BZMPOP.
- Bitmaps: BITFIELD + BITFIELD_RO (WRAP/SAT/FAIL overflow).
- Streams: blocking XREAD/XREADGROUP (`BLOCK`, `$`, `+`), XAUTOCLAIM cursor
  semantics incl. deleted-ids third element.
- Geo: GEORADIUS/GEORADIUSBYMEMBER (WITH*/COUNT ANY/STORE/STOREDIST),
  GEOSEARCHSTORE.
- Strings/keys: GETEX options audit, SET GET wrong-type abort, OBJECT
  ENCODING reporting Redis-expected values (`int/embstr/raw`, `quicklist`,
  `intset/listpack/hashtable`, `skiplist`, `stream`).
- Every new command: parser variant in `cld_srv/command.rs`, dispatch in
  `served_store.rs`, AOF-effect form, integration test, oracle test coverage.
- **Exit**: `unit/type/*`, `unit/bitops`, `unit/bitfield`, `unit/geo`,
  `unit/scan` tcl suites passing; oracle diff clean.

### M3 — Durability (A3)
- Multi-DB AOF (`SELECT` frames) + multi-DB snapshots; scheduled snapshots;
  MP-AOF rewrite with manifest + atomic swap; real BGREWRITEAOF + auto
  triggers + failure backoff; INFO persistence fields; WAITAOF implemented
  against local fsync state or explicitly rejected with correct error.
- **Exit**: `integration_aof.rs`/`integration_persistence*.rs` extended
  (rewrite roundtrip, crash-mid-rewrite recovery, 16-DB replay); tcl
  `integration/aof*` client-portable parts.

### M4 — Eviction & expiry
- maxmemory accounting + enforcement with correct OOM error; all 8 policies;
  approximated LRU (24-bit clock + 16-entry eviction pool, `maxmemory-samples`);
  LFU (16-bit LDT + 8-bit Morris counter, `lfu-log-factor`/`lfu-decay-time`,
  init value 5); OBJECT FREQ/IDLETIME; active-expire slow cycle (10 Hz,
  `active-expire-effort`, 25% loop, time-boxed) + fast cycle; expiry/eviction
  emit DEL-effects to journal/AOF + `expired`/`evicted` notifications;
  MEMORY STATS/USAGE real.
- **Exit**: tcl `unit/expire`, `unit/maxmemory`-equivalent checks, eviction
  policy integration tests, memory-enforcement stress test.

### M5 — Security (A4)
- rustls listener (config: `tls-port`, cert/key/ca, `tls-auth-clients`), hot
  reload; ACL engine + commands + `aclfile`; AUTH <user> <pass>; NOPERM paths;
  `protected-mode`.
- **Exit**: tcl `unit/acl` subset, `unit/auth`, TLS integration tests (incl.
  mTLS), ACL file roundtrip.

### M6 — Functions, sharded pub/sub, Lua hardening (A7, A8)
- FUNCTION family per spec; Lua VM pool + time limit + SCRIPT/FUNCTION KILL;
  cjson/cmsgpack/struct/bit libs; deterministic reply sorting in scripts;
  sharded pub/sub commands.
- **Exit**: tcl `unit/functions`, `unit/pubsubshard`, `unit/scripting`
  client-portable parts; VM-reuse microbenchmark.

### M7 — Observability (A5)
- Real SLOWLOG/LATENCY; per-command histograms; Prometheus endpoint; INFO
  completeness (commandstats, latencystats, persistence, memory).
- **Exit**: redis_exporter sidecar scrape parity check; native /metrics
  histogram p99 matches LATENCY output.

### M8 — Replication & failover (A2)
- Per-shard journal in `store/` (effect-rewriting at journal boundary);
  full sync (epoch-cut snapshot + catch-up) + partial sync (journal tail);
  replica apply path + read-only enforcement + stale-data config; REPLICAOF /
  ROLE / INFO replication; per-flow LSN acks + WAIT; replid2 promotion
  handoff; expiry = primary-synthesized DELs, replica read-hiding; Sentinel
  surface or documented sidecar contract; manual failover runbook.
- **Exit**: 3-node integration tests (sync, reconnect partial resync,
  promotion without full resync, WAIT semantics, replica read-only), chaos
  test (kill -9 primary mid-load, promote, verify no divergence via digest).

### M9 — Performance verification & release hardening
- Re-run full benchmark matrix; close p99 cliffs (hot-key shard contention,
  writer batching, parser allocations); fuzz RESP parser; soak test (24h
  mixed workload + AOF + replication); upgrade/migration docs; README/BENCHMARKS
  honesty pass; 1.0.
- **Exit**: published reproducible numbers meeting the headline claim;
  tcl subset report in repo; zero known correctness bugs.

## 4. Cross-cutting invariants (enforce in every milestone)

1. Every mutating command has a deterministic **effect form** (journal/AOF)
   from the day it lands — retrofitting determinism is the classic replication
   killer.
2. Every new command lands with: parser + dispatch + RESP2/RESP3 replies +
   oracle test + integration test + COMMAND DOCS metadata + ACL category.
3. No canned replies. A stub either implements or returns the exact Redis
   error for an unsupported feature.
4. Benchmarks and tcl suite run in CI per milestone; perf regressions >5%
   block merge.

## 5. Top risks

1. **Baseline definition**: "faster than Redis" will be judged against
   Redis 8/Valkey 8 io-threads, not just Redis 7.4. Mitigate: benchmark both
   from M0; margin must come from per-core efficiency (memory layout, parsing).
2. **Replication determinism**: sharded concurrent writes make naive
   replication diverge. Mitigate: A2's journal invariant + digest-based
   divergence tests in M8.
3. **DashMap hot-key serialization**: p99 cliff under skew. Mitigate: Gaussian
   benchmarks from M0; shard-affine fallback ready.
4. **Parity breadth**: ~40 commands + functions + ACL is a large surface for
   near-total parity. Mitigate: tcl-suite-driven development — let Redis's
   own tests define done.
5. **Scope**: if timeline pressure appears, cut order is: M6 functions →
   M7 Prometheus-native (keep sidecar) → M5 mTLS/hot-reload. Never cut M0
   measurement, M2–M4 correctness, or M8 replication.
