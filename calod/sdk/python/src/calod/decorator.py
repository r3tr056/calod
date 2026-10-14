"""The ``@calod.effect`` decorator: durable, idempotent tool execution.

The wrapped function is invoked **at most once per business operation**,
coordinated through the Calod ledger:

1. ``acquire`` — one caller per (scope, idempotency key, input digest) gets
   ``execute`` with a lease + fencing token.
2. ``execute`` -> ``start``, invoke the function, then ``commit`` the
   JSON-normalized result. The committed result is the receipt.
3. ``replay`` -> the recorded receipt is returned without invoking the
   function again.
4. ``wait`` -> another worker owns the lease; poll with exponential backoff
   (honoring ``Retry-After`` / ``poll_after_ms``) up to ``wait_timeout``.
5. ``reconcile`` -> a previous attempt's outcome is unknown; raise
   :class:`~calod.OutcomeUnknown`. Never auto-retried.

Exception classification when the wrapped function raises (configurable via
``unknown_exceptions`` / ``retryable_exceptions``):

============================== =============================================
Exception (default classes)    Recorded as
============================== =============================================
``httpx.ReadTimeout``,         ``outcome_unknown`` — the request may have
``httpx.WriteTimeout``,        reached the provider. Raises
``httpx.ReadError``,           :class:`~calod.OutcomeUnknown` (never retried).
``httpx.WriteError``,
``TimeoutError``
------------------------------ ---------------------------------------------
``httpx.ConnectError``,        ``failed_retryable`` — the request provably
``httpx.ConnectTimeout``,      never left the process; safe to reacquire.
``httpx.PoolTimeout``,
``ConnectionError``
------------------------------ ---------------------------------------------
anything else (validation,     ``failed_terminal`` — the tool rejected the
provider 4xx, bugs)            input or the caller code is broken.
============================== =============================================
"""

from __future__ import annotations

import asyncio
import functools
import hashlib
import inspect
import json
import os
import re
from types import SimpleNamespace
from typing import Any, Awaitable, Callable, Optional, TypeVar, Union

import httpx

from . import context
from .canonical import canonical_bytes, canonical_dumps
from .client import CalodClient
from .exceptions import CalodError, EffectWaitTimeout, OutcomeUnknown

F = TypeVar("F", bound=Callable[..., Awaitable[Any]])

#: Exceptions meaning "the request may have reached the provider".
DEFAULT_UNKNOWN_EXCEPTIONS: tuple[type[BaseException], ...] = (
    httpx.ReadTimeout,
    httpx.WriteTimeout,
    httpx.ReadError,
    httpx.WriteError,
    TimeoutError,  # includes asyncio.TimeoutError on 3.11+
)

#: Exceptions meaning "the request provably never left the process".
DEFAULT_RETRYABLE_EXCEPTIONS: tuple[type[BaseException], ...] = (
    httpx.ConnectError,
    httpx.ConnectTimeout,
    httpx.PoolTimeout,
    ConnectionError,
)

_KEY_PATTERN = re.compile(r"^[A-Za-z0-9._:-]{8,255}$")


class _Settings:
    """Process-wide defaults for decorators that don't pin a client."""

    client: Any = None
    base_url: Optional[str] = None
    api_key: Optional[str] = None


def configure(
    *,
    client: Any = None,
    base_url: Optional[str] = None,
    api_key: Optional[str] = None,
) -> None:
    """Set the process-wide default client (or connection settings).

    Decorators without an explicit ``client=`` resolve, in order: the
    decorator argument, this configuration, then the ``CALOD_BASE_URL`` /
    ``CALOD_API_KEY`` environment variables.
    """
    _Settings.client = client
    _Settings.base_url = base_url
    _Settings.api_key = api_key


def _resolve_async_client(given: Optional[CalodClient]) -> CalodClient:
    if given is not None:
        return given
    if isinstance(_Settings.client, CalodClient):
        return _Settings.client
    base_url = _Settings.base_url or os.environ.get("CALOD_BASE_URL")
    api_key = _Settings.api_key or os.environ.get("CALOD_API_KEY")
    if base_url and api_key:
        return CalodClient(base_url, api_key)
    raise RuntimeError(
        "no Calod client configured: pass client= to @calod.effect, call "
        "calod.configure(...), or set CALOD_BASE_URL + CALOD_API_KEY"
    )


def derive_idempotency_key(tool_name: str, scope: str, input_payload: Any) -> str:
    """Default key: ``<tool>:<scope>:<sha256(canonical input)[:16]>``."""
    digest = hashlib.sha256(canonical_bytes(input_payload)).hexdigest()[:16]
    return f"{tool_name}:{scope}:{digest}"


def _validate_key(key: str) -> None:
    if not _KEY_PATTERN.match(key):
        raise ValueError(
            f"idempotency key {key!r} does not match { _KEY_PATTERN.pattern }: "
            "8-255 chars of [A-Za-z0-9._:-]"
        )


def effect(
    fn: Optional[F] = None,
    *,
    client: Optional[CalodClient] = None,
    scope: Union[str, Callable[[Any], str], None] = None,
    tool_name: Optional[str] = None,
    key: Union[str, Callable[[Any], str], None] = None,
    input: Optional[Callable[[Any], Any]] = None,
    run_id: Union[str, Callable[[Any], Optional[str]], None] = None,
    lease_duration_seconds: int = 30,
    downstream_idempotency_supported: bool = False,
    wait_timeout: float = 60.0,
    poll_interval: float = 0.25,
    max_poll_interval: float = 5.0,
    unknown_exceptions: tuple[type[BaseException], ...] = DEFAULT_UNKNOWN_EXCEPTIONS,
    retryable_exceptions: tuple[type[BaseException], ...] = DEFAULT_RETRYABLE_EXCEPTIONS,
) -> Any:
    """Decorate an async function as a durable, idempotent effect.

    Parameters
    ----------
    scope:
        Tenant/operation scope string, or ``lambda ctx: ...`` receiving the
        bound arguments as attributes (``ctx.order_id``). Default ``"default"``.
    tool_name:
        Ledger tool name; defaults to ``<module>.<qualname>``.
    key:
        Explicit idempotency key or ``lambda ctx: ...``. Default derives a
        stable key from tool name, scope, and the canonical input digest.
    input:
        ``lambda ctx: ...`` building the recorded input payload; defaults to
        the bound call arguments (must be JSON-serializable).
    wait_timeout:
        Total seconds to poll on ``wait`` decisions before raising
        :class:`~calod.EffectWaitTimeout`.
    unknown_exceptions / retryable_exceptions:
        Exception classification (see module docstring). ``unknown`` is
        checked first; anything unmatched is ``failed_terminal``.
    """

    def decorate(func: F) -> F:
        if not inspect.iscoroutinefunction(func):
            raise TypeError(
                "@calod.effect wraps async functions; use calod.sync.effect "
                "for synchronous functions"
            )
        signature = inspect.signature(func)

        @functools.wraps(func)
        async def wrapper(*args: Any, **kwargs: Any) -> Any:
            bound = signature.bind(*args, **kwargs)
            bound.apply_defaults()
            arguments = {
                k: v for k, v in bound.arguments.items() if k not in ("self", "cls")
            }
            ctx = SimpleNamespace(**arguments)

            the_client = _resolve_async_client(client)
            scope_value = scope(ctx) if callable(scope) else (scope or "default")
            tool = tool_name or f"{func.__module__}.{func.__qualname__}"
            payload = input(ctx) if input is not None else arguments
            run = run_id(ctx) if callable(run_id) else run_id
            idem_key = key(ctx) if callable(key) else key
            if idem_key is None:
                idem_key = derive_idempotency_key(tool, scope_value, payload)
            _validate_key(idem_key)

            acquired, lease_token = await _acquire_until_actionable(
                the_client,
                scope=scope_value,
                tool_name=tool,
                payload=payload,
                idem_key=idem_key,
                run_id=run,
                lease_duration_seconds=lease_duration_seconds,
                downstream_idempotency_supported=downstream_idempotency_supported,
                wait_timeout=wait_timeout,
                poll_interval=poll_interval,
                max_poll_interval=max_poll_interval,
            )
            if isinstance(acquired, _Replayed):
                return acquired.result
            the_effect = acquired

            tokens = context._set(idem_key, the_effect.id)
            try:
                await the_client.start(
                    the_effect.id,
                    lease_token=lease_token,
                    fencing_token=the_effect.fencing_token,
                )
                try:
                    result = await func(*args, **kwargs)
                except BaseException as exc:
                    classification = await _record_failure(
                        the_client,
                        the_effect,
                        lease_token,
                        exc,
                        unknown_exceptions=unknown_exceptions,
                        retryable_exceptions=retryable_exceptions,
                    )
                    if classification == "unknown":
                        # Never surface this as an ordinary failure: the
                        # provider may have applied it, so blind retry is
                        # unsafe.
                        raise OutcomeUnknown(
                            f"effect {the_effect.id} recorded outcome_unknown "
                            f"after {type(exc).__name__}: {exc}",
                            effect=the_effect,
                        ) from exc
                    raise
                try:
                    # Normalize through canonical JSON so the executing caller
                    # and every future replaying caller observe the identical
                    # value.
                    normalized = json.loads(canonical_dumps(result))
                except (TypeError, ValueError) as exc:
                    # The tool fired but its result cannot be recorded — the
                    # side effect exists without a receipt: outcome unknown.
                    try:
                        await the_client.mark_outcome_unknown(
                            the_effect.id,
                            lease_token=lease_token,
                            fencing_token=the_effect.fencing_token,
                            kind="result_not_serializable",
                            message=f"tool result is not JSON-serializable: {exc}"[:2000],
                        )
                    except CalodError:
                        pass
                    raise OutcomeUnknown(
                        f"effect {the_effect.id} fired but its result is not "
                        f"JSON-serializable; recorded outcome_unknown",
                        effect=the_effect,
                    ) from exc
            finally:
                context._reset(tokens)

            committed = await the_client.commit(
                the_effect.id,
                lease_token=lease_token,
                fencing_token=the_effect.fencing_token,
                result=normalized,
            )
            return committed.result

        return wrapper  # type: ignore[return-value]

    if fn is not None:
        return decorate(fn)
    return decorate


async def _acquire_until_actionable(
    client: CalodClient,
    *,
    scope: str,
    tool_name: str,
    payload: Any,
    idem_key: str,
    run_id: Optional[str],
    lease_duration_seconds: int,
    downstream_idempotency_supported: bool,
    wait_timeout: float,
    poll_interval: float,
    max_poll_interval: float,
) -> tuple[Any, str]:
    """Acquire; poll through `wait`; short-circuit `replay`/`reconcile`."""
    loop = asyncio.get_running_loop()
    deadline = loop.time() + wait_timeout
    delay = poll_interval
    while True:
        decision = await client.acquire(
            scope=scope,
            tool_name=tool_name,
            input=payload,
            idempotency_key=idem_key,
            run_id=run_id,
            lease_duration_seconds=lease_duration_seconds,
            downstream_idempotency_supported=downstream_idempotency_supported,
        )
        if decision.decision != "wait":
            break
        remaining = deadline - loop.time()
        if remaining <= 0:
            raise EffectWaitTimeout(
                f"effect {decision.effect.id} still leased after "
                f"{wait_timeout:.1f}s of waiting",
                status=None,
                code="WAIT_TIMEOUT",
            )
        hint = decision.retry_after
        if hint is None and decision.poll_after_ms:
            hint = decision.poll_after_ms / 1000.0
        await asyncio.sleep(min(max(hint or 0.0, delay), remaining))
        delay = min(delay * 2, max_poll_interval)

    if decision.decision == "replay":
        return _Replayed(decision.effect.result), ""
    if decision.decision == "reconcile":
        raise OutcomeUnknown(
            f"effect {decision.effect.id} ({tool_name}) is outcome_unknown: "
            "a previous attempt may have reached the provider; reconcile it "
            "explicitly instead of retrying",
            effect=decision.effect,
        )
    if decision.decision == "execute":
        if not decision.lease_token:
            raise CalodError("server returned execute without a lease token")
        return decision.effect, decision.lease_token
    raise CalodError(f"unknown acquire decision {decision.decision!r}")


class _Replayed:
    """Sentinel carrying a replayed receipt out of the acquire loop."""

    __slots__ = ("result",)

    def __init__(self, result: Any) -> None:
        self.result = result


async def _record_failure(
    client: CalodClient,
    effect: Any,
    lease_token: str,
    exc: BaseException,
    *,
    unknown_exceptions: tuple[type[BaseException], ...],
    retryable_exceptions: tuple[type[BaseException], ...],
) -> str:
    """Classify the tool exception and record the terminal ledger state.

    Returns the classification: ``"unknown"``, ``"retryable"`` or
    ``"terminal"``. Recording is best-effort; on failure the lease expiry
    recovers the effect as ``outcome_unknown`` server-side anyway.
    """
    kind = type(exc).__name__.lower()
    message = (str(exc) or kind)[:2000]
    if isinstance(exc, unknown_exceptions):
        classification = "unknown"
    elif isinstance(exc, retryable_exceptions):
        classification = "retryable"
    else:
        classification = "terminal"
    try:
        if classification == "unknown":
            await client.mark_outcome_unknown(
                effect.id,
                lease_token=lease_token,
                fencing_token=effect.fencing_token,
                kind=kind,
                message=message,
            )
        else:
            await client.fail(
                effect.id,
                lease_token=lease_token,
                fencing_token=effect.fencing_token,
                kind=kind,
                message=message,
                retryable=(classification == "retryable"),
            )
    except CalodError:
        # The original exception is the one the caller must see; the lease
        # expiry will recover the effect as outcome_unknown server-side.
        pass
    return classification
