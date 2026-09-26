# Card gateway

The card gateway authorises, captures and refunds card payments for every Acme checkout. It sits
between the checkout services and the card schemes, and records every movement in the
[ledger](architecture/payment-flow.md#ledger).

- Owned by **payments-core**, in the payments organisation
- Tier 1: an outage stops customers paying
- Start with [Getting started](getting-started.md), or go straight to the [runbook](runbook.md)
