#!/usr/bin/env python3
"""Aggregate memtier_benchmark JSON artifacts into summary.md.

Usage: summarize.py <results-dir>

Reads every measurement artifact named
    <tag>__pl<p>__set<s>get<g>__<size>b__<dist>__c<conns>__<server>.json
inside <results-dir> (files produced by run_benchmarks.sh) and writes
<results-dir>/summary.md with a throughput table and a latency table,
one row per workload cell, comparing calod / redis / valkey.
"""

import json
import re
import sys
from pathlib import Path

SERVERS = ("calod", "redis", "valkey")
CELL_RE = re.compile(
    r"^(?P<tag>.+?)__pl(?P<pipeline>\d+)__set(?P<set>\d+)get(?P<get>\d+)"
    r"__(?P<size>\d+)b__(?P<dist>[a-z]+)__c(?P<conns>\d+)__(?P<server>[a-z0-9-]+)\.json$"
)


def find_percentiles(node):
    """Recursively find a 'Percentile Latencies'-style mapping in memtier JSON.

    memtier versions differ in where (or whether) percentiles land in the
    JSON output, so search defensively and normalise keys like
    '50.000', 'p50', '99.00' into floats.
    """
    found = {}
    if isinstance(node, dict):
        for key, value in node.items():
            low = str(key).lower()
            if "percentile" in low and isinstance(value, dict):
                for pk, pv in value.items():
                    m = re.search(r"(\d+(?:\.\d+)?)", str(pk))
                    if m and isinstance(pv, (int, float)):
                        found[float(m.group(1))] = float(pv)
            else:
                found.update(find_percentiles(value))
    elif isinstance(node, list):
        for item in node:
            found.update(find_percentiles(item))
    return found


def nearest(percentiles, target):
    if not percentiles:
        return None
    best = min(percentiles, key=lambda p: abs(p - target))
    return percentiles[best] if abs(best - target) <= 0.5 else None


def extract_metrics(path):
    with open(path, encoding="utf-8") as fh:
        data = json.load(fh)
    totals = data.get("ALL STATS", {}).get("Totals", {})
    ops = totals.get("Ops/sec")
    percentiles = find_percentiles(data)
    return {
        "ops_sec": ops,
        "p50_ms": nearest(percentiles, 50.0),
        "p99_ms": nearest(percentiles, 99.0),
        "p999_ms": nearest(percentiles, 99.9),
    }


def fmt(value, digits=0):
    if value is None:
        return "n/a"
    return f"{value:,.{digits}f}"


def main():
    results_dir = Path(sys.argv[1])
    cells = {}  # cell_key -> {server: metrics}
    for path in sorted(results_dir.glob("*.json")):
        if path.name in ("run_meta.json",) or path.name.startswith("preload__"):
            continue
        match = CELL_RE.match(path.name)
        if not match:
            print(f"summarize: skipping unrecognised artifact {path.name}", file=sys.stderr)
            continue
        info = match.groupdict()
        cell_key = (
            int(info["pipeline"]),
            f'{info["set"]}:{info["get"]}',
            int(info["size"]),
            info["dist"],
            int(info["conns"]),
            info["tag"],
        )
        cells.setdefault(cell_key, {})[info["server"]] = extract_metrics(path)

    lines = [
        "# Benchmark summary",
        "",
        "memtier_benchmark; servers pinned to equal cpusets "
        "(see `run_meta.json` for full run configuration). "
        "ops/sec from memtier `ALL STATS.Totals`; "
        "latencies are milliseconds. `n/a` = metric absent from artifact.",
        "",
    ]

    header = (
        "| pipeline | ratio (set:get) | size (B) | key dist | conns | tag "
        "| calod ops/s | redis ops/s | valkey ops/s | calod/redis | calod/valkey |"
    )
    sep = "|" + "---|" * 11
    lines += ["## Throughput", "", header, sep]
    for key in sorted(cells):
        pl, ratio, size, dist, conns, tag = key
        row = cells[key]
        calod = row.get("calod", {}).get("ops_sec")
        redis = row.get("redis", {}).get("ops_sec")
        valkey = row.get("valkey", {}).get("ops_sec")
        r_ratio = f"{calod / redis:.2f}x" if calod and redis else "n/a"
        v_ratio = f"{calod / valkey:.2f}x" if calod and valkey else "n/a"
        lines.append(
            f"| {pl} | {ratio} | {size} | {dist} | {conns} | {tag} "
            f"| {fmt(calod)} | {fmt(redis)} | {fmt(valkey)} | {r_ratio} | {v_ratio} |"
        )

    lines += ["", "## Latency (calod / redis / valkey, ms)", ""]
    header = (
        "| pipeline | ratio (set:get) | size (B) | key dist | conns | tag "
        "| p50 | p99 | p99.9 |"
    )
    lines += [header, "|" + "---|" * 9]
    for key in sorted(cells):
        pl, ratio, size, dist, conns, tag = key
        row = cells[key]
        triples = []
        for percentile in ("p50_ms", "p99_ms", "p999_ms"):
            triples.append(
                " / ".join(fmt(row.get(s, {}).get(percentile), 3) for s in SERVERS)
            )
        lines.append(
            f"| {pl} | {ratio} | {size} | {dist} | {conns} | {tag} "
            f"| {triples[0]} | {triples[1]} | {triples[2]} |"
        )

    lines.append("")
    out = results_dir / "summary.md"
    out.write_text("\n".join(lines), encoding="utf-8")
    print(f"summarize: wrote {out} ({len(cells)} cells)")


if __name__ == "__main__":
    main()
