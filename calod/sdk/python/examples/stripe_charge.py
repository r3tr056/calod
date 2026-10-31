#!/usr/bin/env python3
"""Stripe charge protected by Calod (Phase 2 adapter example).

A stub stands in for the Stripe API and counts how often it is
really called. The decorated charge is invoked twice with the
same business arguments (a retry): the provider must see exactly
one call, and the second invocation replays the stored receipt.

Run:  python examples/stripe_charge.py   (needs a built calod binary)
"""

import asyncio

import calod
from calod.testing import LocalServer


class StubStripe:
    """Counts real provider calls, like a Stripe charges endpoint."""

    def __init__(self) -> None:
        self.calls = 0

    async def charge(self, amount_minor: int, *, idempotency_key: str) -> dict:
        self.calls += 1
        return {"id": "ch_stub_1", "amount_minor": amount_minor, "status": "succeeded"}


stripe = StubStripe()


@calod.effect(scope=lambda ctx: ctx.order_id, tool_name="stripe.charge")
async def charge_card(order_id: str, amount_minor: int) -> dict:
    return await stripe.charge(
        amount_minor, idempotency_key=calod.current_idempotency_key()
    )


async def main() -> None:
    with LocalServer() as server:
        calod.configure(base_url=server.base_url, api_key=server.api_key)
        first = await charge_card("order-1001", 5000)
        second = await charge_card("order-1001", 5000)  # deliberate retry
        print(f"first:  {first}")
        print(f"second: {second}")
        print(f"provider_calls={stripe.calls}")
        assert first == second
        assert stripe.calls == 1, "provider must be called exactly once"
    print("OK: retry replayed the receipt, provider called once")


if __name__ == "__main__":
    asyncio.run(main())
