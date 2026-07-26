# {{ values.title }}

{{ values.description }}

A DOC plugin, written in Go, created from DOC's **{{ template.title }}** template.

| | |
|---|---|
| Plugin ID | `{{ values.name }}` |
| In the navigation | **{{ values.title }}**, under **{{ values.menu }}** |
| Classification | `{{ values.classification }}` |
| Go module | `{{ scaffold.module }}` |
| Lifecycle | **{{ lifecycle.title }}.** {{ lifecycle.about }} |

## What is here

| | |
|---|---|
| `main.go` | The **manifest**: what the plugin tells DOC about itself — its navigation entry, its settings, its collections, its schedules and what automations can call |
| `routes.go` | Its pages, under `/ui/`, and its API, under `/api/`, on Go's `http.ServeMux` |
| `run.go` | What it does when DOC loads it, runs it, cancels it and gives it an event |
| `pages/` | The HTML for its pages, in `html/template`, compiled into the binary |
| `internal/doc/` | DOC's plugin protocol. Nothing in it needs changing to write the plugin |

It already works: a page that lists items and adds them, the same through its API, a search, a
setting, and an operation automations can call. Replace them with what the plugin is for.

## Getting it into DOC

DOC only accepts a plugin it has been told to expect. Whoever runs DOC:

1. adds `"{{ values.name }}"` to `ids` under `[plugins]` in DOC's `config/doc.toml`;
2. starts DOC again (with `just dev`, stop it and start it; with Docker Compose,
   `just plugin-add {{ values.name }}` once there is a service for it). Starting it makes the
   plugin's registration token and certificate in DOC's `secrets` directory.

## Running it

With Go 1.24 or newer, beside a DOC started with `just dev`:

```sh
make run SECRETS=path/to/doc/secrets
```

`BACKEND` (`127.0.0.1:4433`) is where DOC's plugin host listens, and `PORT` (`4490`) the UDP port
the plugin listens on. Each change needs the plugin stopped and started again, and a change to be
deployed needs a new version: `make run VERSION=0.2.0`.

## Verification

Check each of these against a development DOC before the plugin runs anywhere that matters.

**The code**

- [ ] `go vet ./...` finds nothing, and `gofmt -l .` lists nothing.

**Registering**

- [ ] Started before DOC, it keeps retrying, and registers within a few seconds of DOC coming up.
- [ ] **Admin → Plugins** shows `{{ values.name }}` at version 0.1.0, `{{ values.classification }}`,
      running. A plugin DOC refuses says why in its log.
- [ ] Its collection, `items`, is there: the page lists items without an error.

**The page and the API**

- [ ] **{{ values.menu }}** has **{{ values.title }}**, and its page greets you by name.
- [ ] **Add an item** opens a page of its own; adding one there goes back to the list with it on; searching finds it.
- [ ] `curl -H "Authorization: Bearer $DOC_TOKEN" {{ platform.url }}/api/v1/plugins/{{ values.name }}/api/items`
      answers the items as JSON, and a `POST` of `{"name": "…"}` answers `201` with the new one.
- [ ] Somebody who may only read the plugin sees no form, and their `POST` is refused by DOC with
      `403` before the plugin sees it.

**Settings and automations**

- [ ] The plugin's Settings page has **Greeting**; changing it changes the page straight away.
- [ ] **Automations** offers **{{ values.name }} · Add an item**.

**Running and handing over**

{% if values.classification == 'long-running' %}- [ ] It starts running as soon as it is loaded, and keeps going; **Cancel** on the Plugins page
      stops the run, and **Resume** starts it again.
{% else %}- [ ] `POST /api/v1/plugins/{{ values.name }}/run` answers what `run` returns.
{% endif %}- [ ] With 0.1.0 running, `make run VERSION=0.2.0 PORT=4491` in another terminal: DOC loads 0.2.0,
      moves every request to it, and 0.1.0 exits by itself. The page never goes away.
- [ ] `kill -9` the plugin: within half a minute DOC shows it in error, and the rest of DOC is
      unaffected. Started again, it is running again.

DOC-SPEC.md, in DOC's repository, has the whole conformance checklist (§14) and everything the
protocol does; `internal/doc` follows it section by section.

## Deploying it

`make image` builds a container from the `Dockerfile`: a static binary on a distroless base. Run it
with DOC's secrets mounted read-only at `/secrets`, on the network DOC's backend is on, and with
`DOC_BACKEND_QUIC` set to where the backend's plugin host listens (`backend:4433` by default).
Don't publish its UDP port: only DOC's backend calls it.

| Variable | Default | Meaning |
|---|---|---|
| `DOC_SECRETS_DIR` | `/secrets` | Where the CA, the plugin's certificate and its token are |
| `DOC_PLUGIN_TOKEN` | `tokens/plugins/{{ values.name }}.token` there | The registration token itself |
| `DOC_BACKEND_QUIC` | `backend:4433` | DOC's plugin host |
| `DOC_PLUGIN_BIND` | `0.0.0.0:4440` | Where the plugin listens (UDP) |
| `DOC_PLUGIN_ADVERTISE` | `$HOSTNAME:<port>` | Where DOC should reach it |
