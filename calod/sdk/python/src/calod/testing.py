"""Test/dev utilities: spin up a real local Calod server for the SDK.

**Dev/test only.** The server has no key-provisioning endpoint by design (keys
are created out-of-band by the operator), so this module writes the server's
``api_keys.bin`` directly, mirroring what the Rust integration tests do
in-process via ``ApiKeyStore`` (``calod-core/src/store/api_keys.rs``):

    | magic "CLDK" (4B) | version (1B) | bincode payload | crc32c trailer (4B LE) |

The payload is a bincode-serialized ``Vec<ApiKeyRecord>``. Because the store
keeps only an argon2 PHC hash of each secret, this module embeds a precomputed
PHC string for one fixed, publicly visible dev key. That key is ONLY ever
valid against throwaway local servers this module itself provisions in a fresh
temp directory. Never use it — or this module — against a real deployment.
"""

from __future__ import annotations

import os
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time
import uuid
from pathlib import Path
from typing import Optional, Sequence

# A fixed, publicly visible DEV-ONLY key pair. `DEV_API_KEY` is the plaintext
# you pass to the SDK; `_DEV_KEY_PHC` is its argon2id PHC hash, written into
# the throwaway server's key store. Regenerate the pair with argon2-cffi:
#     from argon2 import PasswordHasher; PasswordHasher().hash(DEV_API_KEY)
DEV_API_KEY = "cld_" + "0123456789abcdef" * 4
_DEV_KEY_PHC = (
    "$argon2id$v=19$m=65536,t=3,p=4$"
    "rkL+fM8ih9/em6J83nm+pg$Ew+XC2b7GdRcJdIOtKXBFDQd+mUjdj96HA8B78MLvRw"
)

_DEFAULT_SCOPES = ("effects:read", "effects:write", "effects:reconcile")


# ---------------------------------------------------------------------------
# Pure-Python primitives (no third-party deps)
# ---------------------------------------------------------------------------

_XXH64_MASK = 0xFFFFFFFFFFFFFFFF
_XXH64_P1 = 11400714785074694791
_XXH64_P2 = 14029467366897019727
_XXH64_P3 = 1609587929392839161
_XXH64_P4 = 9650029242287828579
_XXH64_P5 = 2870177450012600261


def _rotl64(x: int, r: int) -> int:
    return ((x << r) | (x >> (64 - r))) & _XXH64_MASK


def _xxh64_round(acc: int, lane: int) -> int:
    acc = (acc + lane * _XXH64_P2) & _XXH64_MASK
    acc = _rotl64(acc, 31)
    return (acc * _XXH64_P1) & _XXH64_MASK


def _xxh64_merge(acc: int, val: int) -> int:
    acc ^= _xxh64_round(0, val)
    return (acc * _XXH64_P1 + _XXH64_P4) & _XXH64_MASK


def xxh64(data: bytes, seed: int = 0) -> int:
    """XXHash64 (seed 0 matches ``twox_hash::XxHash64::default()``)."""
    length = len(data)
    i = 0
    if length >= 32:
        v1 = (seed + _XXH64_P1 + _XXH64_P2) & _XXH64_MASK
        v2 = (seed + _XXH64_P2) & _XXH64_MASK
        v3 = seed & _XXH64_MASK
        v4 = (seed - _XXH64_P1) & _XXH64_MASK
        while i + 32 <= length:
            v1 = _xxh64_round(v1, int.from_bytes(data[i : i + 8], "little"))
            v2 = _xxh64_round(v2, int.from_bytes(data[i + 8 : i + 16], "little"))
            v3 = _xxh64_round(v3, int.from_bytes(data[i + 16 : i + 24], "little"))
            v4 = _xxh64_round(v4, int.from_bytes(data[i + 24 : i + 32], "little"))
            i += 32
        h = (
            _rotl64(v1, 1) + _rotl64(v2, 7) + _rotl64(v3, 12) + _rotl64(v4, 18)
        ) & _XXH64_MASK
        h = _xxh64_merge(_xxh64_merge(_xxh64_merge(_xxh64_merge(h, v1), v2), v3), v4)
    else:
        h = (seed + _XXH64_P5) & _XXH64_MASK
    h = (h + length) & _XXH64_MASK
    while i + 8 <= length:
        h ^= _xxh64_round(0, int.from_bytes(data[i : i + 8], "little"))
        h = (_rotl64(h, 27) * _XXH64_P1 + _XXH64_P4) & _XXH64_MASK
        i += 8
    if i + 4 <= length:
        h ^= (int.from_bytes(data[i : i + 4], "little") * _XXH64_P1) & _XXH64_MASK
        h = (_rotl64(h, 23) * _XXH64_P2 + _XXH64_P3) & _XXH64_MASK
        i += 4
    while i < length:
        h ^= (data[i] * _XXH64_P5) & _XXH64_MASK
        h = (_rotl64(h, 11) * _XXH64_P1) & _XXH64_MASK
        i += 1
    h ^= h >> 33
    h = (h * _XXH64_P2) & _XXH64_MASK
    h ^= h >> 29
    h = (h * _XXH64_P3) & _XXH64_MASK
    h ^= h >> 32
    return h


def _crc32c_table() -> list[int]:
    poly = 0x82F63B78
    table = []
    for n in range(256):
        c = n
        for _ in range(8):
            c = (c >> 1) ^ poly if c & 1 else c >> 1
        table.append(c)
    return table


_CRC32C_TABLE = _crc32c_table()


def crc32c(data: bytes) -> int:
    """CRC-32C (Castagnoli), matching the ``crc32c`` crate."""
    crc = 0xFFFFFFFF
    for byte in data:
        crc = _CRC32C_TABLE[(crc ^ byte) & 0xFF] ^ (crc >> 8)
    return crc ^ 0xFFFFFFFF


def _bincode_str(value: str) -> bytes:
    encoded = value.encode("utf-8")
    return struct.pack("<Q", len(encoded)) + encoded


def write_api_key_file(
    data_dir: os.PathLike[str] | str,
    *,
    scopes: Sequence[str] = _DEFAULT_SCOPES,
    tenant_id: str = "tenant-dev",
    project_id: str = "project-dev",
) -> str:
    """Write ``api_keys.bin`` with the dev key into ``data_dir``.

    Returns the plaintext dev key (:data:`DEV_API_KEY`). The directory is
    created if needed. Mirrors ``ApiKeyStore::create`` + ``persist_locked``.
    """
    key = DEV_API_KEY
    record = b"".join(
        [
            # Uuid: serde serialize_bytes under bincode = u64 len + 16 bytes.
            struct.pack("<Q", 16) + uuid.uuid4().bytes,
            _bincode_str(tenant_id),
            _bincode_str(project_id),
            _bincode_str(_DEV_KEY_PHC),
            struct.pack("<Q", xxh64(key.encode("utf-8"))),
            # BTreeSet<String>: u64 count + sorted strings.
            struct.pack("<Q", len(scopes)),
            b"".join(_bincode_str(s) for s in sorted(scopes)),
            struct.pack("<Q", int(time.time())),  # created_at
            b"\x00",  # expires_at: None
            b"\x00",  # revoked: false
        ]
    )
    payload = struct.pack("<Q", 1) + record  # Vec<ApiKeyRecord>
    out = b"CLDK" + b"\x01" + payload
    out += struct.pack("<I", crc32c(out))

    path = Path(data_dir)
    path.mkdir(parents=True, exist_ok=True)
    (path / "api_keys.bin").write_bytes(out)
    return key


# ---------------------------------------------------------------------------
# Local server harness
# ---------------------------------------------------------------------------


def _find_binary() -> Path:
    env = os.environ.get("CALOD_SERVER_BIN")
    candidates: list[Path] = []
    if env:
        candidates.append(Path(env))
    exe = "calod.exe" if sys.platform.startswith("win") else "calod"
    # sdk/python/src/calod/testing.py -> repo calod/ dir is parents[4].
    repo = Path(__file__).resolve().parents[4]
    candidates.append(repo / "target" / "debug" / exe)
    candidates.append(repo / "target" / "release" / exe)
    which = shutil.which("calod")
    if which:
        candidates.append(Path(which))
    for candidate in candidates:
        if candidate.is_file():
            return candidate
    raise FileNotFoundError(
        "calod server binary not found; build it with `cargo build` in the "
        "calod/ directory or set CALOD_SERVER_BIN"
    )


def _free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


class LocalServer:
    """A throwaway Calod server process for local development and tests.

    Usage::

        with LocalServer() as server:
            client = calod.CalodClient(server.base_url, server.api_key)
            ...

    Mirrors the Rust harness in ``tests/integration_api.rs``: the RESP
    listener and REST API each get an ephemeral port, persistence roots at a
    fresh temp dir, and the ledger runs with ``CALOD_API__DURABILITY=always``.
    """

    def __init__(
        self,
        *,
        scopes: Sequence[str] = _DEFAULT_SCOPES,
        binary: Optional[os.PathLike[str] | str] = None,
        ready_timeout: float = 30.0,
    ) -> None:
        self._scopes = tuple(scopes)
        self._binary = Path(binary) if binary else _find_binary()
        self._ready_timeout = ready_timeout
        self._proc: Optional[subprocess.Popen[bytes]] = None
        self.data_dir: Optional[Path] = None
        self.api_port: int = 0
        self.api_key: str = ""

    @property
    def base_url(self) -> str:
        return f"http://127.0.0.1:{self.api_port}"

    def start(self) -> "LocalServer":
        self.data_dir = Path(tempfile.mkdtemp(prefix="calod-sdk-test-"))
        self.api_key = write_api_key_file(self.data_dir, scopes=self._scopes)
        self._spawn()
        self._wait_ready()
        return self

    def restart(self) -> None:
        """Kill the process and start a fresh one over the same data dir.

        Simulates a crash: no graceful shutdown, no flush. The ledger and key
        store recover from disk, exactly like the Rust crash-recovery tests.
        """
        if self.data_dir is None:
            raise RuntimeError("server has not been started")
        self.stop()
        self._spawn()
        self._wait_ready()

    def _spawn(self) -> None:
        assert self.data_dir is not None
        resp_port = _free_port()
        self.api_port = _free_port()
        log = open(self.data_dir / "server.log", "ab")  # noqa: SIM115
        env = {
            **os.environ,
            "CALOD_PERSISTENCE__ENABLE_SNAPSHOT": "false",
            "CALOD_PERSISTENCE__SNAPSHOT_DIR": str(self.data_dir),
            "CALOD_PERSISTENCE__ENABLE_AOF": "false",
            "CALOD_PERSISTENCE__SNAPSHOT_INTERVAL_SECS": "3600",
            "CALOD_API__ENABLED": "true",
            "CALOD_API__LISTEN_ADDR": f"127.0.0.1:{self.api_port}",
            "CALOD_API__DATA_DIR": str(self.data_dir / "ledger"),
            "CALOD_API__DURABILITY": "always",
        }
        self._proc = subprocess.Popen(  # noqa: S603
            [str(self._binary), "--address", "127.0.0.1", "--port", str(resp_port)],
            cwd=str(self.data_dir),
            env=env,
            stdout=log,
            stderr=subprocess.STDOUT,
        )
        self._log = log

    def _wait_ready(self) -> None:
        import httpx

        deadline = time.monotonic() + self._ready_timeout
        with httpx.Client(timeout=2.0) as http:
            while time.monotonic() < deadline:
                if self._proc is not None and self._proc.poll() is not None:
                    self._log.close()
                    raise RuntimeError(
                        f"calod server exited early (rc={self._proc.returncode}); "
                        f"see {self.data_dir / 'server.log'}"
                    )
                try:
                    if http.get(f"{self.base_url}/v1/health/ready").status_code == 200:
                        return
                except httpx.TransportError:
                    pass
                time.sleep(0.1)
        self.stop()
        raise TimeoutError("calod server did not become ready in time")

    def stop(self) -> None:
        if self._proc is not None and self._proc.poll() is None:
            self._proc.kill()
            self._proc.wait(timeout=10)
        self._proc = None

    def __enter__(self) -> "LocalServer":
        return self.start()

    def __exit__(self, *exc: object) -> None:
        self.stop()


def local_server(**kwargs: object) -> LocalServer:
    """Shortcut for ``LocalServer(**kwargs)`` usable as a context manager."""
    return LocalServer(**kwargs)  # type: ignore[arg-type]
