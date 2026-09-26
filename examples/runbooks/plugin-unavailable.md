---
title: A plugin's pages say Service Unavailable
runbook: true
environment: production
resources: [service:backend, service:frontend]
---
# A plugin's pages say Service Unavailable

The frontend sends each plugin's page through the backend to the plugin's own process. When one
plugin is down, its pages say *Service Unavailable* while the rest of DOC is fine.

## Is this what is happening?

1. `doc_status`: list every plugin that is not `running`, with its state and error.
2. If `backend` itself is down, follow *The DOC backend is unreachable* instead, and stop here.
3. `doc_api` to `reliability`, `GET doc`: how long each of those plugins has been down today.

## Read the error

For each plugin that is not running, say what its error means:

- *not running* or *unreachable* — its process has stopped or cannot be dialled.
- *loading* for minutes — it registered but its `load` is stuck, often waiting on a setting or a
  credential from Secret Storage.
- *turned off* — an administrator turned it off, or the flag it follows is off. That is not an
  outage: say who to ask.
- *error* with a detail — quote the detail.

## Put it right (by hand)

4. For a plugin that is *turned off*, stop: tell the requester it is off on purpose and where to
   turn it on (the plugin's page, **Overview**).
5. Otherwise the requester reloads it as an administrator: `POST /api/v1/plugins/<id>/reload`, or
   **Reload** on the plugin's page. Agent Smith cannot do this; it is core's, not a plugin's.
6. If it fails again straight away, the requester reads the plugin process's own output in the
   `just dev` terminal for its first error.

## Afterwards

7. Notify the requester (`doc_notify` to `me`) with the plugins that were down and what each said.
