"""Retry/timeout semantics: idempotent calls retry, others never do."""

import httpx
import pytest

from calod import CalodClient, EffectConflict
from calod.sync import SyncCalodClient


def flaky_handler(failures: int, counter: list):
    def handler(request: httpx.Request) -> httpx.Response:
        counter.append(request.url.path)
        if len(counter) <= failures:
            return httpx.Response(503, json={"detail": "unavailable"})
        return httpx.Response(200, json={"status": "ok"})

    return handler


async def test_get_retries_on_503_then_succeeds():
    counter: list = []
    client = CalodClient(
        "http://calod.test",
        "cld_x",
        retries=2,
        retry_backoff_seconds=0.0,
        http_client=httpx.AsyncClient(
            transport=httpx.MockTransport(flaky_handler(2, counter)),
            base_url="http://calod.test",
        ),
    )
    assert await client.health_ready() is True
    assert len(counter) == 3


async def test_conflict_is_never_retried():
    counter: list = []

    def handler(request: httpx.Request) -> httpx.Response:
        counter.append(1)
        return httpx.Response(
            409,
            json={
                "code": "IDEMPOTENCY_CONFLICT",
                "detail": "different input",
                "request_id": "r-1",
            },
        )

    client = CalodClient(
        "http://calod.test",
        "cld_x",
        retries=3,
        retry_backoff_seconds=0.0,
        http_client=httpx.AsyncClient(
            transport=httpx.MockTransport(handler), base_url="http://calod.test"
        ),
    )
    with pytest.raises(EffectConflict):
        await client.acquire(
            scope="s", tool_name="t", input={}, idempotency_key="key-00001"
        )
    assert len(counter) == 1


async def test_transport_error_retries_then_reraises():
    counter: list = []

    def handler(request: httpx.Request) -> httpx.Response:
        counter.append(1)
        raise httpx.ConnectError("refused", request=request)

    client = CalodClient(
        "http://calod.test",
        "cld_x",
        retries=1,
        retry_backoff_seconds=0.0,
        http_client=httpx.AsyncClient(
            transport=httpx.MockTransport(handler), base_url="http://calod.test"
        ),
    )
    with pytest.raises(httpx.ConnectError):
        await client.health_ready()
    assert len(counter) == 2  # initial + 1 retry


def test_sync_client_retries_and_parity_of_methods():
    counter: list = []
    client = SyncCalodClient(
        "http://calod.test",
        "cld_x",
        retries=2,
        retry_backoff_seconds=0.0,
        http_client=httpx.Client(
            transport=httpx.MockTransport(flaky_handler(1, counter)),
            base_url="http://calod.test",
        ),
    )
    assert client.health_ready() is True
    assert len(counter) == 2

    async_methods = {
        m for m in dir(CalodClient) if not m.startswith("_") and m != "aclose"
    }
    sync_methods = {
        m for m in dir(SyncCalodClient) if not m.startswith("_") and m != "close"
    }
    assert async_methods == sync_methods
