---
title: Card gateway runbook
resources: [service:card-gateway, service:ledger]
---
# Runbook

## Authorisations are failing

Check the scheme status page first, then the adapter logs for timeouts. If one scheme is down,
turn on failover with `gatewayctl failover enable --scheme visa`.

<script>alert("this must never reach a page")</script>

<img src="x" onerror="alert('nor this')">

[A link that runs script](javascript:alert(1))

## Refunds are stuck

Refunds retry for 24 hours. After that, post them by hand from the [ledger](architecture/payment-flow.md#ledger).
