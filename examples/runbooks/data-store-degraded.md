---
title: The data store is degraded
runbook: true
environment: production
resources: [service:backend, component:data-store]
---
# The data store is degraded

Every core table and every plugin's collections live in the data store, PostgreSQL, which the
backend calls for nearly every request. When it is slow or full, pages hang, saves fail and
background tasks pile up.

## Is this what is happening?

1. `doc_status`: is `postgres` up, and how long has its health check been taking?
2. `doc_api` to `reliability`, `GET doc`: DOC's own availability over the last day, by part. Note
   whether `postgres` has dropped below its objective.
3. `doc_api` to `reliability`, `GET outages` with `open=true`: is anything already open?

## Find the cause (by hand)

Write these out for the requester, in order, with what each should show:

4. `docker exec doc-storage-postgres-1 pg_isready` — *accepting connections*.
5. `docker exec doc-storage-postgres-1 df -h /var/lib/postgresql/data` — under 90% used.
6. `docker exec doc-storage-postgres-1 psql -U postgres -c "select count(*), state from
   pg_stat_activity group by state"` — no more than a few dozen connections, few *idle in
   transaction*.
7. `docker exec doc-storage-postgres-1 psql -U postgres -d doc -c "select state, count(*) from
   core.tasks group by state"` — a large number *queued* means the workers are behind.

## Put it right

8. If the disk is nearly full, the requester frees space or grows the volume; nothing in DOC does.
9. If connections are exhausted, restarting the backend releases its pool: `just dev`.
10. If an outage is not already open for `backend`, open one on `reliability`
    (`POST services/backend/down`), and close it (`POST services/backend/up`) once pages load
    again. Against production, list both for the requester.
