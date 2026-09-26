# Payment flow

1. The checkout asks for an authorisation with an idempotency key.
2. The router picks a scheme and the adapter sends the request.
3. The scheme answers, and the answer is stored against the key.

## Ledger

Captures and refunds are posted to the ledger as double entries, so the books always balance.
Back to the [overview](overview.md).
