---
title: The DOC backend is unreachable
runbook: true
environment: production
resources: [service:backend, service:frontend, component:data-store]
---
# The DOC backend is unreachable

The frontend calls the backend for every page, and the backend calls the data store. When the
backend cannot be reached, every plugin's page says *Service Unavailable* and sign-in fails.

## Is this what is happening?

1. Ask for DOC's own health with `doc_status`. Note every part that is not up: `backend`,
   `postgres`, a bus node, and how many plugins are not running.
2. Read open outages: `doc_api` to `reliability`, `GET outages` with the query `open=true`.
3. Read the backend in the Catalogue with `doc_resource` (kind `Service`, name `backend`) to see
   what it depends on.

Then decide:

- **`postgres` is not up** — follow *The data store is degraded* instead, and stop here.
- **One or a few plugins are down but `backend` is up** — follow *A plugin's pages say Service
  Unavailable* instead, and stop here.
- **`backend` itself is down or `doc_status` cannot be read** — carry on.

## Record it

4. If no outage is open for `backend`, open one: `doc_api` to `reliability`,
   `POST services/backend/down` with `{"note": "<what you saw, in a sentence>"}`.
   Against production this is refused: put it in the report for the requester.

## Put it right (by hand)

These are outside DOC. Write each one out for the requester:

5. Check the data store is up: `docker ps --filter name=doc-storage-postgres` should show it
   running, and `docker exec doc-storage-postgres-1 pg_isready` should say *accepting connections*.
6. Check the backend is listening: `ss -ltnp | grep ':8080'` should show `doc-backend`.
7. If it is not, restart the development stack with `just dev`, and watch its output for the
   first error.
8. Once `http://127.0.0.1:8080/api/v1/status` answers, close the outage:
   `POST services/backend/up` on `reliability`.

## Afterwards

9. Start a Watercooler discussion tagged `service:backend` (`doc_discuss`) saying what went wrong,
   how long for, and what put it right. Against production, write it for the requester to post.
