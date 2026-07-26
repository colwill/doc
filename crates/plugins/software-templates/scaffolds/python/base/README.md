# {{ values.name }}

{{ values.description }}

Created from DOC's **{{ template.title }}** template: {{ scaffold.language }}, {{ scaffold.app }}.
It is wired to this platform before you touch it — telemetry goes to the instance's collector, and
its feature flags and runtime configuration come from DOC.

**{{ lifecycle.title }}.** {{ lifecycle.about }}

That is what the Catalogue says about it, and `doc.lifecycle={{ lifecycle.name }}` is on every
trace, metric and log it sends. When that changes, change it in the Catalogue.

## Running it

```sh
python -m venv .venv && . .venv/bin/activate
pip install -e ".[dev]"
cp .env.example .env     # the platform's settings are already in it
python -m {{ scaffold.package }}
```

## Telemetry

Traces and metrics are exported to `{{ telemetry.endpoint }}` over `{{ telemetry.protocol }}`, as
`{{ values.name }}` in `{{ telemetry.environment }}`. Nothing in the code names the collector: it is
read from `OTEL_EXPORTER_OTLP_ENDPOINT`, so pointing this somewhere else is an environment variable
and not a change. `src/{{ scaffold.package }}/runtime/telemetry.py` is where it is set up.

## Feature flags and runtime configuration

`src/{{ scaffold.package }}/runtime/flags.py` reads every flag and setting that applies to `{{ values.name }}` from
DOC, in one call, and keeps them up to date while the service runs:

```python
if flags.boolean("new-pricing", False):
    ...
limit = flags.number("page-size", 50)
```

Every read names the value to fall back to, so the service keeps running on its own defaults when
the platform cannot be reached. Flags are changed in DOC under **Platform → Feature flags**, and
take effect within `refresh_seconds` without a deployment.

Give the {{ scaffold.noun }} an account to read them with:

1. In DOC, **Service accounts** → new account named `{{ values.name }}`, and issue it a token.
2. Grant it `plugin:flags:service:ro`.
3. Put the token in `DOC_FLAGS_TOKEN`{% if scaffold.app != 'cli' %} (the Kubernetes manifests read
   it from the `{{ values.name }}-doc` secret){% endif %}.

{% if scaffold.app == 'cli' %}## Installing it

```sh
uv tool install .        # or: pip install .
{{ values.name }} --help
```
{% else %}## Deploying it

```sh
kubectl apply -k deploy/kubernetes
```
{% endif %}
## Where things are

| Path | What it is |
|---|---|
| `src/{{ scaffold.package }}/__main__.py` | The entry point |
| `src/{{ scaffold.package }}/runtime` | Settings, telemetry and flags — the platform's own wiring |
{% if scaffold.app != 'cli' %}| `deploy/kubernetes` | Deployment and kustomization |
{% endif %}