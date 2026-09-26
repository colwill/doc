---
title: Turn on debug logging for the backend
runbook: true
environment: development
resources: [service:backend]
---
# Turn on debug logging for the backend

The backend reads its log level from the feature flag `log-level`, kept per environment by the
Feature flags plugin. Turning it up to `debug` for a while is how you see what a request did.

**Only ever in development or test.** A run against production must change nothing: it says what
it would have done, and stops.

## Steps

1. Find the flag: `doc_api` to `flags`, `GET entries` with the query `service=backend`. Find the
   entry whose key is `log-level` and whose environment is the one this run is against.
2. If there is no such entry, stop and say so: it is made on the Feature flags page, with
   **Add a flag**, as a string.
3. If its value is already `debug`, stop and say so: nothing to do.
4. Note its current value, so it can be set back.
5. Set it to `debug`: `doc_api` to `flags`, `PATCH entries/<its id>` with `{"value": "debug"}`.
   A run limited to development and test is refused if the entry is in production, which is
   right: say so and stop.
6. Read it back with `GET entries/<its id>` and confirm the value is `debug`.

## Set it back later

7. Set a reminder for the requester in 2 hours (`doc_remind`, `at`: `in 2 hours`) to set `log-level`
   for the backend back to the value noted in step 4, with a link to the Feature flags page
   (`/p/flags/`).

## Report

8. Say the value it had, the value it has now, the environment, and when the reminder is due.
