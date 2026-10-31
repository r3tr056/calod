#!/usr/bin/env python3
"""Razorpay UPI collect with an `outcome_unknown` path (Phase 2).

The stub gateway accepts the collect request, then the response is
lost (timeout): the money may have moved, so Calod records
`outcome_unknown` and the SDK raises `OutcomeUnknown` instead of
retrying. An operator checks the provider, reconciles the effect
as committed, and only then does a retry replay the receipt. The
provider is still called exactly once throughout.

Run:  python examples/razorpay_upi.py   (needs a built calod binary)
"""

import asyncio

import calod
from calod import OutcomeUnknown
from calod.testing import LocalServer


class StubRazorpay:
    """UPI collect stub: first call is captured but its reply is lost."""

    def __init__(self) -> None:
        self.calls = 0
        self.captured: dict[str, dict] = {}

    async def collect(self, payment_id: str, amount_paise: int) -> dict:
        self.calls += 1
        receipt = {"payment_id": payment_id, "status": "captured", "amount": amount_paise}
        self.captured[payment_id] = receipt
        raise TimeoutError("gateway response lost after capture")


razorpay = StubRazorpay()


@calod.effect(scope=lambda ctx: ctx.payment_id, tool_name="razorpay.upi_collect")
async def collect_upi(payment_id: str, amount_paise: int) -> dict:
    return await razorpay.collect(payment_id, amount_paise)


async def main() -> None:
    with LocalServer() as server:
        calod.configure(base_url=server.base_url, api_key=server.api_key)

        try:
            await collect_upi("pay_2001", 49900)
            raise AssertionError("expected OutcomeUnknown")
        except OutcomeUnknown as unknown:
            effect = unknown.effect
            print(f"outcome_unknown recorded for effect {effect.id}")

        # Operator truth from the provider: the collect did capture.
        provider_receipt = razorpay.captured["pay_2001"]
        async with calod.CalodClient(server.base_url, server.api_key) as client:
            reconciled = await client.reconcile(
                effect.id,
                fencing_token=effect.fencing_token,
                outcome="committed",
                result=provider_receipt,
            )
        print(f"reconciled as: {reconciled.state}")

        replayed = await collect_upi("pay_2001", 49900)
        print(f"retry replayed: {replayed}")
        print(f"provider_calls={razorpay.calls}")
        assert replayed == provider_receipt
        assert razorpay.calls == 1, "provider must be called exactly once"
    print("OK: outcome_unknown reconciled, retry replayed, provider called once")


if __name__ == "__main__":
    asyncio.run(main())
