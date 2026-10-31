# Adapter examples

Runnable, self-contained examples. Each one starts a throwaway
local Calod server (`calod.testing.LocalServer`, needs a built
`calod` binary — `cargo build` in the repo's `calod/` directory),
protects a stubbed provider call with `@calod.effect`, invokes it
twice (a deliberate retry), and asserts the provider saw exactly
one call.

| Example | Shows |
|---|---|
| `stripe_charge.py` | Card charge; retry replays the receipt |
| `razorpay_upi.py` | UPI collect where the response is lost: `outcome_unknown`, operator reconcile, then replay |
| `whatsapp_message.py` | WhatsApp template send; customer gets exactly one message |
| `email_receipt.py` | Sync client (`calod.sync`) parity; exactly one email |

Run any of them with `python examples/<name>.py`. In real code,
replace the stub provider with the actual SDK (Stripe, Razorpay,
Meta, SMTP) and keep the decorator: the Calod side does not change.
