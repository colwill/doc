---
title: Events are not being delivered
runbook: true
environment: production
resources: [service:backend, component:event-bus, component:service-bus, component:cache-bus, component:doc-event-source]
---
# Events are not being delivered

The backend publishes what happens through `doc-event-source`, which sits on all three buses: the
event bus carries events, the service bus carries calls between plugins and background tasks, and
the cache bus holds what is cached. The backend subscribes to the event bus to hand each event to
the plugins that asked for it.

When events stop, automations do not fire, Agent Smith's jobs that start on an event never start,
pages stop updating by themselves, and reminders arrive late or not at all.

## Is this what is happening?

1. `doc_status`: every bus node's role, leader and replication lag. A bus with no leader, or a
   node far behind, is the likeliest cause.
2. Read each bus in the Catalogue with `doc_resource` (kind `Component`, names `event-bus`,
   `service-bus` and `cache-bus`) for anything recorded about it.
3. `doc_jobs`: if a job that starts on an event has a last run much older than the events it
   should have heard, that confirms it.

## Which bus

- **The event bus** — automations and event-started jobs stop, but pages still load. Carry on.
- **The service bus** — calls between plugins fail with *deadline* or *no-handler*, and
  background tasks stay *queued*. Carry on.
- **The cache bus** — every page is slow, and core's API may stall. Carry on, and say so first.

## Put it right (by hand)

4. In development the buses run in the backend's own memory, so restarting the stack with
   `just dev` restarts them. Write that out for the requester.
5. In a cluster, the requester restarts the node with no leader, or the one far behind, one at a
   time, and waits for a leader before the next.
6. Afterwards, read `doc_status` again and confirm each bus has a leader and little lag.

## Afterwards

7. Events published while a bus was down are not lost unless its log was: say which automations
   or jobs the requester should run by hand to catch up, from what `doc_jobs` showed.
