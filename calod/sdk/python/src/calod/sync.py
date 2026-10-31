"""Synchronous twin of the async SDK: :class:`SyncCalodClient` + ``effect``.

Same decision loop and exception classification as :mod:`calod.decorator`,
built on ``httpx.Client`` and ``time.sleep``. The async API is the primary
one; this module exists for scripts and frameworks without an event loop.
"""

from __future__ import annotations

import functools
import inspect
import json
import time
from types import SimpleNamespace
from typing import Any, Callable, Optional, TypeVar, Union

import httpx

from . import context
from .canonical import canonical_dumps
from .client import (
    DEFAULT_BASE_URL,
    RETRYABLE_STATUSES,
    _normalize_base_url,
    _parse_page,
    _raise_for_problem,
    _random_key,
)
from .decorator import (
    DEFAULT_RETRYABLE_EXCEPTIONS,
    DEFAULT_UNKNOWN_EXCEPTIONS,
    _Settings,
    _validate_key,
    derive_idempotency_key,
)
from .exceptions import CalodError, EffectWaitTimeout, OutcomeUnknown
from .models import (
    Effect,
    EffectDecision,
    Event,
    Page,
    RetentionReport,
    Run,
    RunExport,
)

F = TypeVar("F", bound=Callable[..., Any])


class SyncCalodClient:
    """Synchronous Calod client. See :class:`calod.CalodClient`."""

    def __init__(
        self,
        base_url: str = DEFAULT_BASE_URL,
        api_key: str = "",
        *,
        timeout: float = 30.0,
        retries: int = 2,
        retry_backoff_seconds: float = 0.2,
        http_client: Optional[httpx.Client] = None,
    ) -> None:
        self._base = _normalize_base_url(base_url)
        self._api_key = api_key
        self._timeout = timeout
        self._retries = max(0, retries)
        self._retry_backoff = retry_backoff_seconds
        self._owns_client = http_client is None
        self._http = http_client or httpx.Client(timeout=timeout)

    def __enter__(self) -> "SyncCalodClient":
        return self

    def __exit__(self, *exc: Any) -> None:
        self.close()

    def close(self) -> None:
        if self._owns_client:
            self._http.close()

    def _request(
        self,
        method: str,
        path: str,
        *,
        json_body: Any = None,
        headers: Optional[dict[str, str]] = None,
        params: Optional[dict[str, Any]] = None,
    ) -> httpx.Response:
        hdrs = {"X-Calod-Key": self._api_key}
        if headers:
            hdrs.update(headers)
        # Same retry policy as the async client (see CalodClient).
        idempotent = method == "GET" or (method == "POST" and path in ("/v1/effects", "/v1/runs"))
        attempt = 0
        while True:
            try:
                response = self._http.request(
                    method,
                    f"{self._base}{path}",
                    content=(
                        canonical_dumps(json_body).encode("utf-8")
                        if json_body is not None
                        else None
                    ),
                    headers=(
                        {**hdrs, "Content-Type": "application/json"}
                        if json_body is not None
                        else hdrs
                    ),
                    params={k: v for k, v in (params or {}).items() if v is not None},
                    timeout=self._timeout,
                )
            except httpx.TransportError:
                if not (idempotent and attempt < self._retries):
                    raise
                time.sleep(self._retry_backoff * (2**attempt))
                attempt += 1
                continue
            if (
                response.status_code in RETRYABLE_STATUSES
                and idempotent
                and attempt < self._retries
            ):
                time.sleep(self._retry_backoff * (2**attempt))
                attempt += 1
                continue
            break
        if response.status_code >= 400:
            _raise_for_problem(response)
        return response

    # -- system ---------------------------------------------------------------

    def health_live(self) -> bool:
        return self._request("GET", "/v1/health/live").json().get("status") == "ok"

    def health_ready(self) -> bool:
        return self._request("GET", "/v1/health/ready").json().get("status") == "ok"

    def version(self) -> dict[str, Any]:
        return self._request("GET", "/v1/version").json()

    def metrics(self) -> str:
        return self._request("GET", "/metrics").text

    # -- runs -----------------------------------------------------------------

    def create_run(
        self,
        *,
        external_id: str,
        agent_name: str,
        metadata: Any = None,
        idempotency_key: Optional[str] = None,
    ) -> Run:
        key = idempotency_key or _random_key("run")
        response = self._request(
            "POST",
            "/v1/runs",
            json_body={
                "external_id": external_id,
                "agent_name": agent_name,
                **({"metadata": metadata} if metadata is not None else {}),
            },
            headers={"Idempotency-Key": key},
        )
        return Run.from_dict(response.json())

    def list_run_events(
        self,
        run_id: str,
        *,
        type: Optional[str] = None,
        after_sequence: Optional[int] = None,
        cursor: Optional[str] = None,
        limit: Optional[int] = None,
    ) -> Page[Event]:
        response = self._request(
            "GET",
            f"/v1/runs/{run_id}/events",
            params={
                "type": type,
                "after_sequence": after_sequence,
                "cursor": cursor,
                "limit": limit,
            },
        )
        return _parse_page(response.json(), Event.from_dict)

    def export_run(self, run_id: str, *, format: str = "json"):
        """Synchronous variant of `CalodClient.export_run`."""
        if format not in ("json", "csv"):
            raise ValueError("format must be 'json' or 'csv'")
        response = self._request(
            "GET", f"/v1/runs/{run_id}/export", params={"format": format}
        )
        if format == "csv":
            return response.text
        return RunExport.from_dict(response.json())

    def apply_retention(
        self,
        *,
        max_age_days: Optional[int] = None,
        compact: bool = True,
    ) -> RetentionReport:
        """Synchronous variant of `CalodClient.apply_retention`."""
        body: dict[str, Any] = {"compact": compact}
        if max_age_days is not None:
            body["max_age_days"] = max_age_days
        response = self._request("POST", "/v1/admin/retention", json_body=body)
        return RetentionReport.from_dict(response.json())

    # -- effects ---------------------------------------------------------------

    def acquire(
        self,
        *,
        scope: str,
        tool_name: str,
        input: Any,
        idempotency_key: str,
        run_id: Optional[str] = None,
        lease_duration_seconds: Optional[int] = None,
        downstream_idempotency_supported: Optional[bool] = None,
    ) -> EffectDecision:
        body: dict[str, Any] = {"scope": scope, "tool_name": tool_name, "input": input}
        if run_id is not None:
            body["run_id"] = run_id
        if lease_duration_seconds is not None:
            body["lease_duration_seconds"] = lease_duration_seconds
        if downstream_idempotency_supported is not None:
            body["downstream_idempotency_supported"] = downstream_idempotency_supported
        response = self._request(
            "POST", "/v1/effects", json_body=body,
            headers={"Idempotency-Key": idempotency_key},
        )
        payload = response.json()
        retry_after = response.headers.get("retry-after")
        return EffectDecision(
            decision=payload["decision"],
            effect=Effect.from_dict(payload["effect"]),
            lease_token=payload.get("lease_token"),
            poll_after_ms=payload.get("poll_after_ms"),
            retry_after=float(retry_after) if retry_after is not None else None,
        )

    def get_effect(self, effect_id: str) -> Effect:
        return Effect.from_dict(self._request("GET", f"/v1/effects/{effect_id}").json())

    def list_effects(
        self,
        *,
        run_id: Optional[str] = None,
        state: Optional[str] = None,
        tool_name: Optional[str] = None,
        created_after: Optional[str] = None,
        cursor: Optional[str] = None,
        limit: Optional[int] = None,
    ) -> Page[Effect]:
        response = self._request(
            "GET",
            "/v1/effects",
            params={
                "run_id": run_id,
                "state": state,
                "tool_name": tool_name,
                "created_after": created_after,
                "cursor": cursor,
                "limit": limit,
            },
        )
        return _parse_page(response.json(), Effect.from_dict)

    def start(self, effect_id: str, *, lease_token: str, fencing_token: int) -> Effect:
        return self._transition(
            effect_id, {"transition": "start"},
            lease_token=lease_token, fencing_token=fencing_token,
        )

    def commit(
        self,
        effect_id: str,
        *,
        lease_token: str,
        fencing_token: int,
        result: Any,
        external_receipt: Optional[dict[str, Any]] = None,
    ) -> Effect:
        body: dict[str, Any] = {"transition": "commit", "result": result}
        if external_receipt is not None:
            body["external_receipt"] = external_receipt
        return self._transition(
            effect_id, body, lease_token=lease_token, fencing_token=fencing_token
        )

    def fail(
        self,
        effect_id: str,
        *,
        lease_token: str,
        fencing_token: int,
        kind: str,
        message: str,
        retryable: bool = False,
    ) -> Effect:
        return self._transition(
            effect_id,
            {
                "transition": "fail_retryable" if retryable else "fail_terminal",
                "failure": {"kind": kind, "message": message, "retryable": retryable},
            },
            lease_token=lease_token,
            fencing_token=fencing_token,
        )

    def mark_outcome_unknown(
        self,
        effect_id: str,
        *,
        lease_token: str,
        fencing_token: int,
        kind: str,
        message: str,
    ) -> Effect:
        return self._transition(
            effect_id,
            {
                "transition": "mark_outcome_unknown",
                "failure": {"kind": kind, "message": message, "retryable": False},
            },
            lease_token=lease_token,
            fencing_token=fencing_token,
        )

    def reconcile(
        self,
        effect_id: str,
        *,
        fencing_token: int,
        outcome: str,
        result: Any = None,
        failure: Optional[dict[str, Any]] = None,
        external_receipt: Optional[dict[str, Any]] = None,
    ) -> Effect:
        if outcome not in ("committed", "failed"):
            raise ValueError("outcome must be 'committed' or 'failed'")
        body: dict[str, Any] = {"transition": f"reconcile_{outcome}"}
        if result is not None:
            body["result"] = result
        if failure is not None:
            body["failure"] = failure
        if external_receipt is not None:
            body["external_receipt"] = external_receipt
        return self._transition(effect_id, body, fencing_token=fencing_token)

    def _transition(
        self,
        effect_id: str,
        body: dict[str, Any],
        *,
        fencing_token: int,
        lease_token: Optional[str] = None,
    ) -> Effect:
        payload = {**body, "fencing_token": fencing_token}
        if lease_token is not None:
            payload["lease_token"] = lease_token
        response = self._request(
            "PATCH",
            f"/v1/effects/{effect_id}",
            json_body=payload,
            headers={"If-Match": f'"{fencing_token}"'},
        )
        return Effect.from_dict(response.json())


def _resolve_sync_client(given: Optional[SyncCalodClient]) -> SyncCalodClient:
    import os

    if given is not None:
        return given
    if isinstance(_Settings.client, SyncCalodClient):
        return _Settings.client
    base_url = _Settings.base_url or os.environ.get("CALOD_BASE_URL")
    api_key = _Settings.api_key or os.environ.get("CALOD_API_KEY")
    if base_url and api_key:
        return SyncCalodClient(base_url, api_key)
    raise RuntimeError(
        "no Calod client configured: pass client= to @calod.sync.effect, call "
        "calod.configure(...), or set CALOD_BASE_URL + CALOD_API_KEY"
    )


def effect(
    fn: Optional[F] = None,
    *,
    client: Optional[SyncCalodClient] = None,
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
    """Synchronous variant of :func:`calod.effect`. Same semantics."""

    def decorate(func: F) -> F:
        if inspect.iscoroutinefunction(func):
            raise TypeError(
                "@calod.sync.effect wraps sync functions; use calod.effect "
                "for coroutines"
            )
        signature = inspect.signature(func)

        @functools.wraps(func)
        def wrapper(*args: Any, **kwargs: Any) -> Any:
            bound = signature.bind(*args, **kwargs)
            bound.apply_defaults()
            arguments = {
                k: v for k, v in bound.arguments.items() if k not in ("self", "cls")
            }
            ctx = SimpleNamespace(**arguments)

            the_client = _resolve_sync_client(client)
            scope_value = scope(ctx) if callable(scope) else (scope or "default")
            tool = tool_name or f"{func.__module__}.{func.__qualname__}"
            payload = input(ctx) if input is not None else arguments
            run = run_id(ctx) if callable(run_id) else run_id
            idem_key = key(ctx) if callable(key) else key
            if idem_key is None:
                idem_key = derive_idempotency_key(tool, scope_value, payload)
            _validate_key(idem_key)

            deadline = time.monotonic() + wait_timeout
            delay = poll_interval
            while True:
                decision = the_client.acquire(
                    scope=scope_value,
                    tool_name=tool,
                    input=payload,
                    idempotency_key=idem_key,
                    run_id=run,
                    lease_duration_seconds=lease_duration_seconds,
                    downstream_idempotency_supported=downstream_idempotency_supported,
                )
                if decision.decision != "wait":
                    break
                remaining = deadline - time.monotonic()
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
                time.sleep(min(max(hint or 0.0, delay), remaining))
                delay = min(delay * 2, max_poll_interval)

            if decision.decision == "replay":
                return decision.effect.result
            if decision.decision == "reconcile":
                raise OutcomeUnknown(
                    f"effect {decision.effect.id} ({tool}) is outcome_unknown: "
                    "a previous attempt may have reached the provider; "
                    "reconcile it explicitly instead of retrying",
                    effect=decision.effect,
                )
            if decision.decision != "execute":
                raise CalodError(f"unknown acquire decision {decision.decision!r}")
            the_effect = decision.effect
            lease_token = decision.lease_token
            if not lease_token:
                raise CalodError("server returned execute without a lease token")

            tokens = context._set(idem_key, the_effect.id)
            try:
                the_client.start(
                    the_effect.id,
                    lease_token=lease_token,
                    fencing_token=the_effect.fencing_token,
                )
                try:
                    result = func(*args, **kwargs)
                except BaseException as exc:
                    classification = _record_failure_sync(
                        the_client,
                        the_effect,
                        lease_token,
                        exc,
                        unknown_exceptions=unknown_exceptions,
                        retryable_exceptions=retryable_exceptions,
                    )
                    if classification == "unknown":
                        raise OutcomeUnknown(
                            f"effect {the_effect.id} recorded outcome_unknown "
                            f"after {type(exc).__name__}: {exc}",
                            effect=the_effect,
                        ) from exc
                    raise
                try:
                    normalized = json.loads(canonical_dumps(result))
                except (TypeError, ValueError) as exc:
                    try:
                        the_client.mark_outcome_unknown(
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

            committed = the_client.commit(
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


def _record_failure_sync(
    client: SyncCalodClient,
    effect: Any,
    lease_token: str,
    exc: BaseException,
    *,
    unknown_exceptions: tuple[type[BaseException], ...],
    retryable_exceptions: tuple[type[BaseException], ...],
) -> str:
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
            client.mark_outcome_unknown(
                effect.id,
                lease_token=lease_token,
                fencing_token=effect.fencing_token,
                kind=kind,
                message=message,
            )
        else:
            client.fail(
                effect.id,
                lease_token=lease_token,
                fencing_token=effect.fencing_token,
                kind=kind,
                message=message,
                retryable=(classification == "retryable"),
            )
    except CalodError:
        pass
    return classification
