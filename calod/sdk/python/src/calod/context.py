"""Call-context for code running inside a protected effect.

While a function wrapped with :func:`calod.effect` executes (i.e. the acquire
returned ``execute`` and the wrapped function is actually running), these
context variables expose the stable identifiers of the in-flight effect — most
importantly the idempotency key, so it can be forwarded to downstream
providers that support their own idempotency (Stripe, Razorpay, ...).
"""

from __future__ import annotations

import contextvars
from typing import Optional

_current_idempotency_key: contextvars.ContextVar[Optional[str]] = contextvars.ContextVar(
    "calod_idempotency_key", default=None
)
_current_effect_id: contextvars.ContextVar[Optional[str]] = contextvars.ContextVar(
    "calod_effect_id", default=None
)


def current_idempotency_key() -> str:
    """The stable idempotency key of the effect currently executing.

    Forward this to downstream providers as *their* idempotency key so a
    retried Calod effect replays at the provider too::

        @calod.effect(scope=lambda ctx: ctx.order_id)
        async def charge_card(order_id: str, amount: int):
            return await stripe.charge(
                amount, idempotency_key=calod.current_idempotency_key()
            )

    Raises :class:`LookupError` outside a protected effect.
    """
    key = _current_idempotency_key.get()
    if key is None:
        raise LookupError("no calod effect is executing in this context")
    return key


def current_effect_id() -> str:
    """The ledger effect id of the effect currently executing.

    Raises :class:`LookupError` outside a protected effect.
    """
    effect_id = _current_effect_id.get()
    if effect_id is None:
        raise LookupError("no calod effect is executing in this context")
    return effect_id


def _set(key: str, effect_id: str) -> tuple[contextvars.Token, contextvars.Token]:
    return _current_idempotency_key.set(key), _current_effect_id.set(effect_id)


def _reset(tokens: tuple[contextvars.Token, contextvars.Token]) -> None:
    _current_idempotency_key.reset(tokens[0])
    _current_effect_id.reset(tokens[1])
