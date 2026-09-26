# Running DOC with Docker Compose

`doc-system` is the platform. Each plugin is a project of its own that joins the same network and
reads the same secrets.

## Bring it up

```sh
cd deploy/compose

# The platform: its network and secrets volume are made here, so this goes first.
docker compose -f doc-system/compose.yaml --profile bootstrap run --rm --build bootstrap
docker compose -f doc-system/compose.yaml up -d --build --wait

# Then whichever plugins you want. `rbac` first: every other plugin's routes are checked
# against the permissions it provides.
docker compose -f plugins/rbac/compose.yaml up -d --build
docker compose -f plugins/local/compose.yaml up -d --build
docker compose -f plugins/resources/compose.yaml up -d --build
```

Everything under `plugins/` is optional and independent. To run the lot:

```sh
for plugin in plugins/*/compose.yaml; do docker compose -f "$plugin" up -d --build; done
```

The first sign-in is written to the secrets volume as `first-sign-in`:

```sh
docker compose -f doc-system/compose.yaml run --rm --entrypoint cat backend /secrets/first-sign-in
```

## One plugin at a time

That is the point of the arrangement. A plugin is rebuilt and restarted on its own:

```sh
docker compose -f plugins/kb/compose.yaml up -d --build
```

and stopped without disturbing anything else:

```sh
docker compose -f plugins/kb/compose.yaml down
```

The platform keeps serving; the plugin's pages say it is not running until it is back.

## Before this goes anywhere real

- **Every password in `doc-system/compose.yaml` is `dev-only-change-me`.** Put real ones in a
  `.env` beside it, which Compose reads on its own.
- `DOC_PUBLIC_URL` is where people reach DOC. It is also what makes a plugin's own name work
  (`rbac.rundoc.sh` → `/p/rbac/`), so it has to be the real host.
- Only the frontend, the backend's API and Grafana publish ports, and all three bind to
  `127.0.0.1` by default. Put a reverse proxy with TLS in front rather than widening them.
- A plugin added to `config/doc.toml` needs the bootstrap run again before it can register.

## Adding a plugin

Copy the nearest `plugins/<id>/compose.yaml`, change the name, the `BIN` build argument, the image
and `DOC_PLUGIN_ADVERTISE`, add the plugin's ID to `config/doc.toml`, and run the bootstrap again
so it gets a registration token.
