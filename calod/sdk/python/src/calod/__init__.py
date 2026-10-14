"""Calod Python SDK — durable, idempotent tool execution for AI agents.

Quickstart::

    import calod

    calod.configure(base_url="http://127.0.0.1:7379", api_key="cld_...")

    @calod.effect(scope=lambda ctx: ctx.order_id)
    async def charge_card(order_id: str, amount_minor: int):
        return await stripe.charge(
            amount_minor,
            idempotency_key=calod.current_idempotency_key(),
        )

Sync variant::

    from calod.sync import SyncCalodClient, effect
"""

from .canonical import canonical_dumps
from .client import CalodClient
from .context import current_effect_id, current_idempotency_key
from .decorator import configure, effect
from .exceptions import (
    CalodError,
    EffectConflict,
    EffectWaitTimeout,
    LeaseLost,
    OutcomeUnknown,
)
from .models import (
    Effect,
    EffectDecision,
    EffectFailure,
    EffectLease,
    Event,
    ExternalReceipt,
    Page,
    Run,
)

__version__ = "0.1.0"

__all__ = [
    "CalodClient",
    "CalodError",
    "Effect",
    "EffectConflict",
    "EffectDecision",
    "EffectFailure",
    "EffectLease",
    "EffectWaitTimeout",
    "Event",
    "ExternalReceipt",
    "LeaseLost",
    "OutcomeUnknown",
    "Page",
    "Run",
    "__version__",
    "canonical_dumps",
    "configure",
    "current_effect_id",
    "current_idempotency_key",
    "effect",
]
