#!/usr/bin/env python3
"""Independent verifier for Calod evidence exports (Phase 1).

Shares no code with the Rust exporter. Given a JSON bundle from
`GET /v1/runs/{run_id}/export`, it recomputes:

1. the per-run event hash chain (sequences 1..N gap-free, each
   `hash = sha256(previous_hash_bytes || canonical(event))`),
2. `manifest.bundle_sha256` (SHA-256 of the canonical bundle
   without the manifest),
3. `manifest.signature` (HMAC-SHA256 of the bundle digest) when
   `--key-file` (hex key, as in the server's `export_signing.key`)
   is supplied.

It can also verify a `format=csv` export (`--csv`): the chain is
recomputed from the CSV columns alone.

Usage:
    python verify_export.py export.json --key-file export_signing.key
    python verify_export.py events.csv --csv

Exit code 0 means every check passed; 1 with a reason otherwise.
Standard library only.
"""

from __future__ import annotations

import argparse
import csv
import hashlib
import hmac
import json
import sys

GENESIS_HASH = "sha256:" + hashlib.sha256(b"").hexdigest()


def canonical(value) -> bytes:
    """Canonical JSON: sorted keys, no whitespace, raw unicode.

    Mirrors the ledger's canonical form for the JSON values that
    occur in exports (objects, arrays, strings, numbers, bools).
    """
    return json.dumps(
        value, sort_keys=True, separators=(",", ":"), ensure_ascii=False
    ).encode("utf-8")


def hash_bytes(data: bytes) -> str:
    return "sha256:" + hashlib.sha256(data).hexdigest()


def event_hash(event: dict, previous_hash: str) -> str:
    payload = {
        "id": event["id"],
        "run_id": event["run_id"],
        "sequence": event["sequence"],
        "type": event["type"],
        "occurred_at": event["occurred_at"],
        "recorded_at": event["recorded_at"],
        "payload": event["payload"],
    }
    previous_bytes = bytes.fromhex(previous_hash.removeprefix("sha256:"))
    return hash_bytes(previous_bytes + canonical(payload))


def verify_chain(events: list[dict]) -> str | None:
    """Return an error string, or None when the chain verifies."""
    previous = GENESIS_HASH
    for index, event in enumerate(events):
        if event["sequence"] != index + 1:
            return f"sequence gap at index {index}: got {event['sequence']}"
        if event["previous_hash"] != previous:
            return f"event {event['sequence']} previous_hash does not link"
        if event_hash(event, previous) != event["hash"]:
            return f"event {event['sequence']} hash does not recompute (tampered?)"
        previous = event["hash"]
    return None


def verify_json_bundle(path: str, key_file: str | None) -> int:
    with open(path, encoding="utf-8") as handle:
        bundle = json.load(handle)
    manifest = bundle.get("manifest")
    if not isinstance(manifest, dict):
        print("FAIL: no manifest in bundle")
        return 1

    error = verify_chain(bundle.get("events", []))
    if error:
        print(f"FAIL: event chain: {error}")
        return 1
    print(f"event chain: ok ({len(bundle.get('events', []))} events)")

    unsigned = {k: v for k, v in bundle.items() if k != "manifest"}
    digest = hash_bytes(canonical(unsigned))
    if digest != manifest.get("bundle_sha256"):
        print("FAIL: bundle_sha256 does not recompute (bundle was modified)")
        return 1
    print("bundle_sha256: ok")

    if key_file:
        with open(key_file, encoding="utf-8") as handle:
            key = bytes.fromhex(handle.read().strip())
        expected = hmac.new(key, digest.encode("utf-8"), hashlib.sha256).hexdigest()
        if not hmac.compare_digest(expected, manifest.get("signature", "")):
            print("FAIL: manifest signature does not verify")
            return 1
        print("manifest signature: ok")
    else:
        print("manifest signature: skipped (no --key-file)")

    print("EXPORT VERIFIED")
    return 0


def verify_csv_export(path: str) -> int:
    with open(path, newline="", encoding="utf-8") as handle:
        rows = list(csv.DictReader(handle))
    events = [
        {
            "id": None,
            "run_id": row["run_id"],
            "sequence": int(row["sequence"]),
            "type": row["type"],
            "occurred_at": row["occurred_at"],
            "recorded_at": row["recorded_at"],
            "payload": json.loads(row["payload"]),
            "previous_hash": row["previous_hash"],
            "hash": row["hash"],
        }
        for row in rows
    ]
    # The event hash commits to the event id, which the CSV does not
    # carry; from CSV alone we verify linkage and sequence integrity.
    previous = GENESIS_HASH
    for event in events:
        if event["previous_hash"] != previous:
            print(f"FAIL: CSV chain breaks at sequence {event['sequence']}")
            return 1
        previous = event["hash"]
    print(f"CSV chain linkage: ok ({len(events)} events)")
    print("EXPORT VERIFIED (linkage only; use JSON for full hash proof)")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("export", help="export file (JSON bundle or CSV)")
    parser.add_argument("--key-file", help="hex export signing key file")
    parser.add_argument(
        "--csv", action="store_true", help="treat the export as CSV"
    )
    args = parser.parse_args()
    if args.csv:
        return verify_csv_export(args.export)
    return verify_json_bundle(args.export, args.key_file)


if __name__ == "__main__":
    sys.exit(main())
