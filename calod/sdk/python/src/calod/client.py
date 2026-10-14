"""Async Calod API client (httpx).

Implements the `/v1` effects contract from ``docs/openapi.yaml``:

- ``POST   /v1/effects``            -> :meth:`CalodClient.acquire`
- ``PATCH  /v1/effects/{id}``       -> ``start`` / ``commit`` / ``fail`` /
  ``mark_outcome_unknown`` / ``reconcile``
- ``GET    /v1/effects/{id}``       -> :meth:`CalodClient.get_effect`
- ``GET    /v1/effects``            -> :meth:`CalodClient.list_effects`
- ``GET    /v1/runs/{id}/events``   -> :meth:`CalodClient.list_run_events`
- ``POST   /v1/runs``               -> :meth:`CalodClient.create_run`
- ``GET    /v1/health/*``, ``/v1/version`` -> liveness/readiness/version

Auth is the ``X-Calod-Key`` header (OpenAPI ``ApiKeyAuth``). Worker-owned
transitions send the lease token, the fencing token, and an ``If-Match``
header (for effects the ETag is the quoted fencing token).
"""

from __future__ import annotations

import json
from typing import Any, Callable, Optional

import httpx

from .canonical import canonical_dumps
from .exceptions import CalodError, EffectConflict, LeaseLost
from .models import Effect, EffectDecision, Event, Page, Run

DEFAULT_BASE_URL = "http://127.0.0.1:7379"

_CONFLICT_CODES = {
    "IDEMPOTENCY_CONFLICT": EffectConflict,
    "LEASE_LOST": LeaseLost,
}


def _raise_for_problem(response: httpx.Response) -> None:
    """Map a non-2xx response to a typed SDK error."""
    problem: dict[str, Any] = {}
    try:
        body = response.json()
        if isinstance(body, dict):
            problem = body
    except (json.JSONDecodeError, ValueError):
        pass
    status = response.status_code
    code = problem.get("code")
    detail = problem.get("detail") or response.text or f"HTTP {status}"
    request_id = problem.get("request_id") or response.headers.get("x-request-id")
    kwargs = dict(status=status, code=code, request_id=request_id, problem=problem)

    if status == 409 and code in _CONFLICT_CODES:
        raise _CONFLICT_CODES[code](detail, **kwargs)
    if status in (403, 412):
        raise LeaseLost(detail, **kwargs)
    raise CalodError(detail, **kwargs)


def _normalize_base_url(base_url: str) -> str:
    base = base_url.rstrip("/")
    if base.endswith("/v1"):
        base = base[: -len("/v1")]
    return base


class CalodClient:
    """Async client for the Calod action-ledger API.

    Parameters
    ----------
    base_url:
        Server root (``http://host:port``); a trailing ``/v1`` is accepted and
        stripped. Defaults to the local dev server port 7379.
    api_key:
        Project API key (``cld_...``), sent as ``X-Calod-Key``.
    timeout:
        Per-request timeout in seconds.
    http_client:
        Optional pre-configured ``httpx.AsyncClient`` (the caller owns its
        lifecycle; ``aclose`` will not close it).
    """

    def __init__(
        self,
        base_url: str = DEFAULT_BASE_URL,
        api_key: str = "",
        *,
        timeout: float = 30.0,
        http_client: Optional[httpx.AsyncClient] = None,
    ) -> None:
        self._base = _normalize_base_url(base_url)
        self._api_key = api_key
        self._owns_client = http_client is None
        self._http = http_client or httpx.AsyncClient(timeout=timeout)

    async def __aenter__(self) -> "CalodClient":
        return self

    async def __aexit__(self, *exc: Any) -> None:
        await self.aclose()

    async def aclose(self) -> None:
        if self._owns_client:
            await self._http.aclose()

    # -- low level -----------------------------------------------------------

    async def _request(
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
        response = await self._http.request(
            method,
            f"{self._base}{path}",
            content=(
                canonical_dumps(json_body).encode("utf-8") if json_body is not None else None
            ),
            headers=(
                {**hdrs, "Content-Type": "application/json"}
                if json_body is not None
                else hdrs
            ),
            params={k: v for k, v in (params or {}).items() if v is not None},
        )
        if response.status_code >= 400:
            _raise_for_problem(response)
        return response

    # -- system ---------------------------------------------------------------

    async def health_live(self) -> bool:
        """True when the process is alive."""
        response = await self._request("GET", "/v1/health/live")
        return response.json().get("status") == "ok"

    async def health_ready(self) -> bool:
        """True when the service can accept durable writes."""
        response = await self._request("GET", "/v1/health/ready")
        return response.json().get("status") == "ok"

    async def version(self) -> dict[str, Any]:
        """API/server version info."""
        return (await self._request("GET", "/v1/version")).json()

    # -- runs -----------------------------------------------------------------

    async def create_run(
        self,
        *,
        external_id: str,
        agent_name: str,
        metadata: Any = None,
        idempotency_key: Optional[str] = None,
    ) -> Run:
        """Create an agent run. ``idempotency_key`` defaults to a random key."""
        key = idempotency_key or _random_key("run")
        response = await self._request(
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

    async def list_run_events(
        self,
        run_id: str,
        *,
        type: Optional[str] = None,
        after_sequence: Optional[int] = None,
        cursor: Optional[str] = None,
        limit: Optional[int] = None,
    ) -> Page[Event]:
        """List the append-only events of a run, oldest first."""
        response = await self._request(
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

    # -- effects ---------------------------------------------------------------

    async def acquire(
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
        """Acquire or inspect an idempotent effect.

        Returns an :class:`EffectDecision` — ``execute`` (201, carries a lease
        token), ``replay`` / ``reconcile`` (200), or ``wait`` (202, with
        ``retry_after`` from the ``Retry-After`` header). Raises
        :class:`EffectConflict` on a 409 input-digest conflict.
        """
        body: dict[str, Any] = {"scope": scope, "tool_name": tool_name, "input": input}
        if run_id is not None:
            body["run_id"] = run_id
        if lease_duration_seconds is not None:
            body["lease_duration_seconds"] = lease_duration_seconds
        if downstream_idempotency_supported is not None:
            body["downstream_idempotency_supported"] = downstream_idempotency_supported
        response = await self._request(
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

    async def get_effect(self, effect_id: str) -> Effect:
        """Fetch an effect and its current receipt."""
        response = await self._request("GET", f"/v1/effects/{effect_id}")
        return Effect.from_dict(response.json())

    async def list_effects(
        self,
        *,
        run_id: Optional[str] = None,
        state: Optional[str] = None,
        tool_name: Optional[str] = None,
        created_after: Optional[str] = None,
        cursor: Optional[str] = None,
        limit: Optional[int] = None,
    ) -> Page[Effect]:
        """List effects, newest first."""
        response = await self._request(
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

    async def start(
        self,
        effect_id: str,
        *,
        lease_token: str,
        fencing_token: int,
    ) -> Effect:
        """Mark a claimed effect ``in_progress`` before invoking the tool."""
        return await self._transition(
            effect_id,
            {"transition": "start"},
            lease_token=lease_token,
            fencing_token=fencing_token,
        )

    async def commit(
        self,
        effect_id: str,
        *,
        lease_token: str,
        fencing_token: int,
        result: Any,
        external_receipt: Optional[dict[str, Any]] = None,
    ) -> Effect:
        """Commit the tool result. The result is the replayed receipt."""
        body: dict[str, Any] = {"transition": "commit", "result": result}
        if external_receipt is not None:
            body["external_receipt"] = external_receipt
        return await self._transition(
            effect_id, body, lease_token=lease_token, fencing_token=fencing_token
        )

    async def fail(
        self,
        effect_id: str,
        *,
        lease_token: str,
        fencing_token: int,
        kind: str,
        message: str,
        retryable: bool = False,
    ) -> Effect:
        """Record a failed invocation.

        ``retryable=True`` -> ``fail_retryable`` (the effect can be reacquired
        safely); ``retryable=False`` -> ``fail_terminal``.
        """
        return await self._transition(
            effect_id,
            {
                "transition": "fail_retryable" if retryable else "fail_terminal",
                "failure": {"kind": kind, "message": message, "retryable": retryable},
            },
            lease_token=lease_token,
            fencing_token=fencing_token,
        )

    async def mark_outcome_unknown(
        self,
        effect_id: str,
        *,
        lease_token: str,
        fencing_token: int,
        kind: str,
        message: str,
    ) -> Effect:
        """Record that the tool request may have reached the provider.

        Use after a timeout/connection failure where the request might have
        been sent. The effect blocks further execution until reconciled.
        """
        return await self._transition(
            effect_id,
            {
                "transition": "mark_outcome_unknown",
                "failure": {"kind": kind, "message": message, "retryable": False},
            },
            lease_token=lease_token,
            fencing_token=fencing_token,
        )

    async def reconcile(
        self,
        effect_id: str,
        *,
        fencing_token: int,
        outcome: str,
        result: Any = None,
        failure: Optional[dict[str, Any]] = None,
        external_receipt: Optional[dict[str, Any]] = None,
    ) -> Effect:
        """Manually reconcile an ``outcome_unknown`` effect.

        ``outcome`` is ``"committed"`` (the tool did fire; supply ``result``)
        or ``"failed"`` (it provably did not; supply ``failure``). Requires
        the ``effects:reconcile`` API-key scope. Never called automatically.
        """
        if outcome not in ("committed", "failed"):
            raise ValueError("outcome must be 'committed' or 'failed'")
        body: dict[str, Any] = {"transition": f"reconcile_{outcome}"}
        if result is not None:
            body["result"] = result
        if failure is not None:
            body["failure"] = failure
        if external_receipt is not None:
            body["external_receipt"] = external_receipt
        return await self._transition(effect_id, body, fencing_token=fencing_token)

    async def _transition(
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
        response = await self._request(
            "PATCH",
            f"/v1/effects/{effect_id}",
            json_body=payload,
            # For effects the strong ETag is the quoted fencing token.
            headers={"If-Match": f'"{fencing_token}"'},
        )
        return Effect.from_dict(response.json())


def _parse_page(payload: dict[str, Any], parse: Callable[[dict[str, Any]], Any]) -> Page[Any]:
    pagination = payload.get("pagination") or {}
    return Page(
        data=[parse(item) for item in payload.get("data") or []],
        next_cursor=pagination.get("next_cursor"),
        has_more=bool(pagination.get("has_more", False)),
        limit=int(pagination.get("limit", 50)),
    )


def _random_key(prefix: str) -> str:
    import secrets

    return f"{prefix}-{secrets.token_hex(16)}"
