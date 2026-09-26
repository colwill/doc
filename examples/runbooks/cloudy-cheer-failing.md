---
title: Cloudy Cheer's job is failing
runbook: true
environment: test
resources: [service:cloudy-cheer, component:cloudy-cheer, repository:colwill/cloudy-cheer]
---
# Cloudy Cheer's job is failing

Cloudy Cheer is an experiment: a DOC plugin written in Go, run as a job, which cheers people up on
cloudy days. Its code is in `colwill/cloudy-cheer`.

## Is this what is happening?

1. `doc_status`: is the plugin `cloudy-cheer` registered and `running`? Quote its error if not.
2. `doc_readiness` for the service `cloudy-cheer`: what maturity, delivery, pipelines,
   reliability and end of life say about it.
3. `cicd_broken_pipelines` (or `doc_api` to `cicd`) for `colwill/cloudy-cheer`: is its build red?
4. `eol` tools, if offered: is the Go version it is built with near or past its end of life?

## Decide

- **The plugin is not registered** — its process is not running. Carry on to *By hand*.
- **Its pipeline is broken** — the job is failing because the last change did not build. Say which
  run broke and when.
- **Its Go version is past end of life** — say so as a likely cause, and the version to move to.
- **Everything looks healthy** — say so: the failure is in what it does, not whether it runs.

## Tell people

5. Start a Watercooler discussion tagged `service:cloudy-cheer` (`doc_discuss`) titled
   *Cloudy Cheer is not cheering*, with what you found in the steps above. This run is against
   test, so it may.

## By hand

6. For the requester: in `colwill/cloudy-cheer`, `go build ./... && go test ./...` to reproduce;
   then restart the plugin's process and watch it register in DOC's status page.
