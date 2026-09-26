---
title: CodeCaChe's code map is stale
runbook: true
environment: development
resources: [service:tooling, repository:colwill/ccc]
---
# CodeCaChe's code map is stale

CodeCaChe (`colwill/ccc`) is the platform's static analyser. The Insights plugin scans a
repository with it to answer what the code holds, and the Knowledge Base keeps `colwill/ccc`'s own
documentation in the space `colwill-ccc`. When the map is stale, insights and search miss code
that has changed since the last scan.

## Is this what is happening?

1. `doc_api` to `insights`, `GET repositories`: find `colwill/ccc` and note when it was last
   scanned and at which commit.
2. `doc_api` to `insights`, `GET repositories/<source>/colwill/ccc/scans`, with the source from
   step 1: did the last scans succeed?
3. `kb_search_docs` in the space `colwill-ccc` for something recently written about, to see whether
   its documentation is up to date too.

## Put it right

4. If the last scan is more than a day old, or failed, queue another: `doc_api` to `insights`,
   `POST scans` with `{"repository": "colwill/ccc"}`. A run against development may do this.
5. Read the repository again after a minute (`GET repositories/<source>/colwill/ccc/scans`) and
   say whether the new scan is waiting, running or done.
6. If the documentation is stale too, say so: the Knowledge Base resyncs `colwill-ccc` on its own
   schedule, or the requester presses **Sync now** on the space's source.

## Report

7. When the last good scan was, what was queued, and what the requester should look at if it
   fails again.
