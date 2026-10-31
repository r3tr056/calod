"""Decorator happy-path tests with a stub ledger client (no server)."""

from types import SimpleNamespace

import pytest

from calod import OutcomeUnknown, effect


def make_effect(result=None, state="committed"):
    return SimpleNamespace(
        id="11111111-1111-1111-1111-111111111111",
        fencing_token=1,
        result=result,
        state=state,
    )


class StubClient:
    """Records calls; first acquire executes, later ones replay."""

    def __init__(self, first_decision="execute"):
        self.calls = []
        self.first_decision = first_decision
        self.acquires = 0

    async def acquire(self, **kwargs):
        self.calls.append(("acquire", kwargs))
        self.acquires += 1
        if self.acquires == 1 and self.first_decision == "execute":
            return SimpleNamespace(
                decision="execute",
                effect=make_effect(state="claimed"),
                lease_token="lease-1",
                poll_after_ms=None,
                retry_after=None,
            )
        if self.first_decision == "reconcile":
            return SimpleNamespace(
                decision="reconcile",
                effect=make_effect(state="outcome_unknown"),
                lease_token=None,
                poll_after_ms=None,
                retry_after=None,
            )
        return SimpleNamespace(
            decision="replay",
            effect=make_effect(result={"charged": 5000}),
            lease_token=None,
            poll_after_ms=None,
            retry_after=None,
        )

    async def start(self, effect_id, **kwargs):
        self.calls.append(("start", effect_id))
        return make_effect(state="in_progress")

    async def commit(self, effect_id, **kwargs):
        self.calls.append(("commit", kwargs))
        return make_effect(result=kwargs["result"])

    async def fail(self, effect_id, **kwargs):
        self.calls.append(("fail", kwargs))
        return make_effect(state="failed_terminal")

    async def mark_outcome_unknown(self, effect_id, **kwargs):
        self.calls.append(("mark_outcome_unknown", kwargs))
        return make_effect(state="outcome_unknown")


async def test_effect_executes_once_then_replays_receipt():
    stub = StubClient()
    invocations = []

    @effect(client=stub, scope="order-123", tool_name="charge_card")
    async def charge(order_id: str, amount_minor: int):
        invocations.append(order_id)
        return {"charged": amount_minor}

    first = await charge("order-123", 5000)
    second = await charge("order-123", 5000)

    assert first == {"charged": 5000}
    assert second == {"charged": 5000}
    # The tool body ran exactly once; the second call replayed.
    assert invocations == ["order-123"]
    kinds = [kind for kind, *_ in stub.calls]
    assert kinds == ["acquire", "start", "commit", "acquire"]


async def test_effect_reconcile_decision_raises_outcome_unknown():
    stub = StubClient(first_decision="reconcile")

    @effect(client=stub, scope="order-123", tool_name="charge_card")
    async def charge(order_id: str):
        return {"charged": 1}

    with pytest.raises(OutcomeUnknown):
        await charge("order-123")
