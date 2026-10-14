# Calod Python SDK

Python SDK for Calod, the durable action ledger for AI agents. It wraps the
Calod REST API (`/v1`) so a retried tool call cannot repeat a real-world side
effect: the first caller executes, concurrent callers wait, and retries after
a commit replay the recorded receipt instead of running the tool again.

## Install

```bash
pip install calod
```

## Quickstart

```python
import calod

calod.configure(base_url="http://127.0.0.1:7379", api_key="cld_...")


@calod.effect(scope=lambda ctx: ctx.order_id)
async def charge_card(order_id: str, amount_minor: int):
    return await stripe.charge(
        amount_minor,
        idempotency_key=calod.current_idempotency_key(),
    )
```

A synchronous client is available from `calod.sync`. See
`src/calod/__init__.py` for the exported API and the server repository's
`docs/openapi.yaml` for the underlying REST contract.
