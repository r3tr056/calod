"""Typed models for the Calod action-ledger REST API.

Plain dataclasses parsed from the `/v1` JSON payloads. Parsing is tolerant of
unknown fields so newer server versions don't break older SDKs.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Generic, Optional, TypeVar

T = TypeVar("T")


def _expect_str(value: Any, name: str) -> str:
    if not isinstance(value, str):
        raise ValueError(f"malformed server payload: {name} must be a string")
    return value


@dataclass(frozen=True)
class EffectLease:
    expires_at: str

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> "EffectLease":
        return cls(expires_at=_expect_str(data.get("expires_at"), "expires_at"))


@dataclass(frozen=True)
class ExternalReceipt:
    provider: str
    id: str
    metadata: Any = None


@dataclass(frozen=True)
class EffectFailure:
    kind: str
    message: str
    retryable: bool = False

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> "EffectFailure":
        return cls(
            kind=_expect_str(data.get("kind"), "failure.kind"),
            message=_expect_str(data.get("message"), "failure.message"),
            retryable=bool(data.get("retryable", False)),
        )


@dataclass(frozen=True)
class Effect:
    """An idempotent effect record and its current receipt."""

    id: str
    scope: str
    idempotency_key: str
    tool_name: str
    input_digest: str
    state: str
    fencing_token: int
    run_id: Optional[str] = None
    lease: Optional[EffectLease] = None
    result: Any = None
    external_receipt: Optional[ExternalReceipt] = None
    failure: Optional[EffectFailure] = None
    created_at: str = ""
    updated_at: str = ""

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> "Effect":
        lease = data.get("lease")
        receipt = data.get("external_receipt")
        failure = data.get("failure")
        return cls(
            id=_expect_str(data.get("id"), "id"),
            scope=_expect_str(data.get("scope"), "scope"),
            idempotency_key=_expect_str(data.get("idempotency_key"), "idempotency_key"),
            tool_name=_expect_str(data.get("tool_name"), "tool_name"),
            input_digest=_expect_str(data.get("input_digest"), "input_digest"),
            state=_expect_str(data.get("state"), "state"),
            fencing_token=int(data.get("fencing_token") or 0),
            run_id=data.get("run_id"),
            lease=EffectLease.from_dict(lease) if isinstance(lease, dict) else None,
            result=data.get("result"),
            external_receipt=(
                ExternalReceipt(
                    provider=str(receipt.get("provider")),
                    id=str(receipt.get("id")),
                    metadata=receipt.get("metadata"),
                )
                if isinstance(receipt, dict)
                else None
            ),
            failure=EffectFailure.from_dict(failure) if isinstance(failure, dict) else None,
            created_at=str(data.get("created_at", "")),
            updated_at=str(data.get("updated_at", "")),
        )


@dataclass(frozen=True)
class EffectDecision:
    """Response of `POST /v1/effects`.

    ``decision`` is one of ``execute`` | ``replay`` | ``wait`` | ``reconcile``.
    ``lease_token`` is present only for ``execute``. ``retry_after`` is the
    whole-second ``Retry-After`` header value on a ``wait`` decision.
    """

    decision: str
    effect: Effect
    lease_token: Optional[str] = None
    poll_after_ms: Optional[int] = None
    retry_after: Optional[float] = None


@dataclass(frozen=True)
class Run:
    id: str
    external_id: str
    agent_name: str
    status: str
    version: int
    last_event_sequence: int
    parent_run_id: Optional[str] = None
    forked_from_event_sequence: Optional[int] = None
    metadata: Any = None
    created_at: str = ""
    updated_at: str = ""

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> "Run":
        return cls(
            id=_expect_str(data.get("id"), "id"),
            external_id=_expect_str(data.get("external_id"), "external_id"),
            agent_name=_expect_str(data.get("agent_name"), "agent_name"),
            status=_expect_str(data.get("status"), "status"),
            version=int(data.get("version") or 0),
            last_event_sequence=int(data.get("last_event_sequence") or 0),
            parent_run_id=data.get("parent_run_id"),
            forked_from_event_sequence=data.get("forked_from_event_sequence"),
            metadata=data.get("metadata"),
            created_at=str(data.get("created_at", "")),
            updated_at=str(data.get("updated_at", "")),
        )


@dataclass(frozen=True)
class Event:
    id: str
    run_id: str
    sequence: int
    type: str
    occurred_at: str
    recorded_at: str
    previous_hash: str
    hash: str
    payload: Any = None

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> "Event":
        return cls(
            id=_expect_str(data.get("id"), "id"),
            run_id=_expect_str(data.get("run_id"), "run_id"),
            sequence=int(data.get("sequence") or 0),
            type=_expect_str(data.get("type"), "type"),
            occurred_at=str(data.get("occurred_at", "")),
            recorded_at=str(data.get("recorded_at", "")),
            previous_hash=str(data.get("previous_hash", "")),
            hash=str(data.get("hash", "")),
            payload=data.get("payload"),
        )


@dataclass(frozen=True)
class Page(Generic[T]):
    """A cursor-paginated list response."""

    data: list[T] = field(default_factory=list)
    next_cursor: Optional[str] = None
    has_more: bool = False
    limit: int = 50
