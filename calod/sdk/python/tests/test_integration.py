"""Live-server integration tests (real calod binary, real WAL).

Skipped automatically when the server binary has not been built.
Build it with `cargo build` in the repo's calod/ directory.
"""

import pytest

from calod import CalodClient, effect
from calod.testing import LocalServer


@pytest.fixture(scope="module")
def server():
    try:
        srv = LocalServer().start()
    except (FileNotFoundError, RuntimeError, TimeoutError) as exc:
        pytest.skip(f"calod server unavailable: {exc}")
    yield srv
    srv.stop()


async def test_live_double_invoke_calls_tool_once(server):
    calls = []

    async with CalodClient(server.base_url, server.api_key) as client:

        @effect(client=client, scope="order-77", tool_name="stripe.charge")
        async def charge(order_id: str, amount_minor: int):
            calls.append(order_id)
            return {"charged": amount_minor, "receipt": "rcpt_1"}

        first = await charge("order-77", 5000)
        second = await charge("order-77", 5000)

    assert first == second == {"charged": 5000, "receipt": "rcpt_1"}
    assert calls == ["order-77"], "tool body must run exactly once"


async def test_live_export_metrics_and_restart_replay(server):
    async with CalodClient(server.base_url, server.api_key) as client:
        run = await client.create_run(
            external_id="phase2-run", agent_name="phase2-agent",
            idempotency_key="phase2-run-create",
        )
        decision = await client.acquire(
            scope="order-88",
            tool_name="stripe.charge",
            input={"amount_minor": 100},
            idempotency_key="order-88:charge",
            run_id=run.id,
        )
        assert decision.decision == "execute"
        await client.start(
            decision.effect.id,
            lease_token=decision.lease_token,
            fencing_token=decision.effect.fencing_token,
        )
        committed = await client.commit(
            decision.effect.id,
            lease_token=decision.lease_token,
            fencing_token=decision.effect.fencing_token,
            result={"charged": 100},
        )
        assert committed.state == "committed"

        bundle = await client.export_run(run.id)
        assert bundle.run.id == run.id
        assert bundle.manifest.signature
        assert len(bundle.events) >= 2

        metrics = await client.metrics()
        assert "calod_acquires_total" in metrics

    # After a hard restart the receipt still replays (WAL durability).
    server.restart()
    async with CalodClient(server.base_url, server.api_key) as client:
        replay = await client.acquire(
            scope="order-88",
            tool_name="stripe.charge",
            input={"amount_minor": 100},
            idempotency_key="order-88:charge",
        )
        assert replay.decision == "replay"
        assert replay.effect.result == {"charged": 100}
