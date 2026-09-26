# Deploying DOC

Two ways to run the whole platform, both arranged the same way: **`doc-system`** is the platform —
storage, the fabric, core and the workers — and **every plugin is a deployment of its own** that
connects to it. Stopping, rebuilding or upgrading one plugin touches nothing else.

| | |
|---|---|
| [`compose/`](compose/) | Docker Compose: one project for `doc-system`, one per plugin |
| [`kubernetes/`](kubernetes/) | Kubernetes: the `doc-system` namespace, and a Service and Deployment per plugin in `doc-plugins` |

The stacks under `crates/*/docker-compose.yaml` are the development ones, split by the part of the
repository they belong to and brought up together by `just up`. These are for running DOC
somewhere, where each plugin has its own lifecycle.

## What every deployment needs

**The secrets.** One bootstrap step writes the CA, every certificate and every plugin's
registration token. Everything else reads them. Nothing registers without them, so this runs
first — and again whenever a plugin ID is added to `config/doc.toml`, since a plugin with no token
cannot register at all.

**The workers.** Anything that goes through `backend.task()` — an import finishing, a sync
starting, a maturity model being scored — runs in the workers. Without them the platform serves
pages and does nothing in the background.

**The fabric.** Three nodes each of the event, service and cache buses. `DOC_FABRIC_MODE=memory`
runs without them, which is what `just dev` does; that mode has no workers and no cross-process
events, so it is for development only.

## What was checked

The Compose files are validated by `docker compose config` — all 39 of them. The Kubernetes
manifests are validated by `kubernetes/validate.py`, which reads them and checks what a schema
check would not: a Service that fronts nothing, a mount with no volume, a claim or Secret nothing
declares. It needs no cluster. All 102 documents pass.

**Neither has been deployed.** Doing that needs the 41 images built and pushed somewhere a cluster
can pull them, which is a step for whoever runs it. Read the two READMEs for what is still to fill
in — the image registry, the ingress host, the storage class and every password in this directory.
