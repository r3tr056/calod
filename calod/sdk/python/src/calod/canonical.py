"""Canonical JSON rendering.

Mirrors the server's canonical form (``calod-ledger/src/digest.rs``): object
keys sorted recursively, no insignificant whitespace, minimal string escaping.
Used for stable wire payloads and for deriving default idempotency keys. The
authoritative input digest is always computed server-side; this exists so the
client can derive stable keys and send stable bytes.
"""

from __future__ import annotations

import json
from typing import Any


def canonical_dumps(value: Any) -> str:
    """Render ``value`` as canonical JSON (sorted keys, compact separators).

    ``sort_keys`` matches the server's recursive key ordering and
    ``separators=(",", ":")`` removes insignificant whitespace. Non-ASCII
    characters are emitted as UTF-8, matching serde_json's default.
    """
    return json.dumps(
        value, sort_keys=True, separators=(",", ":"), ensure_ascii=False
    )


def canonical_bytes(value: Any) -> bytes:
    """Canonical JSON as UTF-8 bytes."""
    return canonical_dumps(value).encode("utf-8")
