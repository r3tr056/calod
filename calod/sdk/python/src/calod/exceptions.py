"""Typed exceptions raised by the Calod Python SDK.

Every HTTP error from the server is an RFC 7807 ``application/problem+json``
body; :class:`CalodError` carries its fields. The SDK maps the stable problem
``code`` values onto subclasses so callers can catch precisely:

- 409 ``IDEMPOTENCY_CONFLICT`` -> :class:`EffectConflict`
- 409 ``LEASE_LOST`` / 412 ``PRECONDITION_FAILED`` / 403 ``INSUFFICIENT_SCOPE``
  -> :class:`LeaseLost`
- a ``reconcile`` acquire decision (no HTTP error) -> :class:`OutcomeUnknown`
- everything else -> :class:`CalodError`
"""

from __future__ import annotations

from typing import Any, Optional


class CalodError(Exception):
    """Base error for the SDK.

    Carries the RFC 7807 problem details returned by the server. Instances
    raised without an HTTP response (e.g. ``OutcomeUnknown``) leave
    ``status``/``code`` as ``None``.
    """

    def __init__(
        self,
        message: str,
        *,
        status: Optional[int] = None,
        code: Optional[str] = None,
        request_id: Optional[str] = None,
        problem: Optional[dict[str, Any]] = None,
    ) -> None:
        super().__init__(message)
        self.status = status
        self.code = code
        self.request_id = request_id
        self.problem = problem or {}

    def __repr__(self) -> str:  # pragma: no cover - debugging aid
        return (
            f"{type(self).__name__}(status={self.status}, code={self.code}, "
            f"request_id={self.request_id}, msg={self.args[0]!r})"
        )


class EffectConflict(CalodError):
    """409 ``IDEMPOTENCY_CONFLICT``: the key is bound to different input.

    The same idempotency key was acquired with a different canonical input
    digest. This is almost always a caller bug (reusing a business key for a
    different operation) and is never retried by the SDK.
    """


class LeaseLost(CalodError):
    """The caller no longer owns the effect lease.

    Raised for 409 ``LEASE_LOST`` (a newer fencing token owns the effect),
    412 ``PRECONDITION_FAILED`` (stale ETag), and 403 ``INSUFFICIENT_SCOPE``
    (the key may not perform the transition). The effect must not be touched
    again with the old lease; re-acquire or surface the error.
    """


class EffectWaitTimeout(CalodError):
    """The ``wait`` polling budget expired before the effect resolved.

    Another worker held the lease for longer than ``wait_timeout`` seconds.
    The wrapped function was never invoked.
    """


class OutcomeUnknown(CalodError):
    """The effect is in ``outcome_unknown`` and requires reconciliation.

    Raised when the server answers ``reconcile`` on acquire: a previous
    attempt may have reached the downstream provider, so the SDK never
    auto-retries. Carries the effect in :attr:`effect` for inspection.
    Resolve it via :meth:`CalodClient.reconcile` (requires the
    ``effects:reconcile`` API-key scope).
    """

    def __init__(self, message: str, *, effect: Any = None) -> None:
        super().__init__(message)
        self.effect = effect
