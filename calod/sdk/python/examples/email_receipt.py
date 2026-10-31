#!/usr/bin/env python3
"""Email receipt protected by Calod, via the sync client (Phase 2).

Uses `calod.sync` to show the sync/async parity: same decorator
semantics, no event loop. A retried send must not email the
customer twice; the stub SMTP sender counts real sends.

Run:  python examples/email_receipt.py   (needs a built calod binary)
"""

from calod.sync import SyncCalodClient, effect
from calod.testing import LocalServer


class StubSmtp:
    def __init__(self) -> None:
        self.calls = 0

    def send(self, to: str, subject: str) -> dict:
        self.calls += 1
        return {"message_id": f"<stub-{self.calls}@mail>", "to": to, "subject": subject}


def main() -> None:
    smtp = StubSmtp()
    with LocalServer() as server:
        with SyncCalodClient(server.base_url, server.api_key) as client:

            @effect(client=client, scope="order-4001", tool_name="email.send_receipt")
            def send_receipt(order_id: str, email: str) -> dict:
                return smtp.send(email, f"Receipt for {order_id}")

            first = send_receipt("order-4001", "buyer@example.com")
            second = send_receipt("order-4001", "buyer@example.com")
        print(f"first:  {first}")
        print(f"second: {second}")
        print(f"provider_calls={smtp.calls}")
        assert first == second
        assert smtp.calls == 1, "exactly one email must be sent"
    print("OK: sync retry replayed the receipt, exactly one email sent")


if __name__ == "__main__":
    main()
