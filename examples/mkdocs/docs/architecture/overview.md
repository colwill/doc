# Architecture overview

| Component | Does | Talks to |
|---|---|---|
| Router | Chooses a scheme for each card by its BIN range | Scheme adapters |
| Scheme adapters | Speak each card scheme's protocol | Visa, Mastercard, Amex |
| Idempotency store | Makes a retried payment safe | Postgres |

Every authorisation is written before it is sent, so a crash never loses one. See the
[payment flow](payment-flow.md) for the order of events.
