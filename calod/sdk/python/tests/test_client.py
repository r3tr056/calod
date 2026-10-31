"""Client happy-path tests over httpx MockTransport (no live server)."""

import httpx

from calod import CalodClient, EffectConflict

EFFECT = {
    "id": "11111111-1111-1111-1111-111111111111",
    "scope": "order-123",
    "idempotency_key": "order-123:charge",
    "tool_name": "charge_card",
    "input_digest": "sha256:" + "0" * 64,
    "state": "in_progress",
    "fencing_token": 1,
    "created_at": "2026-10-14T00:00:00Z",
    "updated_at": "2026-10-14T00:00:00Z",
}


def make_client(handler) -> CalodClient:
    transport = httpx.MockTransport(handler)
    return CalodClient(
        "http://calod.test",
        "cld_testsecret",
        http_client=httpx.AsyncClient(
            transport=transport, base_url="http://calod.test"
        ),
    )


async def test_health_and_version():
    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.path == "/v1/health/ready":
            return httpx.Response(200, json={"status": "ok"})
        if request.url.path == "/v1/version":
            return httpx.Response(200, json={"version": "0.1.0"})
        return httpx.Response(404, json={})

    client = make_client(handler)
    assert await client.health_ready() is True
    assert (await client.version())["version"] == "0.1.0"


async def test_acquire_execute_then_commit():
    seen = []

    def handler(request: httpx.Request) -> httpx.Response:
        seen.append((request.method, request.url.path))
        assert request.headers.get("x-calod-key") == "cld_testsecret"
        if request.method == "POST" and request.url.path == "/v1/effects":
            assert request.headers.get("idempotency-key") == "order-123:charge"
            return httpx.Response(
                201,
                json={
                    "decision": "execute",
                    "effect": EFFECT,
                    "lease_token": "lease-1",
                },
            )
        if request.method == "PATCH":
            assert request.headers.get("if-match") == '"1"'
            return httpx.Response(
                200, json={**EFFECT, "state": "committed", "result": {"ok": True}}
            )
        return httpx.Response(404, json={})

    client = make_client(handler)
    decision = await client.acquire(
        scope="order-123",
        tool_name="charge_card",
        input={"amount_minor": 5000},
        idempotency_key="order-123:charge",
    )
    assert decision.decision == "execute"
    assert decision.lease_token == "lease-1"

    committed = await client.commit(
        decision.effect.id,
        lease_token="lease-1",
        fencing_token=1,
        result={"ok": True},
    )
    assert committed.state == "committed"
    assert committed.result == {"ok": True}
    assert seen[0][0] == "POST" and seen[1][0] == "PATCH"


async def test_acquire_conflict_raises_typed_error():
    def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(
            409,
            json={
                "code": "IDEMPOTENCY_CONFLICT",
                "detail": "same key, different input",
                "request_id": "req-1",
            },
        )

    client = make_client(handler)
    try:
        await client.acquire(
            scope="order-123",
            tool_name="charge_card",
            input={"amount_minor": 9999},
            idempotency_key="order-123:charge",
        )
        raise AssertionError("expected EffectConflict")
    except EffectConflict as exc:
        assert exc.status == 409
