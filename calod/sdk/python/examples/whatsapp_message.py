#!/usr/bin/env python3
"""WhatsApp Business message protected by Calod (Phase 2 example).

Sending the same order confirmation twice spams the customer; a
retried send must replay the receipt instead. The stub counts real
sends; a deliberate double-invoke must produce exactly one.

Run:  python examples/whatsapp_message.py   (needs a built calod binary)
"""

import asyncio

import calod
from calod.testing import LocalServer


class StubWhatsApp:
    def __init__(self) -> None:
        self.calls = 0
        self.sent: list[str] = []

    async def send_template(self, phone: str, template: str) -> dict:
        self.calls += 1
        message_id = f"wamid.stub.{self.calls}"
        self.sent.append(message_id)
        return {"message_id": message_id, "to": phone, "template": template}


whatsapp = StubWhatsApp()


@calod.effect(scope=lambda ctx: ctx.order_id, tool_name="whatsapp.send_template")
async def send_confirmation(order_id: str, phone: str) -> dict:
    return await whatsapp.send_template(phone, "order_confirmed")


async def main() -> None:
    with LocalServer() as server:
        calod.configure(base_url=server.base_url, api_key=server.api_key)
        first = await send_confirmation("order-3001", "+919999999999")
        second = await send_confirmation("order-3001", "+919999999999")
        print(f"first:  {first}")
        print(f"second: {second}")
        print(f"provider_calls={whatsapp.calls}")
        assert first == second
        assert whatsapp.calls == 1, "customer must receive exactly one message"
    print("OK: retry replayed the receipt, exactly one WhatsApp message sent")


if __name__ == "__main__":
    asyncio.run(main())
