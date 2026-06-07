# DOC plugin specification

**Protocol version:** v1 · **Last updated:** 2026-09-30

This is the contract between DOC and a plugin. It covers what a plugin declares, which calls it must
answer, which calls it can make, and how it presents data in the DOC user interface. A plugin can be
written in any language that can serve and dial HTTP/3. There are two supported routes:

- **Rust:** use the `doc-plugin-sdk` crate, which implements everything in #5–#7 for you. You write
  the plugin's logic (#12).
- **Go:** start from the **New DOC plugin** template, whose `internal/doc` package implements #5–#9,
  or implement the protocol directly from [`go-hello`](crates/core/plugin-sdk/examples/go-hello/main.go),
  a complete single-file reference you can copy (#13).

Everything below is protocol v1 as the backend implements it today. Plugin data is declared in the
manifest and reached through the data API (#4.3, #9.1); there is no SQL. Anything designed but not
built yet is marked in the text.

## Contents

1. [How a plugin fits in](#1-how-a-plugin-fits-in)
2. [Getting a plugin ID](#2-getting-a-plugin-id)
3. [Lifecycle](#3-lifecycle)
4. [The manifest](#4-the-manifest)
5. [Transport and security](#5-transport-and-security)
6. [Registration and liveness](#6-registration-and-liveness)
7. [Calls the backend makes to a plugin](#7-calls-the-backend-makes-to-a-plugin)
8. [Routes, callers and permissions](#8-routes-callers-and-permissions)
9. [The backend API](#9-the-backend-api)
10. [Errors](#10-errors)
11. [Building the UI](#11-building-the-ui)
12. [Writing a plugin in Rust](#12-writing-a-plugin-in-rust)
13. [Writing a plugin in Go](#13-writing-a-plugin-in-go)
14. [Conformance checklist](#14-conformance-checklist)
15. [Rules and limits](#15-rules-and-limits)

## 1. How a plugin fits in

A plugin is a separate process, usually its own container. It never connects to Postgres or to the
buses directly: the backend is its only peer, and every storage or bus operation goes through the
backend API. There are two HTTP/3 connections, one in each direction, because in HTTP/3 only the
client can start a request.

```text
                        /plugin/v1/*  (register, liveness, backend API)
   ┌──────────┐  ─────────────────────────────────────────────▶  ┌──────────┐
   │  plugin  │                                                  │ backend  │──▶ Postgres, Event Bus,
   │ UDP 4440 │  ◀─────────────────────────────────────────────  │ UDP 4433 │    Service Bus, Cache Bus
   └──────────┘   /host/v1/*  (lifecycle, routes, events)        └──────────┘
                                                                      ▲
                                        browsers, CLI, API clients ───┘  /api/v1/plugins/{id}/…
```

What happens during a plugin's life:

1. The plugin starts its HTTP/3 endpoint, then **registers** with its token and manifest.
2. The backend gives it an **instance secret**. From then on the backend authenticates its calls to
   the plugin with that secret.
3. The backend creates or updates the storage for the plugin's declared **data collections**, then
   calls **`load`**. The plugin is now `running`.
4. The backend **forwards** API and UI requests to the plugin, calls **`run`** according to the
   plugin's classification and delivers **events** the plugin subscribed to. The plugin calls the
   **backend API** to read and write its data, and to use events, the Service Bus, the cache, tasks,
   state and the audit log.
5. The plugin reports **liveness** every few seconds. When a new version registers, the backend calls
   `unload` on the old one, passes the state it returns to the new one's `load`, then tells the old
   one to **exit**.

## 2. Getting a plugin ID

A plugin's ID names everything that belongs to it: its token, certificate, data collections,
topics, cache namespace and permissions. A DOC operator issues the ID. Plugins cannot create their own.

**The operator:**

1. Adds the ID to `plugins.ids` in `config/doc.toml`.
2. Adds the ID to `[plugins.capabilities]`, if the plugin needs a capability (#4.2).
3. Puts the ID in one of the `[[plugins.categories]]`, which sort the Plugins page into sections
   and give a plugin its category's pill elsewhere; a plugin in none is listed under Other.
4. Runs the bootstrap job again, which is safe to repeat. It creates what is missing and leaves the
   rest alone:
   - Docker: `docker compose -f crates/core/docker-compose.yaml --profile bootstrap run --rm bootstrap`
   - Local development: `DOC_CONFIG=config/doc.toml DOC_SECRETS_DIR=secrets cargo run -p doc-backend -- bootstrap`
5. Restarts the backend, so that it loads the new token.

**The plugin then finds in the secrets directory** (mounted read-only at `/secrets` in containers):

| File | What it is |
|---|---|
| `ca/ca.pem` | The bootstrap CA. Trust **only** this when dialling the backend |
| `certs/plugin-<id>.pem` | The plugin's certificate (ECDSA P-256). Its SAN is `plugin-<id>` |
| `certs/plugin-<id>.key` | The certificate's private key (PKCS#8 PEM) |
| `tokens/plugins/<id>.token` | The registration token. It is bound to this ID and must never be logged |

**ID rules:** `^[a-z][a-z0-9-]{0,31}$`. These IDs are reserved: `core`, `backend`, `frontend`,
`workers`, `platform` and `plugin`.

## 3. Lifecycle

### 3.1 States

| State | Meaning | Serves routes | Accepts `run` | Can become |
|---|---|---|---|---|
| `loading` | Registered; data changes and `load` in progress | No | No | `running`, `error` |
| `running` | Loaded and active | Yes | Yes | `cancelled`, `unloading`, `error` |
| `cancelled` | Work stopped by an operator or by the plugin | Yes | No, until resumed | `running`, `unloading`, `error` |
| `unloading` | `unload` in progress | No | No | removed, `error` |
| `error` | `load` or `unload` failed, a long-running `run` ended, the process stopped responding, or the plugin reported an error | No | No | `loading`, `unloading` |

There is no "unloaded" state: an unloaded plugin leaves the registry. A single failed request or
`run` fails only that call; it does not put the plugin in `error`.

### 3.2 Classifications

The classification says when the backend calls `run`. `handle` (routes) always runs synchronously
within a deadline, and `on_event` always runs from a queue, whatever the classification.

| Classification | When `run` is called | Deadline | Example |
|---|---|---|---|
| `synchronous` | On demand, while the caller waits | Request deadline (30 s) | Rendering a service map |
| `async` | On demand, in the background. The caller gets `202` and a task ID. Completion publishes `plugin.<id>.run.completed` | 30 s per attempt, at most | Importing one batch of documents |
| `one-shot` | Once after each `load`, as a background task. It must return | 10 min, at most | Checking an OAuth configuration |
| `long-running` | Once after `load` and after each resume. It runs until `cancel` or `unload` | None | Consuming a trigger queue |

If a long-running `run` returns or fails while the plugin is still `running`, the plugin goes to
`error`.

The async and one-shot limits are maximums. An operator can lower them in `config/doc.toml`
(`plugins.async_deadline_s` and `plugins.one_shot_deadline_s`) but not raise them. **Work that takes
longer than 30 s does not fit in one async run.** Split it into tasks of one batch each, queued with
`POST /plugin/v1/tasks` (#9.5), with each run queueing the next and keeping its place in `state`.
Alternatively, make the plugin `long-running`.

## 4. The manifest

The manifest is JSON, sent in the registration body. Unknown fields are ignored, and every field
except `id`, `version` and `classification` may be left out.

```json
{
  "id": "hello",
  "version": "1.2.0",
  "classification": "synchronous",
  "custom_permissions": [
    { "kind": "plugin-user", "name": "greetings", "description": "Writing greetings" },
    { "kind": "plugin-service", "name": "greetings", "description": "The same, for a service account" }
  ],
  "nav": [{ "label": "Hello", "path": "/", "description": "An example plugin that greets you" }],
  "capabilities": [],
  "public_routes": [],
  "read_routes": [],
  "resource_panels": [{ "resource": "service", "label": "Greetings", "path": "/panels/service", "order": 10 }],
  "insights": [{ "resource": "service", "id": "greeted", "label": "Greetings sent", "path": "/insight/greeted",
                 "description": "Over the last 30 days" }],
  "subscriptions": ["platform.plugin.>", "plugin.hello.run.completed"],
  "schedules": [{ "name": "nightly", "cron": "0 2 * * *", "description": "Greets everyone", "feature": "nightly-greetings" }],
  "settings": [
    { "key": "greeting", "label": "Greeting", "kind": "text", "default": "Hello", "group": "Wording" },
    { "key": "api-token", "label": "API token", "kind": "secret", "required": true, "group": "Credentials" }
  ],
  "features": [
    { "name": "nightly-greetings", "label": "Nightly greetings", "description": "Greets everyone at 2am", "default": false }
  ],
  "data": {
    "collections": {
      "greetings": {
        "fields": {
          "id":   { "type": "uuid", "key": true },
          "name": { "type": "text", "required": true, "max": 200 },
          "by":   { "type": "ref", "to": "core.users" },
          "at":   { "type": "timestamp", "required": true, "default": "now" }
        },
        "indexes": [["by", "at"]],
        "search": ["name"],
        "export": { "read": ["kb"], "fields": ["id", "name", "at"] }
      }
    }
  }
}
```

### 4.1 Fields

| Field | Type | Rules |
|---|---|---|
| `id` | string | Must match the ID the token was issued for (#2) |
| `version` | string | `major.minor.patch`, with optional `-pre` and `+build`. Versions are compared for equality, never ordered. A version the backend has already seen must come with the same binary hash (#6.1) |
| `classification` | string | `synchronous`, `async`, `one-shot` or `long-running` |
| `custom_permissions` | array | `kind` is `plugin-user` or `plugin-service`. `name` matches `^[a-z][a-z0-9-]{0,31}$`. They become grantable as `plugin:<id>:pluginuser:<name>` and `plugin:<id>:pluginservice:<name>` (#8.3). The optional `description`, one line, is what the plugin's Permissions tab says each one allows (#4.4) |
| `nav` | array | Navigation entries. `path` is relative to the plugin's UI root, so `/` is the plugin's `ui/` route. Shown only to callers who can read the plugin. Platform admins can rename, reorder, group or hide them, so treat the label as a default. The optional `description`, one sentence, is shown on the entry's card on the landing page. The optional `group` is the menu it sits in: core's own bar menus are `Workspace`, `Platform`, `Access`, `Watercooler` and `Admin` (that last one for platform administrators only), and `Help` is the footer rather than the bar. Naming one of those joins it, any other name makes a menu of its own, and an entry naming none stands on its own in the bar |
| `capabilities` | array | `permission-provider`, `identity-provider`, `team-provider`, `team-writer`, `offboarding`, `token-issuer`, `secret-store`, `telemetry-sink`, `service-account` or `public-routes`. Each is refused unless the operator has allowed it (#4.2) |
| `service_account` | object | Only with `service-account`: `name` (up to 64 of `a-z`, `0-9` and `-`) and a one-line `description` of the service account the plugin acts as when it asks another plugin's `api/` as itself (#9.3) |
| `public_routes` | array | Routes under `public/` that need no sign-in, such as `callback` or `hooks/*` (`/*` covers everything below). Only allowed with `public-routes` |
| `read_routes` | array | Routes under `api/` whose `POST` only reads, such as `mcp` or `search/*` (`/*` covers everything below), or under `ui/` when written `ui/…`. Core checks a `POST` to one as a read, so `ro` is enough and `wo` is not. Declare only routes that read, or that change only the caller's own settings, such as an MCP endpoint or an RSVP |
| `resource_panels` | array | Panels this plugin adds to Resource Definitions' resource pages. `resource` is a kind as a URL names it (`service`, `team`, `cloud-resource`, …). `path` is relative to the plugin's UI root, and is loaded by HTMX with `?resource=Service:card-gateway`. `order` is where it sits among the page's panels, lowest first and 0 when it says nothing, with equal ones in the order of their plugins. Shown only to callers who can read the plugin |
| `insights` | array | Single measured facts about a resource, small enough to read at a glance, which somebody may **pin** to the top of its page. A panel is everything this plugin has to say about a resource; an insight is one number out of it. `id` names it within the plugin, and with the plugin's id (`dora:frequency`) is how a pin names it. `label` is read away from the plugin that offers it, so it says what it measures — "Deployment frequency", not "DORA" — and `description` is a sentence for whoever is choosing. `path` is loaded with `?resource=…` exactly as a panel's is, and answers the inside of a `doc-stat` (#11.3): a `doc-stat__label`, a `doc-stat__value` and a `doc-stat__note`. Put the link on the label, not the value: the number is what the row is for. Declaring one grants nothing, and a viewer who cannot read the plugin is offered none of its insights |
| `subscriptions` | array | Event Bus topic filters to deliver to the plugin (#7.6). Segments are `[a-z0-9-]+`. `*` matches one segment, and `>` (last segment only) matches the rest |
| `schedules` | array | Up to 16 cron schedules, five fields in UTC. `name` is up to 32 of `a-z`, `0-9` and `-`. Each time one comes round, a background task calls `run` with `{"schedule": "<name>"}`, as the plugin itself. They appear in `/api/v1/cron` as `plugin.<id>.<name>`, where they can be paused, and a version that drops one removes it. The optional `feature` names the feature it belongs to, and the schedule is not recorded at all while that feature is off (#4.4). The cron that starts them is in the workers, so a deployment without them has no schedules coming round at all |
| `data` | object | The plugin's data collections, applied before `load` (#4.3). There is no SQL: the backend creates and changes the storage |
| `settings` | array | What the plugin is configured with, which the platform renders as its Settings page and stores for it, secrets encrypted (#4.4) |
| `features` | array | Named switches on what the plugin does, which the platform shows as its Features tab (#4.4) |
| `named_secrets` | object | For a plugin whose credentials are named by its own data: `label` and `hint` for the section an administrator adds them in, by name (#4.4) |
| `sign_in` | object | An identity provider's sign-in, as the sign-in page offers it: `title`, such as `GitHub`, and `kind`, `redirect` (off to the provider and back) or `password` (a form on the sign-in page). The optional `feature` and `setting` let what is offered, and what it is called, follow the plugin's settings (#4.4). A plugin that serves one provider of a kind many deployments have, such as OpenID Connect, may take its ID from its configuration and run once per provider, each instance its own plugin with its own ID, settings, routes and sign-in (#9.9). See #9.9 |
| `linked_accounts` | array | Providers a caller needs an account with, linked to their DOC user, for what the plugin does as them, such as `github`. After their first sign-in, people are offered a link for each of these, and each of the plugin's pages offers any the viewer has not linked. The caller's linked accounts arrive as `linked` (#8.2), and the plugin decides what to do without one: say so, and never fail outright |

### 4.2 Capabilities

A capability gives a plugin power over the whole platform. That is why the operator allows it in
configuration rather than the plugin asking for it:

```toml
[plugins.capabilities]
github = ["identity-provider", "public-routes"]
```

| Capability | Allows |
|---|---|
| `permission-provider` | Deciding permissions for the platform (the RBAC plugin) |
| `identity-provider` | Signing people in, linking accounts to them and making users for its own accounts (#9.9) |
| `team-provider` | Keeping its own teams and their members in core, and making users for its own accounts (#9.10) |
| `team-writer` | Making and changing the organisations and teams that were made in DOC, for a plugin that shows them, such as the Catalogue (#9.12), and adding somebody by email address for the person it acts for (#9.18) |
| `offboarding` | Acting on somebody who has left: disabling them, taking accounts and team memberships away, and stopping their tokens (#9.11) |
| `public-routes` | Serving the `public_routes` it declares without sign-in |
| `token-issuer` | Minting a scoped token for the person a call is for, and revoking the ones it minted (#9.16) |
| `secret-store` | Keeping the platform's credentials: having core seal and open values under the settings key, answering for the secrets other plugins' settings point at, and saying when they change (#9.17). One plugin holds it: Secret Storage |
| `telemetry-sink` | Taking DOC's telemetry somewhere that keeps it — Grafana, InfluxDB, whatever an organisation already runs. While one is registered the week DOC keeps may be raised (#9.19) |
| `service-account` | A service account of its own, named by its manifest's `service_account`, which it is checked as when it asks another plugin's `api/` as itself, rather than holding nothing (#9.3). Core makes it, owned by the platform, the first time the plugin registers, and never claims one somebody else made under that name. It starts with no permissions: administrators grant it some in RBAC like any service account, and may disable it. No access token is ever issued for it, so only the plugin acts as it. Agent Smith holds it, as `agent-smith`, for runbooks run by schedules, events and automations |

### 4.3 Data

A plugin declares the data it keeps, and the backend decides how to store it. Plugins never send
SQL and never hold database credentials. They read and write through the data API (#9.1).

#### Collections

`data.collections` maps each collection's name to its declaration. A collection's name matches
`^[a-z][a-z0-9-]{0,31}$`. A plugin can have up to 64 collections.

| Key | Meaning |
|---|---|
| `fields` | The fields, by name (below). Up to 64 per collection |
| `indexes` | Lists of fields to index together, in order, such as `[["by", "at"]]`. Up to 16. Sorting needs an index (#9.1) |
| `unique` | Lists of fields whose values must be unique together, such as `[["source", "external_id"]]`. These are the targets for `upsert` |
| `search` | Text fields to include in the collection's full-text index |
| `export` | Who else may read the collection (below) |
| `deprecated` | `true` in the version before the one that removes the collection (below) |

#### Fields

A field's name matches `^[a-z][a-z0-9_]{0,31}$`. Names starting with `_` are reserved for the fields
every record gets automatically:

| System field | Meaning |
|---|---|
| `_version` | Starts at 1 and goes up by one with each update. Pass it back on `update` or `delete` to detect concurrent changes |
| `_created_at`, `_updated_at` | Timestamps kept by the backend |

| `type` | JSON value | Options |
|---|---|---|
| `text` | string | `max` (characters), `one_of` (allowed values) |
| `integer` | number, 64-bit | `min`, `max` |
| `number` | number, 64-bit float | `min`, `max` |
| `decimal` | string, so no precision is lost | `scale` |
| `boolean` | `true` or `false` | |
| `timestamp` | RFC 3339 string | |
| `date` | `YYYY-MM-DD` string | |
| `uuid` | string | |
| `json` | any JSON | |
| `bytes` | base64 string | `max` (bytes) |
| `ref` | the referenced record's key | `to`: one of this plugin's collections, or `core.users`, `core.service-accounts` or `core.plugins`. `on_delete` (own collections only): `restrict` (the default), `cascade` or `null` |
| `list` | array | `of`: `text`, `integer` or `uuid` |

Every field can also have:
- `required: true`, so the field can never be null
- `default`: a literal value, `"now"` for a timestamp or `"uuid"` for a UUID
- `description`: shown to operators

**Keys:** exactly one field has `"key": true`, and it is either a `uuid` or a `text` field. A `uuid`
key that an insert leaves out is generated as a UUIDv7. A `text` key must always be supplied.

**References** to a plugin's own collections are enforced by the database. References to `core.*`
records are checked when a record is written, and are not updated if the core record changes later.
To point at another plugin's records, store their key in a `uuid` or `text` field. The data API won't
check it.

#### Exports

A collection is private to its plugin unless it has an `export`:

```json
"export": { "read": ["kb", "service-map"], "fields": ["id", "name", "at"] }
```

- `read` lists the plugins that may read the collection, or is `"all"`.
- `fields` limits what those plugins see. Leave it out to export every field.
- Exports are **read-only**. To change another plugin's data, ask that plugin over the Service Bus
  (#9.3).
- **Access is per plugin, not per user.** A reading plugin sees every exported row, whichever user it
  is acting for. So **only export data that any user of the reading plugins may see.** For anything
  user-specific, the reader asks the owner over the Service Bus with the user's context token, and
  the owner checks its own permissions.
- A reader names an exported collection with its owner's ID: `kb.documents`.

#### Core collections

Every plugin can read these, and none can write them:

| Collection | Holds |
|---|---|
| `core.users` | `id`, `provider`, `login`, `organisation_id`, `name`, `email`, `first_name`, `surname`, `disabled`, `first_signed_in_at`, `last_signed_in_at`, `created_at`. A user is a person in exactly one organisation, and can exist before they first sign in. `login` is the name they go by, and the names and email address are the latest their accounts reported. `provider` is the provider of the account they were first known by, or null if they have none |
| `core.identities` | The accounts people sign in with or have linked, one per provider per user: `id`, `user_id`, `provider`, `external_id` (the provider's immutable ID), `login`, `source` (`sign-in`, `link`, `admin` or `provider`), what the provider last reported (`name`, `email`, `first_name`, `surname`, `reported_at`), `created_at`, `last_used_at` |
| `core.service-accounts` | `id`, `name`, `description`, `owner_id` (a user), `owner_team_id` (a team, whose members and whose sub-teams' members manage it), `disabled`, `created_at` |
| `core.organisations` | `id`, `name` (unique, what plugins name it by), `title`, `description`, `created_at` |
| `core.teams` | `id`, `organisation_id`, `parent_id` (a team in the same organisation), `name` (unique in its organisation), `title`, `description`, `email` (a contact address, or empty), `default` (everyone is placed in it as they arrive), `provider` and `external_id` (the plugin that provides it and its key there, or null for a team made in DOC), `lead_id` (the member who leads it, or null), `created_at` |
| `core.team-members` | `id` (`<team_id>:<user_id>`), `team_id`, `user_id`, `source` (`admin`, `default` or `provider`), `provider`, `position` (the name of the position they hold in the team, or null), `created_at`. Members are users, never service accounts |
| `core.positions` | The positions an organisation defines, such as Senior Software Engineer: `id`, `organisation_id`, `name` (unique in its organisation, never changed), `title`, `description`, `responsibilities` (a list), `created_at` |
| `core.team-positions` | Each team's positions as it has them, inherited from its organisation through every team above it and changed on the way: `id` (`<team_id>:<name>`), `team_id`, `name`, `title`, `description`, `responsibilities`, `hidden` (held by nobody in this team), `origin` (`organisation`, or the team that made it its own) |
| `core.plugins` | `id`, `display_name`, `version`, `classification`, `state`, `registered_at`, `last_seen_at`, `mcp` (whether it serves MCP at `api/mcp`, which is a read route called `mcp`), `telemetry` (whether it holds `telemetry-sink`, which is how a plugin keeping telemetry decides how long to keep it — #9.19) |
| `core.plugin-status` | `plugin`, `version`, `state`, `error`, `since`, `last_error`, `last_error_at` |
| `core.plugin-status-history` | Every state each plugin's registrations entered, kept 30 days: `id`, `plugin`, `version`, `state` (a lifecycle state, or `removed`), `error`, `at`. Read from a lower bound on `at` (`gte` or `gt`), at most 31 days at a time |
| `core.status-history` | Every check the status probes made, one a minute for each part of DOC, kept 30 days: `id`, `at`, `kind`, `name` (`postgres`, `eventbus`, `servicebus`, `cachebus`, `backend`, `frontend`), `state` (`up`, `degraded`, `down` or `unknown`), `detail`, `latency_ms`, `nodes`. Read from a lower bound on `at`, at most a day at a time; a read without one is `400` `bad-query` |
| `core.tasks` | The reading plugin's **own** tasks: `id`, `state`, `payload`, `result`, `error`, `attempts`, `created_at`, `finished_at` |
| `core.topics` | Every topic published on this platform, read off the Event Bus: `topic`, `retained` (events it still holds) and `published` (events ever published on it). It is what there is to subscribe to, so a form that asks for a topic can offer it |

#### Changing the declaration

Each version's declaration is compared with that of the version currently serving. During a handover
both versions use the same data, so every change must keep the serving version working:

| Change | Allowed? |
|---|---|
| Add a collection; add a field that isn't `required`, or has a `default`; add an index, `search` field or `one_of` value; raise a `max` | Yes. Applied before the new version's `load` |
| Make a `required` field optional; lower a `min`; drop a `one_of` altogether, so any text is taken; change `export` or `description` | Yes |
| Remove a field or collection | Only if the serving version marks it `"deprecated": true`. Its data is deleted once the handover to the new version has completed |
| Add a `unique` constraint | Yes, but if existing records break it, the plugin goes to `error` and the old version keeps serving |
| Change a type, rename, change the key, add a `required` field without a `default`, or tighten any other rule | **No.** Registration is refused with `400` `incompatible-data` |

To change a field's type or name:
1. Add a new field.
2. Copy the values across in `load` or a task, using `batch` and `upsert`.
3. Mark the old field deprecated.
4. Remove it in the next version.

Because only additive changes are applied before a handover, a failed `load` can always fall back to
the old version.

### 4.4 Settings and features

A plugin declares what it is configured with; **the platform renders the page, checks every value
and stores them, secrets encrypted**.
Nobody edits a deployment to change a sync schedule, and no plugin writes a settings page of its own.

```json
{
  "settings": [
    { "key": "base-url", "label": "Jira URL", "hint": "https://acme.atlassian.net", "kind": "url", "required": true, "group": "Connection" },
    { "key": "api-token", "label": "API token", "kind": "secret", "required": true, "group": "Connection" },
    { "key": "projects", "label": "Projects", "kind": "list", "feature": "sync" },
    { "key": "keep-for", "label": "Keep issues for", "kind": "duration", "default": 2592000, "min": 86400 }
  ],
  "features": [
    { "name": "sync", "label": "Issue sync", "description": "Reads issues every hour", "default": true }
  ]
}
```

| Field | Rules |
|---|---|
| `key` | `^[a-z][a-z0-9-]{0,63}$`, and the plugin's own name for the setting |
| `label`, `hint`, `group` | What the field is called, one sentence under it, and the heading it sits under. Settings with no group come first, and each group keeps the order its first setting appears in |
| `kind` | `text`, `number`, `boolean`, `choice`, `list`, `url`, `duration`, `cron`, `secret` or `map` |
| `default` | What it is worth before anybody sets it |
| `required` | A plugin whose required settings are unset **serves what does not depend on them and says what is missing**, rather than failing (#4.4, below) |
| `min`, `max` | For `number`, for `duration` in seconds, and for the length of `text` and of each `list` line |
| `one_of` | The choices, for `choice` |
| `choices` | A `GET` route under the plugin's own `api/` that offers what can be chosen when the page is drawn, below |
| `pattern` | A regular expression the whole value must match, for `text`, `list` and `url` |
| `feature` | The feature the setting belongs to, so the page groups it with that feature's others |
| `requestable` | For a `list` of plugin IDs: another plugin may ask an administrator to add it (#9.15) |

A **`map`** holds keys, each with a list of values, such as a Jira project and the services it is
for, and is stored as an object of arrays: `{"PAY": ["card-gateway", "ledger"]}`. It is written
anywhere else as `KEY=one,two` entries, one to a line or separated by `;`, which is also what
`DOC_<PLUGIN>_<KEY>` takes; a key written twice is one entry, and a value given to no key is
refused. It holds at most 256 keys and 256 values in all.

**Choices from the plugin.** What can be chosen is often the plugin's data rather than something
fixed in its manifest: the Jira projects it reads, the services in the Catalogue. A setting naming
`choices` is drawn with them. The frontend asks `GET api/<choices>` as whoever is looking, and the
plugin answers:

```json
{
  "choices": [{ "value": "PAY", "label": "Payments (PAY)", "hint": "jira" }],
  "values": [{ "value": "card-gateway", "label": "Card gateway" }],
  "choices_label": "Jira project",
  "values_label": "Services"
}
```

A `choice` is then a select of `choices`, a `list` a box to tick for each, and a `map` a row for
each key held and a few blank ones to add with: a select of `choices` for the key and boxes for
`values`, headed by the two labels. What is held but no longer offered stays chosen and is marked
so, rather than being lost on the next save. Core holds a value to no `choices`, since what is
offered changes; the plugin passes over what it no longer knows. When the route cannot be asked —
the plugin is not running, or the viewer may read its settings but not the plugin — the field is
a box to type into, as `KEY=one,two` lines for a `map`, and the page says why.

A **feature** is a named switch: `name` (`^[a-z][a-z0-9-]{0,31}$`), `label`, `description`,
`default`, and an optional `warning` shown beside it. A plugin checks its own features for its
routes and its work with `backend.feature("sync")`, and three things in the manifest may follow
what is configured rather than being fixed at start-up:

| In the manifest | What follows the settings |
|---|---|
| `schedules[].feature` | The schedule is not recorded at all while that feature is off, so nothing it drives runs |
| `schedules[].setting` | A `cron` setting whose value replaces the declared expression, so how often the work runs is changed in DOC |
| `sign_in.feature` | The provider is not offered on the sign-in page until that feature is on, so a plugin that *can* sign people in once configured is not offered before it is |
| `sign_in.setting` | A `text` setting whose value replaces `title`, so what the page calls the provider is changed in DOC |

A feature may also say what it **needs**: a feature of another plugin it works from, of any one of
several, such as DORA's metrics needing `github`'s or `ghe`'s delivery data:

```json
{ "name": "metrics", "label": "DORA metrics", "needs": [{ "plugins": ["github", "ghe"], "feature": "delivery-data" }] }
```

The plugins page offers such a plugin an **Enable** button while one of these features is off, or
while no named plugin has the feature it needs on. One press turns the feature on, and the needed
feature on every named plugin that is running and has all its required settings
(`POST /api/v1/plugins/<id>/enable`). Changing another plugin's features is changing its settings,
so the caller needs `plugin:<plugin>:settings:rw` for every plugin it touches, and nothing is
switched unless they have all of them. A feature with a `warning` is never turned on this way. When
no named plugin can meet a need, the page says which are wanted instead of offering the button.

That is deliberately the whole of it. Everything else in a manifest — the ID, the capabilities,
the routes, the collections — is fixed for as long as the process runs, because core reads it once
at registration. A plugin declares what it *can* do and lets its settings decide what it *does*.

#### Credentials named at runtime

A plugin whose credentials are named by its own data — one per source, per vendor, per account —
declares `named_secrets: { label, hint }` instead of a secret setting for each. The page then has a
section where an administrator adds them **by name** (`^[a-z0-9][a-z0-9-]{0,63}$`, at most 200),
each kept encrypted exactly as a declared secret is, and the plugin reads one with
`backend.settings().named("confluence")`. Nothing else changes: they are never returned, and only
the plugin that holds them is given their values.

#### Credentials from Secret Storage

Any secret setting, and any credential held by name, can point at a secret in Secret Storage
instead of holding a value. The Settings page offers **Or use a secret from Secret
Storage**, listing the secrets that plugin is shared with; `PUT /api/v1/plugins/<id>/settings`
takes `{"secret": "<id>"}` as a secret setting's value, and `named_from_store` beside
`named_secrets` for credentials by name. What is stored is that reference and never a value.

- Core asks the store for the value whenever the plugin's settings are resolved, and the store
  answers only for a secret shared with that plugin. The plugin reads it with `backend.settings()`
  as it reads any secret, so no plugin has to change.
- A save pointing at a secret the store will not give the plugin is refused against that field.
- When a secret changes, stops being shared or is deleted, the store says so (#9.17), and core
  tells every plugin whose settings point at it that its settings changed.
- A reference the store will not answer, or cannot while it is not running, leaves the setting
  unset rather than falling back to the environment. The page says why. Plugins that could not
  reach the store are told again when it next loads.

#### Where a value comes from

Core resolves each setting in this order, and the first that answers wins:

1. **What an administrator set** on the Settings page, typed in or pointed at Secret Storage.
2. **The environment**: `DOC_<PLUGIN>_<KEY>` (upper case, `-` becomes `_`), or the file that
   `DOC_<PLUGIN>_<KEY>_FILE` names.
3. **The plugin's declared default.**

A variable is where a setting **starts**, not where it is stuck. The page shows the value in the
field with "From `DOC_GITHUB_ORGS` in the deployment. Saving something here replaces it."; once
something is saved it reads "Set here, over `DOC_GITHUB_ORGS` from the deployment. Clear it to use
that again." So a platform can be handed its first configuration by its deployment — Vault,
Kubernetes or Docker secrets included — and still be administered from DOC afterwards, without
anybody editing the deployment to change an organisation list.

What is **not** a plugin setting: the platform's own configuration — the database URLs and the
credentials in them, the secrets directory, the bind addresses, `DOC_PUBLIC_URL` — belongs to the
deployment alone (#5.3). It configures the thing that stores the settings, so it cannot come from
storage.

#### Secrets

This reverses the old rule that credentials never come from the database, and keeps what that rule
protected: **the database only ever holds ciphertext.** A secret is encrypted with AES-256-GCM
under a settings key in the secrets volume (`keys/settings.key`), with the plugin ID and the
setting's key bound in, so a ciphertext cannot be moved to another plugin or another setting.

- **Write-only.** No API, page, export or log ever returns a secret. The page says
  "Set, changed by Ada on 19 Sep 2026", with a box to replace it and a tick to clear it.
- **Only its own plugin reads it**, through `POST /plugin/v1/settings` over its own authenticated
  connection (#9.14), as a `Secret<String>`.
- **Rotating**: `doc-backend rotate-settings-key` writes a new key and re-encrypts every stored
  secret under it. Each value records which key sealed it, so a rotation cut short loses nothing.
- **Losing the key** loses the secrets under it. The page shows them as needing setting again, and
  the settings key must be backed up with the rest of the secrets volume.

#### Who may see and change them

A permission of its own, `plugin:<plugin>:settings[:<scope>]`, for users and service accounts alike
and grantable through RBAC roles like any other. `ro` sees the Settings and Features tabs, `rw`
changes them, and a platform administrator passes. **Holding `plugin:<plugin>:user:rw` does not
grant it**: using a plugin and configuring it are different jobs. Every change is audited as
`plugin.settings.changed` or `plugin.features.changed`, with the keys and their values — a secret
recorded only as `changed`.

#### The page

`/plugins/<id>` is tabs: **Overview**, **Settings**, **Features**, **Permissions** and
**Registration tokens**. Each is shown only to those who may see it, and each route refuses anyone
else on its own, so hiding a link is never what keeps anybody out. The Settings tab has a **Test
connection** button, which asks the plugin what it thinks of what is in the boxes without storing
anything (#7.9).

#### Changing the declaration

Settings follow the rules data does (#4.3): add freely, and a type change is refused. Values belong
to the plugin rather than to a version, so a hot reload keeps them and a new version's new settings
start at their defaults.

## 5. Transport and security

### 5.1 QUIC and HTTP/3

| Setting | Value |
|---|---|
| Protocol | QUIC v1 + HTTP/3, ALPN `h3` |
| TLS | 1.3 only |
| Idle timeout | 3 s |
| Keep-alive | **Required on every client connection**: send an ack-eliciting packet at least once a second. Without it, idle connections close after 3 s and failures are detected slowly |
| Windows | 8 MiB per stream, 64 MiB per connection; up to 4,096 concurrent bidirectional streams |
| Header section | Accept at least 64 KiB of headers, because `x-doc-caller` can be large |
| Connect timeout | Wrap each connection attempt in a 1 s timeout when retrying. QUIC has no equivalent of a TCP reset, so dialling a dead address only fails at the idle timeout |

Keep internal QUIC ports unpublished in Docker. Containers reach each other over the `doc` network.

### 5.2 Certificates and authentication

- **Plugin → backend:** the plugin verifies the backend's certificate against `ca/ca.pem` with the
  server name `backend`. It authenticates itself with `authorization: Bearer <registration token>`
  on **every** call, including registration and liveness.
- **Backend → plugin:** the plugin serves with `certs/plugin-<id>.pem` and its key. The backend dials
  with the server name `plugin-<id>` and verifies it against the CA, whatever address the plugin
  advertised. A process holding another plugin's token but not that plugin's key cannot pose as it.
- The backend authenticates its calls with `authorization: Bearer <instance secret>`, using the
  secret from the registration response. The plugin **must** refuse any `/host/v1/*` call without the
  current secret (#7.1).
- Mutual TLS is not used in v1.

### 5.3 Configuration

The SDK and the Go reference both read these environment variables. Use the same names so an
operator can deploy any plugin the same way.

| Variable | Default | Meaning |
|---|---|---|
| `DOC_SECRETS_DIR` | `/secrets` | The secrets directory from #2 |
| `DOC_PLUGIN_TOKEN` | contents of `tokens/plugins/<id>.token` | Registration token override |
| `DOC_BACKEND_QUIC` | `backend:4433` | The backend's QUIC address |
| `DOC_PLUGIN_BIND` | `0.0.0.0:4440` | The UDP address this plugin listens on |
| `DOC_PLUGIN_ADVERTISE` | `$HOSTNAME:<bind port>` | The `host:port` the backend dials. The container hostname makes two versions of a plugin, running side by side during a handover, distinct |

### 5.4 What the platform calls itself

Some things are the whole platform's rather than one service's, and an administrator sets them in
**Admin → Settings** rather than in a file. `GET /api/v1/settings` answers `{"instance_name"}` to
anyone signed in, and `PUT` takes it from a platform administrator (`plugin:core:user:rw`); it is
audited as `settings.changed` and announced on `platform.settings.changed`, so every frontend drops
what it cached. The settings also travel with each caller's access summary, so a page names the
platform without another call. A service's configured `[instance] name` is what is used until one
is set here.

### 5.5 Setting the platform up

**Admin → Set up DOC** is the frontend's guided first run, and core keeps only how far it has got,
in one row (`core.setup`): when it was first opened, the tools the organisation said it uses, the
steps marked done, and when it was finished and by whom. `GET /api/v1/setup` answers it and
`PATCH /api/v1/setup` changes it — `{"started": true}`, `{"step": "<name>"}`, `{"tools": [...]}`
(replacing those given before) or `{"finished": true|false}` — for platform administrators only;
names are lowercase letters, digits and dashes, at most 24 of each. Finishing is audited as
`setup.finished`, offering it again as `setup.reopened`. Each step's own work goes through the API
it belongs to: the settings above, plugin switches and features, organisations and plugin settings.
An administrator's first sign-in opens it while nobody has, and the home page reminds
administrators of it until it is finished.

## 6. Registration and liveness

### 6.1 `POST /plugin/v1/register`

The plugin sends this once its endpoint is listening:

```json
{
  "manifest": { "id": "hello", "version": "1.2.0", "classification": "synchronous" },
  "address": "doc-hello-1717:4440",
  "binary_sha256": "9c1e…64 lowercase hex characters…",
  "started_at": "2026-09-18T09:14:07Z"
}
```

- `binary_sha256` is the SHA-256 of the running executable, read at start-up
  (`os.Executable()` in Go, `std::env::current_exe()` in Rust). A version the backend has seen
  before, arriving with a different hash, is refused with `409`, so **bump the version for every
  build you deploy**. Development backends set `plugins.allow_rebuilds` to relax this rule.
- If `address` is empty, the backend uses the address the request came from.

A successful registration returns **`200`**:

```json
{
  "secret": "k2V…43 characters…",
  "previous": null,
  "state": "loading",
  "liveness_ms": 5000,
  "instance": "0192b0c4-7e1a-7c3e-9f8e-3b1f2a9c4d10"
}
```

| Field | Meaning |
|---|---|
| `secret` | The instance secret (#5.2). Store it **before doing anything else**, because the backend calls `/host/v1/load` straight away. Never log it |
| `previous` | The state the last `unload` of any version returned, or `null`. The same value arrives in `load` |
| `state` | Always `loading` |
| `liveness_ms` | How often to report liveness |
| `instance` | This registration's ID. Send it with every liveness report |

| Refusal | Status | `type` |
|---|---|---|
| No bearer token, or a token that isn't usable | 401 | `unauthorized` |
| A usable token that isn't a plugin token | 403 | `forbidden` |
| A token for a different plugin ID | 403 | `wrong-token` |
| Reserved ID / capability not allowed / public routes without the capability | 403 | `reserved-id`, `capability-refused`, `public-routes-refused` |
| Malformed ID, version, permission name, topic filter or schedule | 400 | `bad-id`, `bad-version`, `bad-permission`, `bad-subscription`, `bad-schedule` |
| A data declaration that is malformed, or incompatible with the serving version (#4.3) | 400 | `bad-data`, `incompatible-data` |
| Known version, different binary | 409 | `hash-conflict` |
| A handover of this plugin already in progress, or storage unavailable | 503 | `handover-in-progress`, `storage-unavailable` |

**Retry with backoff** from 500 ms, doubling up to 15 s, for as long as the backend doesn't answer.
A plugin started before the backend is normal. `400`, `401`, `403` and `409` won't succeed on retry
until an operator acts, so log them prominently.

**What happens after `200`:**

- If no other version of the plugin is serving, the backend applies the plugin's data declaration
  (#4.3), then calls `load`. If the plugin answers `503` with an empty body
  (#7.1), the backend retries `load` for about 2 s.
- If another version is serving, the backend runs a **handover**:
  1. It applies the additive part of the new version's data declaration.
  2. It pauses routing. Requests wait up to 5 s before getting `503`.
  3. It lets in-flight requests finish, for up to 10 s, then calls `unload` on the old version.
  4. It calls `load` on the new version with the old version's state.
  5. It resumes routing to the new version and tells the old one to exit.
  6. Only then does it delete the data of any fields and collections that were deprecated and have
     now been removed.

  If the new `load` fails, the old version is loaded again and keeps serving.

### 6.2 `POST /plugin/v1/liveness`

Send this every `liveness_ms` milliseconds:

```json
{ "id": "hello", "state": "running", "error": null, "instance": "0192b0c4-…" }
```

`state` is the plugin's own view of its state, and `error` is the last error it noticed itself, or
`null`.

| Answer | Meaning | What the plugin does |
|---|---|---|
| `204` | Recorded | Nothing |
| `404` `not-registered` | The backend has no record of this plugin, for example because it restarted | Forget the secret and register again. If the plugin is `unloading`, exit instead |
| `410` `superseded` | A newer registration replaced this process | Exit with code 0 |
| No answer | The backend is unreachable | Keep reporting |

If no report arrives for 20 s, the backend marks the plugin `error` ("unreachable"). It checks
every 10 s, so this happens 20–30 s after the last report. Requests to the plugin then get `503`
straight away. Missed reports are the only signal the backend uses: the plugin dials the backend,
not the other way round.

## 7. Calls the backend makes to a plugin

### 7.1 Rules for every `/host/v1/*` call

1. Answer paths outside `/host/v1/` with `404`.
2. Check `authorization: Bearer <instance secret>` with a constant-time comparison. **Without a stored
   secret, or with the wrong one, answer `503` with an empty body.** The backend reads that as "not
   ready yet" and retries. Nothing else may drive a process that hasn't registered.
3. Limit how many calls are handled at once. The SDK allows 32, and answers `429` beyond that.
4. If `x-doc-deadline-ms` is present, cancel the work when that many milliseconds have passed and
   answer `504`. No header means no deadline, as for a long-running `run`.
5. Handle each call so that a panic or crash fails **only that call**. Answer `500` with a problem
   (#10). A panic in `load` or `unload` also puts the plugin in `error`.

Every call is a `POST`, except forwarded requests (#7.8), which keep the caller's method.

| Header | On | Meaning |
|---|---|---|
| `authorization` | all | `Bearer <instance secret>` |
| `x-doc-context` | all except `health` and `exit` | A short-lived context token. Pass it back on backend API calls made while handling this call (#9) |
| `x-doc-caller` | `run`, forwarded requests | JSON describing who is calling (#8.2) |
| `x-doc-deadline-ms` | all except long-running `run` | Milliseconds left |
| `traceparent` | all except `health`, while the backend is tracing | The W3C trace context of the backend's span for this call |

Continue the trace from `traceparent`: start the span for the call as its child, and send the
current span's `traceparent` on every backend API call made while handling it (#9). The SDK does
both, names each span `<plugin> <function>`, tags it with the plugin's ID, version and function, and
exports traces, metrics and logs over OTLP/HTTP as `doc-plugin-<id>` when
`OTEL_EXPORTER_OTLP_ENDPOINT` is set. `telemetry::external` counts a plugin's calls to outside
services as `doc.external.calls`, which is what the external APIs dashboard shows.

### 7.2 `/host/v1/health`

Body: `null`. Deadline: 5 s. Answer: `{"id": "hello", "version": "1.2.0", "state": "running"}`.

### 7.3 `/host/v1/load`

Body: `{"previous": <any JSON or null>}`. Deadline: 60 s.

Restore from `previous` if it is set, set up anything the plugin needs, and answer `2xx`. The body
may be `{"state": "running"}` or empty. Any error answer puts the plugin in `error`, with the
problem's `detail` shown as the reason. Wait for any `run` still in progress to finish before
reloading.

### 7.4 `/host/v1/unload`

Body: `null`. Deadline: 30 s.

Stop work, asking any `run` in progress to stop, and answer `{"state": <any JSON or null>}`. That
value is saved and handed to the next `load`. Keep it under 1 MiB. **Do not exit yet**: if the new
version fails to load, the backend loads this one again. Wait for `exit` or a `410` on liveness.

### 7.5 `/host/v1/run` and `/host/v1/cancel`

`run` body:

```json
{ "task": "0192b0c5-…", "payload": { "name": "Ada" } }
```

`task` is set when the run comes from a background task (async and one-shot plugins, or
`POST /plugin/v1/tasks`) and is `null` otherwise. Answer `{"payload": <any JSON>}`, or a problem to
fail the run. A task that fails is retried up to its `max_attempts`, so **runs must be idempotent**.

`cancel` has body `null` and a 30 s deadline. It asks a run in progress to stop. Answer `2xx`, for
example `{"state": "cancelled"}`. The plugin then stays `cancelled` until it is resumed.

### 7.6 `/host/v1/event`

The body is one Event Bus envelope. Deadline: 30 s.

```json
{
  "id": "0192b0c6-…",
  "topic": "platform.plugin.state",
  "source": "core.plugins",
  "at": "2026-09-18T09:14:07.123Z",
  "correlation_id": null,
  "schema_version": 1,
  "payload": { "plugin": "kb", "state": "running" }
}
```

- `2xx` acknowledges the event. Anything else, or no answer, is retried after 0.5 s, then 1 s, 2 s
  and so on, up to 60 s between attempts.
- Delivery is **at least once**. Deduplicate by `id`.
- Events are delivered only while the plugin is `running`, and wait while it is paused.
- A new subscription starts from the moment the plugin registers. History is not replayed into it.
- The context token on this call acts as the plugin itself.

### 7.7 `/host/v1/exit`

Answer `204`, then close the endpoint and exit with code 0. Containers run with restart policy
`on-failure`, so a clean exit stays stopped.

### 7.8 `/host/v1/request/{route}`

These are API, UI, public and internal routes forwarded from callers (#8). `{route}` starts with
`api/`, `ui/`, `public/` or `internal/`, with no leading slash, and arrives still percent-encoded.
Use `r.URL.EscapedPath()` in Go. The method, query string and body are the caller's, and bodies are
limited to 16 MiB. Answer `503` with `{"type": "/problems/unavailable", …}` unless the plugin's state
is `running` or `cancelled`.

**Headers passed to the plugin:** `accept`, `accept-language`, `content-type`, `if-match`,
`if-modified-since`, `if-none-match`, `traceparent` (the backend's own while it is tracing, #7.1),
`user-agent`, `x-request-id`, the webhook
signatures `x-doc-signature` and `x-hub-signature-256`, every `hx-*` and every `mcp-*`. Cookies and
credentials are never passed on. A plugin acts through its context token.

**Headers passed back to the caller:** `allow`, `cache-control`, `content-disposition`,
`content-language`, `content-type`, `etag`, `last-modified`, `location`, `vary`, every `hx-*` and
every `mcp-*`. Everything else is dropped, so a plugin cannot set cookies on the platform's origin.

### 7.9 `/host/v1/settings/check`

What the plugin thinks of settings somebody has typed and nothing has stored yet. This is where a
credential is tried against the service it is for, so "the token is refused" lands on the field
rather than in a log an hour later. **Answering is optional**: a plugin that serves no such route answers `404`, and
core takes that as having no objection. There are 10 seconds.

```json
{ "values": { "base-url": "https://acme.atlassian.net" }, "secrets": { "api-token": "…" }, "named": { "prod-eu": "…" }, "features": { "sync": true } }
```

The body is the settings **as they would be if this save went through**, secrets and credentials
held by name all included, so a check is made against what is about to be true rather than what is
true now: a credential being added or replaced is the one tried, and one being removed is left out. Answer:

```json
{ "problems": { "api-token": "Jira refused this token" }, "problem": null, "message": "Signed in as doc-bot" }
```

A `problems` entry puts its message on that field and **nothing is stored**; `problem` is about the
settings as a whole; `message` is shown when all is well. The Settings page's **Test connection**
button makes the same call without saving.

### 7.10 `/host/v1/settings/changed`

Core's word that settings were stored, with the keys and features that changed and never their
values: `{"keys": ["base-url"], "features": ["sync"]}`. The plugin reads the new values back
itself (#9.14). The SDK's default is to **reload the plugin in the same process** — `unload` then
`load`, carrying its state across — so every plugin follows a change without a restart; a plugin
that can do better handles it and says so.

## 8. Routes, callers and permissions

### 8.1 Route kinds

| Caller uses | Reaches the plugin as | Who can call it |
|---|---|---|
| `GET/POST/… /api/v1/plugins/{id}/api/{path}` | `api/{path}`, answering JSON | Signed-in principals with `plugin:{id}:user` or `plugin:{id}:service` at a scope that allows the method. `GET`, `HEAD` and a `POST` to one of the `read_routes` are reads; everything else is a write |
| `… /api/v1/plugins/{id}/ui/{path}` | `ui/{path}`, answering an HTML fragment (#11) | As above |
| `… /api/v1/plugins/{id}/public/{path}` | `public/{path}` | Anyone, but only for routes in `public_routes`, with the `public-routes` capability. Anything else is `404` |
| Service Bus request to `plugin.{id}`, subject `{path}` | `POST internal/{path}`, with a JSON body | Core only. Answer `2xx` with JSON |
| Service Bus request to `plugin.{id}`, subject `api/{path}`, from another plugin (#9.3) | `api/{path}`, as the calling plugin's principal | Whoever that plugin acts for, checked as if they had asked over HTTP |
| Service Bus request to `plugin.{id}`, subject `discovery/{path}`, from another plugin (#9.3) | `discovery/{path}`, with caller `{"kind": "plugin", "id": …}` | Any plugin, acting as itself. Core checks nothing, so the route decides which plugins it serves |

The backend answers some requests itself: `404` for an unknown plugin or route, `401` or `403` when
access is refused, `503` with the plugin's state when the plugin isn't serving, `504` when the
deadline passes, and `413` for a body over 16 MiB. A route containing `.` or `..` segments is
refused.

### 8.2 The caller

`x-doc-caller` is compact JSON. Characters outside ASCII are escaped as `\uXXXX`, which any JSON
parser decodes:

```json
{
  "kind": "user",
  "id": "0192a…",
  "label": "Ada Lovelace",
  "admin": false,
  "scope": "ro",
  "custom": { "greetings": "ro" },
  "attributes": { "team": "payments-core" },
  "linked": { "local": "ada", "github": "ada-l" }
}
```

| `kind` | When |
|---|---|
| `user` | A signed-in person |
| `service` | A service account |
| `plugin` | A plugin acting as itself |
| `platform` | Core, on `internal/*` |
| `anonymous` | Nobody signed in (`public/*`) |

`scope` is the caller's own `user` or `service` scope on this plugin: `ro`, `rw` or `wo`. It is
`null` for `platform` and `anonymous` callers, and for an admin who holds nothing themselves. Core
has already checked it, so a plugin uses it only to leave out what the caller can't do, such as
editing controls for someone who only reads. The Rust SDK's `Backend::writes` does this.

`linked` is a user's linked accounts, each provider to the account's login. A plugin that declares
`linked_accounts` (#4.1) checks it before acting for the caller with one.

`via` names the plugin that relayed the call for whoever it is (#9.3), such as `agent` when Agent
Smith acts for somebody; it is absent when they asked themselves. `guard` is what the plugin that
relayed it limited the call to, and holds for every call made while it is handled:

| `guard` | The call may not change |
|---|---|
| `read-only` | Anything. Core refuses every route that writes with `403` before the plugin sees it |
| `not-production` | Anything in production. `production` lists the environment names that mean production, from the operator's `[environments]` (by default `production`, `prod` and `live`, compared without case) |

**A plugin that keeps things per environment honours `not-production`**: it refuses with `403` a
change in one of those environments, or in every environment at once, and says why. The Rust SDK's
`Caller::may_change(Some(environment))` answers it, with `None` for every environment. Feature
flags does this for its entries and providers; a plugin that keeps nothing per environment has
nothing to check.

### 8.3 Checking custom permissions

Core has already checked `plugin:<id>:user` or `plugin:<id>:service` before forwarding a request.
**Checking the plugin's custom permissions is the plugin's job.** `custom` maps each custom
permission the caller holds to its scope.

| Scope | Allows reads (`GET`, `HEAD`) | Allows writes |
|---|---|---|
| `ro` | Yes | No |
| `rw` | Yes | Yes |
| `wo` | No | Yes |

A caller with `admin: true` passes every check. The check is the same in every language:

```go
func (c *Caller) Allows(permission string, write bool) bool {
	if c == nil { return false }
	if c.Admin { return true }
	switch c.Custom[permission] {
	case "rw": return true
	case "ro": return !write
	case "wo": return write
	}
	return false
}
```

Refuse with `403` and say which permission is needed, for example
`needs plugin:hello:pluginuser:greetings:rw`.

A custom permission that names an ability rather than something to read and change, such as
`infra`'s `selfservice` or `water`'s `hackathon`, is held at any scope: check that `custom` has it.

## 9. The backend API

These are all `POST /plugin/v1/<call>` with a JSON body. Each call carries
`authorization: Bearer <registration token>` and, **while the plugin is handling a backend call**,
that call's `x-doc-context`. A success is `200` with a JSON body. A refusal is a problem (#10). A
plugin that hasn't registered gets `404` `not-registered`.

The backend takes the plugin's identity from its token, never from the body, so none of these calls
can reach another plugin's collections (beyond reading its exports), topics, cache or state.

### 9.1 `data`: reading and writing collections

Every request names an `op` and a `collection`. A plugin's own collections are named plainly
(`greetings`). Another plugin's exports and core's collections are prefixed with their owner
(`kb.documents`, `core.users`). Records are JSON objects keyed by field name, and include the system
fields `_version`, `_created_at` and `_updated_at`.

**Reading**

| Request | Answer |
|---|---|
| `{"op": "get", "collection": "greetings", "key": "0192…"}` | `{"record": {…}}`, or `{"record": null}` |
| `{"op": "query", "collection": "greetings", "where": {…}, "search": "ada", "order": [{"field": "at", "dir": "desc"}], "limit": 50, "after": "<cursor>", "fields": ["id", "name"]}` | `{"records": [{…}], "next": "<cursor or null>"}` |
| `{"op": "aggregate", "collection": "greetings", "where": {…}, "group_by": ["by", {"field": "at", "bucket": "day"}], "measures": {"n": {"count": "*"}}}` | `{"groups": [{"by": "0192…", "at": "2026-09-18", "n": 12}]}` |

- **`where`** combines conditions on fields. `{"name": "Ada"}` means equals. Operators take the form
  `{"at": {"gte": "2026-09-01T00:00:00Z", "lt": "2026-10-01T00:00:00Z"}}`:
  - comparisons: `eq`, `ne`, `lt`, `lte`, `gt` and `gte`
  - lists: `in` and `not_in`
  - `prefix`, for text
  - `contains`, for a `list` field
  - `is_null`: `true` or `false`

  Several fields in one object must all match. `{"any": [ {…}, {…} ]}` matches either, and
  `{"all": [ … ]}` can be nested.
- **`search`** matches the collection's `search` fields by full text, and orders the results by
  relevance unless `order` is given.
- **`order`** may use the key, `_created_at`, `_updated_at`, or the leading fields of a declared
  index. Anything else is refused, so a query can never sort a whole collection unindexed.
- **Paging:** `limit` defaults to 50, and the maximum is 1,000. Pass `next` back as `after` for the
  following page. Cursors don't expire, and a page never repeats or skips a record.
- **`fields`** returns only the named fields.
- **`aggregate`**:
  - `measures` are `count` (of `"*"` or a field), and `sum`, `min`, `max` and `avg` of a field.
  - `group_by` takes up to three fields. A timestamp can be grouped by `bucket`: `hour`, `day`,
    `week` or `month`, in UTC.
  - At most 1,000 groups are returned. `sum` and `avg` of a `decimal` come back as strings.
- Reading `core.tasks` returns only the reading plugin's own tasks.

**Writing** (own collections only)

| Request | Answer |
|---|---|
| `{"op": "insert", "collection": "greetings", "values": {"name": "Ada"}}` | `{"record": {…}}`, with the key and defaults filled in |
| `{"op": "update", "collection": "greetings", "key": "0192…", "set": {"name": "Ada L."}, "version": 3}` | `{"record": {…}}` |
| `{"op": "upsert", "collection": "imports", "on": ["source", "external_id"], "values": {…}}` | `{"record": {…}, "created": true}` |
| `{"op": "delete", "collection": "greetings", "key": "0192…", "version": 4}` | `{"deleted": true}` |
| `{"op": "batch", "writes": [ {insert…}, {update…}, … ]}` | `{"results": [ … ]}`, in the same order |

- **`version`** is optional on `update` and `delete`. If it is given and the record has changed since,
  the answer is `409` `version-conflict` and nothing is written.
- A write whose key, or whose values for a `unique` constraint, belong to another record is refused
  with `409` `duplicate-record`, naming the fields. Treat it as "someone got there first": show the
  person that the name is taken, or read the record that won.
- **`upsert`** inserts, or updates the record matching the `on` fields. `on` must be a declared
  `unique` constraint. Use `upsert` to make `run` and event handlers idempotent.
- **`batch`** applies up to 100 writes in one transaction: all of them happen, or none do.
- A write that breaks a declared rule is refused with `400` `invalid-record`, naming the field. This
  covers a type mismatch, a missing required field, a `max` or `one_of` violation, a `ref` to a
  record that doesn't exist, and deleting a record that a `restrict` reference still points at.
- A record is at most 1 MiB.

**Change events.** After each write request commits, the backend publishes one event per collection
changed, on `plugin.<id>.data.<collection>.changed`:

```json
{ "collection": "greetings",
  "changes": [ { "op": "insert", "key": "0192…", "version": 1 },
               { "op": "delete", "key": "0191…", "version": 4 } ] }
```

The event carries no values, because any plugin can subscribe to it. Plugins the collection is
exported to fetch the records they need. Use these events to keep caches and search indexes up to
date, or to react to another plugin's data.

**Limits and errors.** Each request gets 5 s of database time. Beyond that it is `504`
`data-timeout`: page through large reads and split large writes into batches. Other errors:
- `403`: writing a collection the plugin doesn't own, or reading one that isn't exported to it
- `404` `no-collection`
- `400` `bad-query`: an unknown field or operator, or sorting without an index
- `503`: storage unavailable

### 9.2 `events`: publishing

```json
{ "topic": "plugin.hello.greeted", "payload": { "name": "Ada" }, "idempotency_key": "greet-42" }
```

Answer: `{"id": "0192b0c7-…"}`. A plugin can publish only under `plugin.<id>.` with at least one
more segment. Anything else is `403`. The event's `source` is `plugin.<id>`. `idempotency_key` is
optional: publishing the same key again returns the first event's ID. Keys are scoped to the plugin.

**Keeping resources in Resource Definitions.** The `resources` plugin stores Repository resources
for `github` and `ghe`, Documentation and DocumentationSource for `kb`, CloudResource for
`infra`, and Component for `architecture`. Teams are not among
them: they are the platform's, and a directory keeps its own there (#9.10). To keep the rest, those
plugins publish:

- `plugin.<id>.<kind>.synced`, where `<kind>` is `repository`, `documentation`,
  `documentation-source` or `cloud-resource`, with one document such as `resources`' `apply` takes,
  or `{"documents": [...]}`. `kind` can be left out.
- `plugin.<id>.resources.synced`, with `{"documents": [...]}` that each name their `kind`. Documents
  in one event are applied together, so a repository can connect to a team in the same event.
  Events on different topics can arrive in any order.
- `plugin.<id>.<kind>.removed` with `{"name": "…"}`, or `plugin.<id>.resources.removed` with
  `{"documents": [{"kind": "…", "name": "…"}]}`. This deletes a resource the plugin created. For a
  resource `apply` created, it removes only the plugin's own connections.

**Reading the catalogue as a plugin.** `resources`' `api/` routes answer whoever a call is for, and
a plugin's own work — a schedule, an event it hears, its load — is for nobody, so core refuses it
there. For that work, `resources` serves `discovery/version`, `discovery/neighbours` and `discovery/resources`
(with `?kind=`, or `/{kind}/{name}`) to the plugins its requestable **Plugins that read the
catalogue as themselves** (`reader-plugins`) setting names, `eol` by default: the same answers
`api/` gives anybody who can read the catalogue, since none of that is narrowed to the viewer, but
only for the catalogue's own kinds — never people, service accounts or roles, which are core's and
`rbac`'s, read as whoever asks. A plugin not listed is answered `403` `not-listed`, and asks to be
added (#9.15).

**What is published here.** Every event goes through the Event Bus, which keeps a log per topic, so
the bus itself is the list of what there is to subscribe to. A plugin reads it as `core.topics`
(#4.3), and the frontend as `GET /api/v1/events/topics`, which answers
`{"topics": [{"topic", "retained", "published"}]}` in topic order to anyone signed in. Nothing keeps
a list by hand: a topic appears the first time something publishes on it. Automation's trigger and
event-action forms offer them as a picker (#9.13).

**Notifications.** A plugin tells people things by publishing events, and automations deliver
them. The Automation plugin's templates listen for these topics, and match an event to the
resource they are applied to by the field shown:

| Topic | Published when | Fields |
|---|---|---|
| `plugin.water.kudos.given` | Kudos are given | `team`, `from`, `to`, `message`, `url` |
| `plugin.water.card.delivered` | A card reaches its reveal date | `team`, `title`, `recipient`, `url` |
| `plugin.process.occurrence.reminder` | An occurrence is coming up | `resource`, `process`, `due_at`, `assignees`, `url` |
| `plugin.process.occurrence.missed` | An occurrence was missed | `resource`, `process`, `due_at`, `url` |
| `plugin.calendar-events.reminder.due` | An event is about to start | `calendar`, `title`, `starts_at`, `url` |
| `plugin.rota.shift.uncovered` | A shift needs cover: whoever the rotation gives is on holiday, or the cover a lead picked is away for some of it | `rota`, `team`, `shift`, `state` (`gap` or `cover`), `planned`, `assignee`, `starts_at`, `ends_at`, `url` |
| `plugin.rota.shift.assigned` | A lead picks cover for a shift, or gives it back to the rotation | `rota`, `assignee`, `state` |
| `plugin.vacuum.run.finished` | A Data Vacuum run stops taking items in, and waits for review | `run`, `title`, `url` |
| `plugin.infra.resource.expiring` | A cloud resource will soon expire | `team`, `name`, `vendor`, `expires_at`, `url` |

`team` is a team's name. `resource` and `calendar` name a resource as `kind:name`, with the kind in
lower case, such as `service:card-gateway`. `url` is a link to the thing in DOC.

**The bell.** What a plugin puts in somebody's inbox with `Backend::notify` shows under the bell in
the header until it is read, and on `/p/notifications/` until it is archived or deleted. Each is
one line in the bell: a badge naming what sent it, its title (cut short, with the rest and its
body in the tooltip), the time — or the date, if it was not today — and a tick that marks it read.
Its title opens the inbox at it, `/p/notifications/?show=<id>`: the inbox reads on past its first
page until the notification is there, marks it, scrolls to it and marks it read. Once read, here or
on the inbox page, it leaves the bell. What sent it is the plugin core vouches
for on `discovery/notify`, so a notification cannot claim to come from another plugin; Automation's
say **Automation**.

**Delivery data.** A source control plugin feeds DORA metrics (`dora`) by exporting two
collections to it and saying when it has read a repository again. `github` and `ghe` do it with
their **Delivery data** feature on; a GitLab, Gitea or local git plugin would do the same:

| Exported to `dora` | Fields `dora` reads |
|---|---|
| `deployments` | `id`, `repository` (`owner/name`), `environment`, `kind` (`deployment` or `workflow`), `state` (`success` counts), `sha`, `url`, `finished_at`, `rollback` (it deployed a commit behind the one before), `commits` (`[{sha, at, message}]`, what it shipped since the deployment before to the same environment). Indexed by `finished_at` |
| `pull-requests` | `repository`, `title`, `labels`, `merged_at`, `merge_sha`, `url` |

| Topic | Published when | Fields |
|---|---|---|
| `plugin.<id>.delivery.synced` | A repository was read again and something changed | `repository`, `since`: the earliest time anything changed, which `dora` works the repository out again from |
| `plugin.<id>.deployment.recorded` | A deployment finished, once for each state | `deployment`, `repository`, `environment`, `kind`, `state`, `sha`, `url`, `finished_at` |
| `plugin.<id>.pull-request.merged` | A pull request was merged, once | `repository`, `number`, `title`, `labels`, `url`, `merged_at`, `base` (the branch it was merged into), `merge_sha` |
| `plugin.dora.counter.incremented` | Something was counted with `dora`'s `increment` operation | `counter`, `amount`, `service`, `repository`, `deployment` (the one it marked as failed, if any) |

`dora` subscribes to `plugin.*.delivery.synced` and reads the plugins its `sources` setting names.

**Pipeline data.** CI/CD/CT metrics (`cicd`) are fed the same way, from every run of a
repository's CI workflows rather than only its deployments. `github` and `ghe` do it with their
**Pipeline data** feature on:

| Exported to `cicd` | Fields `cicd` reads |
|---|---|
| `workflow-runs` | `id`, `repository` (`owner/name`, lower case), `workflow_id`, `workflow` (its name), `path` (its file), `event`, `branch`, `default_branch` (it ran on the repository's default branch), `sha`, `conclusion` (`success`; `failure`, `timed_out` or `startup_failure` failed; `cancelled`; the rest count for neither), `attempt` (more than 1: it was re-run), `created_at`, `started_at` (of its latest attempt), `finished_at`, `url`. Only finished runs. Indexed by `finished_at` |

| Topic | Published when | Fields |
|---|---|---|
| `plugin.<id>.pipelines.synced` | A repository's runs were read again and something changed | `repository`, `since`: the earliest run that changed, which `cicd` works the repository out again from |
| `plugin.cicd.pipeline.broken` | A workflow failed on a default branch it had passed on, in the last day | `repository`, `workflow`, `stage` (`ci`, `cd` or `ct`), `broke_at`, `url` |
| `plugin.cicd.pipeline.fixed` | It passed there again, in the last day | the same, and `fixed_at`, `seconds`, `failed_runs` |

`cicd` subscribes to `plugin.*.pipelines.synced` and reads the plugins its `sources` setting names.

A topic's segments are lower-case letters, digits and `-` only, so a name that reads as two words
is written with a hyphen: `pull-request.merged`, never `pull_request.merged`, which the Event Bus
refuses to publish or subscribe to.

**Repository insights.** `insights` subscribes to `plugin.*.pull-request.merged` from the plugins
its `sources` setting names, and scans a repository again when a pull request went into its
**Branch** (`main`). It fetches the repository through the source's `discovery/archive-links`, so a
source it reads must list `insights` among the plugins given archive links. It announces:

| Topic | Published when | Fields |
|---|---|---|
| `plugin.insights.scan.completed` | A scan found something new: the branch moved since the scan before | `repository`, `source`, `branch`, `commit`, `base_commit` (the commit scanned before, if any), `url` (its page), `files`, `lines`, `functions`, `lint_warnings`, `untested`, `changed_files` |
| `plugin.insights.scan.failed` | A scan could not be done | `repository`, `source`, `branch`, `problem`, `url` |
| `plugin.insights.packages.listed` | The packages `ccc audit` resolved in a repository were kept, after a scan or for a repository scanned before they were | `id` (`source/owner/name`), `repository`, `source`, `packages`: how many |

None carries security findings, which only holders of `insights`'s `security` permission see. Each
repository's package list — ecosystem, name, version, whether it is direct or only for development,
and its lockfile, with no advisories — is kept in a `packages` collection exported to `eol`.

**Its domain and its name servers.** Until its settings say otherwise, the DNS plugin answers for
the domain the platform is configured with — `[instance] domain` in `doc.toml`, which core gives
every plugin with its settings as `instance.domain`, beside `instance.address`, the machine's IP
address (configured, else the one the backend finds on its network) — and names the plugins
under it. Each domain's NS records are the
names under **Name servers**, or `ns1.<the domain>` where there are none, and its SOA names the
first as the primary. A name server inside DOC's domains is answered with the addresses under
**Where the name servers are reached**, worked out like a plugin's name rather than kept as a
record, so a domain handed to DOC resolves its own name server to the address the parent's glue
gives; a record written by hand for that name is used instead. It is a setting because the plugin
cannot see that address: behind a load balancer it sees only its own.

**Developers' machines.** The DNS page is an administrator's (it is in the **Admin** menu), so how
a developer points their own machine at DOC's DNS is in the Knowledge Base instead, in the space
`development-environment`, as the page *Using DOC's DNS on your machine* (`dns.md`). The DNS plugin
writes it itself, from its settings — the domains, the name server's address and port (the
configured machine's address when the server listens on every address), DNS over HTTPS at
`https://<instance domain>/api/v1/plugins/dns/api/dns-query`, and the plugins' names — with the steps for systemd-resolved, macOS's `/etc/resolver` and
Windows' NRPT (or why Windows cannot, off port 53), and writes it again after every load, which
every settings change brings, and on its hourly `guide` schedule when it would say something new
or has not been written for a day. If it is the first to write there, the space is kept by its
**Who keeps development-environment** setting, or by the organisation when there is only one.

**DNS over HTTPS.** The DNS plugin answers the questions it answers on its port over the
platform's own HTTPS as well (RFC 8484), with **DNS over HTTPS** on. It opens no port and needs no
certificate of its own: a DoH endpoint is a route, so it is served over whatever TLS the platform
is served with. A question is `?dns=<base64url, no padding>` with `GET`, or the message as
`application/dns-message` with `POST`, and the answer comes back as `application/dns-message` with
`cache-control: max-age=<the smallest TTL in it>`. A DNS-level refusal — NXDOMAIN, REFUSED,
SERVFAIL — is a DNS message with `200`, as RFC 8484 requires; `4xx` is only for a question that is
not one.

Who may resolve is decided differently from the port, because the platform does not pass a
caller's address to a plugin, so `forward-for` cannot apply:

| Route | Who | Names outside DOC's domains |
|---|---|---|
| `api/dns-query` | Anyone who may read the plugin | Forwarded |
| `public/dns-query` | Anyone at all | **Refused**, never forwarded |

The second is what a browser or a resolver that cannot hold a DOC token uses, and refusing to
forward is what stops it being an open resolver. It exists only where the deployment asked for it
twice: `dns` allowed `public-routes` in `[plugins.capabilities]`, and `DOC_DNS_PUBLIC_RESOLVER` set
where the plugin runs. The capability is declared only when that variable is set, because a
capability the configuration does not allow refuses registration outright, and a name server that
will not start is worse than one without the route.

**Who is away.** An event on a person's **own** calendar can say they will not be working:
`away` is `holiday`, `sick` or `other`, and empty for everything else. Only a person's own
calendar carries it — an event on a team's is refused — since a meeting is not anybody being away.

`calendar-events` answers `GET discovery/away?users=<logins>&from=&to=` with
`{"away": [{user, starts_on, ends_on, away, note, event}]}`: whole days, worked out in the
person's own time zone, because the question is whether they are working that day. Any plugin may
ask; it says only that somebody is not working and when, and nothing else of their calendar.

Two plugins read it, and neither keeps a copy, so leave that is cancelled stops counting at once:

- **`rota`** adds those spans to the holidays a lead recorded, and plans by both. A turn that falls
  to somebody away for any of it is a gap, cover is offered only from people who will be there, and
  the lead is told — all of which the planner already did for a recorded holiday. An absence from a
  calendar is shown on the Holidays page and cancelled there rather than in DOC.
- **`process`** marks an occurrence whose assignees will not be there, carries them in its
  reminder, and publishes `plugin.process.occurrence.uncovered` when **every** named person is away
  and no team is among the assignees — a process a team is responsible for is not uncovered because
  one of them is on holiday.

**Maturity.** A **model** is what an organisation or one team expects of the things it runs, kept
by `organisation:<name>` or `team:<name>` — the same choice a Knowledge Base space makes, and for
the same reason. It holds **criteria**, each one thing that is either met or not, worth a weight
beside the others, and each decided from something the platform already knows:

| Check | Met when |
|---|---|
| `connected` | The Catalogue connects it to at least *n* of a kind |
| `owned` | The Catalogue names a team that owns it |
| `metadata` | Its metadata sets a key, optionally to one of some values |
| `readiness` | A plugin's `api/readiness` says `ready` for it (#9.2) |
| `model` | Another model grades it at least `least`, 10 being all of that model |
| `manual` | Somebody attested to it, and their name and the day stand beside it |

**A model may stand on another**: a production model asking that a service is development ready
first, by a `model` criterion naming it. The model named is scored before the one standing on it,
so the grade read is that run's rather than yesterday's, and a model may stand on neither itself
nor anything that stands on it — writing either is refused, because two models waiting on each
other could never be scored at all. Where a model is scored on its own, after somebody changed it,
what it stands on is read from the scorecards already written. A component the other model does not
grade is `unknown` rather than unmet, on the same footing as a plugin that could not be reached.

A team's model grades what that team owns; the organisation's grades everything. Each component
gets a **scorecard**: every criterion with whether it was met and why, and a **grade from 1 to
10** — one for meeting nothing, ten for meeting everything, and nothing but everything reaching
ten. A criterion nothing could answer is left out of both sides rather than counted against, so an
unreachable plugin drags no grade down, and the scorecard says which those were.

Scoring runs on a schedule — hourly, and a setting — again whenever a model or a criterion
changes, and again a couple of minutes after the Catalogue changes — nearly every criterion is decided from what the Catalogue
holds, so a grade that only followed the schedule would be a day behind the moment somebody
connected a repository to a service. Catalogue changes arrive in bursts, so the first of them
starts a short wait and everything inside it is scored by the same run.

**Scoring reads the Catalogue as somebody.** The Catalogue answers whoever is asking, and a plugin
asking as itself is told nothing, so a run nobody started would score nothing at all. Whenever
somebody writes a model or a criterion, attests to one, or asks for a score, the plugin takes leave
to act as them (#9.5) and keeps it; the schedule and a Catalogue change then run as whoever last
gave it, and the Maturity page says who that is. Until anybody has, the page says plainly that
nothing is scored on its own. It publishes
`plugin.maturity.component.graded` when a grade is written or moves, and
`plugin.maturity.component.slipped` when it falls, which is what an Automation listens to. The
plugin answers `api/readiness` itself, so the roadmap can ask how mature a service is the same way
it asks everything else.

Grades are shown with `doc-grade` and `doc-grade--1` … `--10`: a circle with the number struck in
it, ringed in its own ink, red through amber to green, every step at least 5.9:1 against that ink.
It is round because a grade is **an award rather than a status** — something earned, not a state a
thing is in — and `doc-badge` stays the shape for a state. `doc-grade--large` is the same medal
where it is the whole of what a tile says, such as a pinned insight. The number is always inside
it: a red-to-green scale is the one thing a colour-blind reader cannot follow, so the colour never
carries it alone, and under `prefers-contrast: more` and in print the fill goes while the number
and the outline stay. The colour fills the medal to its ring — nothing white between the two.

**A name for each plugin.** With **A name for each plugin** on, the DNS plugin answers for one
name per plugin under the domain in its settings — `rbac.rundoc.sh` for `rbac` — pointing at
wherever DOC is served. The names are worked out from `core.plugins`, which every plugin may read,
rather than kept as records: adding a plugin adds its name, removing one takes it away, and a
record written by hand for the same name is used instead of the worked-out one. A name that is not
a plugin's does not exist, as any other name in the zone does not.

Opening one lands on that plugin's page. The frontend answers a request whose `Host` is
`<plugin>.<the host of DOC_PUBLIC_URL>` with `308` to `<DOC_PUBLIC_URL>/p/<plugin>/<the rest>`,
before routing or the CSRF guard, so the method, the path and the query all survive. It is a
redirect rather than a proxy because **every cookie the frontend sets is host-only**: a session on
the platform's own host is not sent to a subdomain, so serving the app under each plugin's name
would mean signing in again for each, or widening the session to the whole domain so that any
plugin's name could act with it. Without `DOC_PUBLIC_URL` the frontend does none of this.

**End of life.** What a service runs comes from three places, and all are read. The
**repositories connected to it in the Catalogue** are the first: `insights` exports the packages
`ccc audit` resolved in each repository it scans (`insights.packages`), and `eol` matches each to a
product by the package URLs endoflife.date publishes as its products' identifiers (`pkg:npm/react`,
`pkg:pypi/django`), from its copy of endoflife.date (below), leaving out development
dependencies. It reads them when `plugin.insights.packages.listed` arrives and again nightly. With
**Runtimes from repository files** on, the same repositories are also fetched for what ccc does not
read — the files a project already keeps, never anything added for DOC: `Dockerfile` and
`docker-compose` image tags, `go.mod`, `.nvmrc`, `.node-version`, `.tool-versions`, `mise.toml`,
`package.json` engines, `pyproject.toml`, `.python-version`, `runtime.txt`, `Gemfile`,
`.ruby-version`, `Cargo.toml`, `rust-toolchain`, `composer.json`, `pom.xml`, `*.csproj`,
`global.json` and `.github/workflows`. Vendored and built directories (`node_modules`, `vendor`,
`target`, `dist`, dot-directories other than `.github`) are stepped over, since what is in them is
not what the repository runs. Last, a service's own `endoflife.date/products` metadata names
products as Backstage's end-of-life plugin reads them (`nodejs@20,postgresql@16`).

Fetching files, `eol` goes through the source's `discovery/archive-links` like `insights`, so a source
must list `eol` among the plugins given archive links — GitHub's list does unless somebody saved it
without — and where it does not, `eol` asks for access rather than failing each night, and reads
again as soon as the request is approved (`platform.plugin.eol.access.approved`). A repository is
fetched again once the Catalogue says it has been pushed to since it was last read, or at the next
read where the last one failed, and a repository connected to no service is not read at all. `eol`
also reads everything as it loads while any repository's last read failed.
Which services a repository belongs to is read from the Catalogue's `discovery/` routes as `eol` itself —
when it first loads, nightly, when a repository's packages are listed, and two minutes after
`plugin.resources.changed` says a connection may have moved — so no reading depends on anybody
looking. `api/repositories`, which names each repository's services, answers only somebody who can
read the Catalogue too. Reading is never something a page waits on: a page
shows what was last found. Every product a page, the API or MCP gives carries `from`, which is empty
where the service's own metadata named it and otherwise names the repository and the lockfile or
file it was read from, so a warning can always be traced to what said it.

Release cycles come from **`eol`'s own copy of endoflife.date**, never from endoflife.date while a
page waits. Every product it tracks is read in one request to `<base-url>/api/v1/products/full` when
`eol` loads (where the copy was never read, is over a day old, was read from another `base-url`, or
its last read failed) and on the `refresh` schedule, sending the last answer's ETag. Each product is
kept as endoflife.date answered it (`document`), with a digest so that only products whose answer
changed are written, and with what judging and package matching read from it; the order of the
answer and its `generated_at` and `schema_version` are kept in the plugin's state. A name the copy
does not hold is not known, without asking; only before the copy is first read is a product asked
for on its own (`/api/v1/products/<product>`). A product endoflife.date stops listing is kept with
`listed` false, still judged and still served by its name. A read that fails keeps the copy, records
when and why with the whole error chain, and fails the run so that it is retried. A service's own
lifecycle file is read on its own and kept a day, and one that fails keeps what it had and is not
asked for again for an hour. The copy is served under `api/v1/` in endoflife.date's API v1 format —
`products`, `products/full`, `products/<product>` and `products/<product>/releases/<release or
latest>`, each with an ETag — so a tool given `<DOC>/api/v1/plugins/eol` as endoflife.date's address
and a token that reads `eol` gets what endoflife.date would have answered; **All products** lists it
for people.

**Reliability.** `reliability` announces each outage it records, whether a failed health check, a
report through its API or DOC's own status history noticed it, once when it starts and once when
it ends:

| Topic | Published when | Fields |
|---|---|---|
| `plugin.reliability.outage.started` | Something went down | `outage`, `subject` (`service:<name>`, `doc:<component>` or `plugin:<id>`), `kind`, `name`, `started_at`, `source` (`check`, `report` or `status`), `detail` |
| `plugin.reliability.outage.ended` | It came back | the same, and `ended_at`, `seconds` |

**Release data.** The delivery roadmap (`roadmap`) is fed releases the same way: by a tracker
plugin exporting three collections to it. `jira` and `jira-dc` do it with their **Release data**
feature on; a Linear or GitHub-milestones plugin would do the same:

| Exported to `roadmap` | Fields `roadmap` reads |
|---|---|
| `projects` | `key` (such as `PAY`), `name`, `url` |
| `versions` | `id`, `project` (its key), `name`, `description`, `released`, `start_date`, `release_date` (days), `url`. Unreleased ones, and those released lately; never archived ones |
| `issues` | `id`, `key`, `project`, `summary`, `type`, `status`, `category` (`new`, `indeterminate` or `done`: to do, in progress, done), `versions` (a `list` of the version IDs it is in, read with `contains`), `components`, `labels`, `updated`, `resolved_at`, `url`. Only issues in a version it exports |

| Topic | Published when | Fields |
|---|---|---|
| `plugin.<id>.releases.synced` | A read of releases changed something | `projects`, `versions`, `issues`: how many were read |

`roadmap` reads the plugins its `sources` setting names when somebody looks, so it keeps nothing
and subscribes to nothing.

**Readiness.** The delivery roadmap asks whether each service in a release is ready to ship. A plugin
that can say answers one route, which `roadmap` calls with `Backend::ask` — as whoever is looking,
so the answer follows the caller's permissions:

```
GET api/readiness?service=card-gateway&service=ledger&until=2026-11-01
```

```json
{ "title": "Pipelines",
  "services": {
    "card-gateway": { "state": "ready", "summary": "98.6% of runs passed in the last 30 days, and nothing is broken", "href": "/p/cicd/?service=card-gateway" },
    "ledger": { "state": "blocked", "summary": "CI is broken on acme/ledger's default branch since 26 Sep 2026, 09:14 UTC", "href": "/p/cicd/?service=ledger" } },
  "test_data": false }
```

- `service` may repeat, up to 100; `until` is the day the release is due, for a plugin whose answer
  depends on it, and is left out for a release with no date.
- `state` is `blocked` (it should not ship as things are), `warning`, `ready`, or `unknown` (the
  plugin cannot judge it: nothing measured, or not a service it can see). Every service asked is
  answered.
- `summary` is one sentence a page shows as it is; `href` is a page in DOC (`/p/…`) that shows why,
  and any other link is dropped. `title` heads the plugin's column.
- A plugin with no such route answers `404` and is left out; a `403` is shown as the caller not
  being able to see that plugin.

`cicd`, `reliability`, `eol` and `dora` answer it; `roadmap`'s `readiness-from` setting names who is
asked.

The Knowledge Base's own `plugin.kb.document.imported` (`space`, `path`, `title`, `url`, the
`resources` a page documents as `kind:name`, and the `source` it came from) and
`plugin.kb.document.removed` are read the same way: each page is a Documentation resource named
`<space>/<path>`, connected to what it documents and to the source that brought it in.

Where those pages come from is catalogued too. The Knowledge Base publishes
`plugin.kb.documentation-source.synced` with every one of its sources — when one is added or
rescheduled, and once at load, so a Knowledge Base filled before any of this says where its pages
came from. A source is a **DocumentationSource** named `<space>/<kind>/<where it reads>`, such as
`payments-docs/github/acme/payments-docs`, titled with what its own page says of it, and connected
to the Repository it reads where it reads one. A
definition added to that list after a catalogue was made is written the next time the plugin loads;
one that somebody has since changed or removed is left as they left it. Nothing removes a source
from the catalogue yet, because nothing removes a source.

**Documentation sits with what it documents.** It hangs below a **Service** or a **Repository**,
which is the usual case, and below an **Organisation** or a **Team** for the documentation that is
not about a piece of software: processes, runbooks, how a team works, what it has agreed. Five
default definitions say so — `Service-to-Documentation`, `Repository-to-Documentation`,
`DocumentationSource-to-Documentation`, `Organisation-to-Documentation` and
`Team-to-Documentation` — and `Repository-to-DocumentationSources` puts a source under the
repository it reads.

**A space belongs to the people who keep it.** A Knowledge Base space names either **one or more
Teams** or **the Organisation** — not both — and it cannot be made without one. That is a different
question from what its pages are *about*, which a space answers with `resource`: a runbook space
kept by the platform team documents nothing in particular, and a repository's space documents that
repository while being kept by whichever team owns it. Both go out on every page's
`plugin.kb.document.imported`, in `resources`, so the Catalogue connects the page to what it
documents *and* to whoever keeps it — which is what `Organisation-to-Documentation` and
`Team-to-Documentation` are for.

**Runbooks.** A page is a runbook when its front matter says `runbook: true`, one of its labels is
`runbook`, or its file or a folder it is in is called `runbook` or `runbooks`; `runbook: false`
keeps a page out whatever it is called. Its front matter's `environment` (`development`, `test` or
`production`, which it is when it says nothing) is what it is run against by default. Nothing is
stored for it, so pages imported before there were runbooks count too. `GET api/runbooks` lists
the runbooks in the spaces in use, and `GET api/runbooks/<space>/<path>` gives one with its text as
Markdown and the SHA-256 of that, which is what Agent Smith runs and pins when one is approved.
A runbook's page offers **Run with Agent Smith** to whoever may use Agent Smith.

**Pages a plugin writes.** A plugin named in the Knowledge Base's requestable **Plugins that
publish pages** (`publisher-plugins`, by default `dns`) writes pages into a space itself with
`POST discovery/pages` and `{space, title, owners, pages: [{path, content}]}`, up to 20 Markdown
pages, written while it waits. Each plugin's pages are a source of their own in the space (kind
`plugin`), so writing them again replaces only what that plugin wrote, and a page it no longer
sends is removed; what people and other sources put in the space is untouched. A space it makes
needs `owners`, as any space does; one that exists keeps whoever keeps it. A plugin left out is
refused with `403` and asks to be added; an administrator may archive what a plugin writes in a
space like any source, and it writes nothing there until it is restored.

A space made for a followed repository takes that repository's owning team from the Catalogue; one
whose repository names no team is left unowned rather than attached to somebody nobody chose. A
space made before this keeps syncing and serving, and is flagged as unowned on the Spaces page and
on its own, with a picker to say. Saying who keeps a space writes the owners onto every page it
already holds and announces each of them again, rather than waiting for a sync that, for an upload
source, never comes. Ownership says whose documentation it is; it grants nothing and withholds
nothing, and `plugin:kb:*` is still what decides who may read and write.

**The order of the kinds is declared, not worked out.** It is the order of the `Kind` enum in the
Catalogue, from the top of a drill-down to the bottom, and `GET kinds` gives each one its `rank`:

```
Organisation · Service · Component · Documentation · DocumentationSource
             · Repository · Team · Role · User · ServiceAccount · CloudResource
             · Attribute · Permission
```

Everything that shows kinds side by side reads that one order — the service map's columns, the
kinds overview, the pickers, and the things a resource is connected to. It is declared rather than
derived from whichever connections exist, because a kind with several parents would otherwise be
pushed to the end of the map, away from what it belongs to, and adding one definition would move
columns that have nothing to do with it. **A kind is placed beside what it belongs to**, which is
why Documentation and DocumentationSource sit between Services and Repositories.

**A Documentation resource shows what its page says.** The Knowledge Base adds a **Summary** panel
to a Documentation resource's page, asking to come after the panels that say nothing about where
they sit, so it reads below Processes and Discussions rather than above them. It shows the opening
of the page — cut at the end of a sentence where one ends near enough, and at a word otherwise —
and a link to read the rest. Metadata that names somewhere is a link too, on every resource page:
a value beginning `http://`, `https://` or `/` is shown as a link to it, so a page's `url` goes to
the page and a repository's goes to GitHub. Nothing else is linked, so metadata cannot put a
`javascript:` link on a page.

**A service's documentation includes its repositories'.** A page documents whatever its space is
bound to and whatever its front matter names, and both of the places a service's documentation is
listed — the **Docs** panel on its page in the catalogue, and the Knowledge Base's own
`/p/kb/catalogue/<service>` — show those plus the pages written in the repositories that service is
connected to, up to twenty of them.

**A README is the front page.** A repository's `README.md` — or an `index.md`, whatever its case,
at the root — is imported first, so it opens its space. Where a service is built from **one**
repository, that README *is* the service's documentation: `/p/kb/catalogue/<service>` reads it in
place, with the rest of that repository's pages listed under it, and the Docs panel opens with its
first few sentences and a way in. Where a service is built from **several**, neither page picks
one: they are a contents list with a heading for each repository and that repository's README at
the top of it. **On a service's page the Docs panel points rather than lists:** under each README,
or under each heading with no README, it says how many more pages there are, "(and 12 more in
acme-card-gateway)", linking to that space, and `/p/kb/catalogue/<service>` lists them all. So
connecting `Repository:acme/ccc` to `Service:tooling` is all it takes for the Markdown in that
repository to be the service's documentation; nothing has to be written twice or moved. A
repository's own panel shows only what documents that repository, and a team's only what names the
team.

**Documentation from repositories.** Markdown written in a repository is documentation, and with
the Knowledge Base's `repository-docs` feature switched on nobody has to say so twice: it keeps a
source of its own for every repository the catalogue holds, reads each one through a GitHub archive
link, and takes **every `.md` file wherever it sits** — the importer already does this for a
repository with no `mkdocs.yml`. Each repository's pages go into a space bound to
`repository:<owner>/<name>`, so they are Documentation connected to that repository and appear
under the **Docs** tab on its page. The sources it keeps are marked `managed`: they have no
schedule of their own and are not edited by hand. A repository is read again only once its
`pushed_at` is later than the last read — which is why `github` publishes `pushed_at` in a
Repository's metadata — and one that says nothing about its pushes is re-read daily. A repository
with no Markdown in it is recorded as `empty` rather than failed, and left alone until it is pushed
to again; one GitHub would not hand a link to yet — it allows 60 archive links an hour to anyone
asking without a token — is recorded as `waiting`, with when it was last read left alone so the
next run reads it. The sweep also runs on demand, from **Look for repositories now** on the Knowledge Base's
Sources page, which says what it found, what it took on and what it is reading. A run takes on at
most 100 new repositories and reads at most
`repository-docs-per-run` of them, twelve by default, which at four runs an hour stays under that
tokenless limit; a deployment whose GitHub plugin has a token or an App is limited far more
generously and can raise it. So a platform turning this on works through its repositories over the
runs that follow rather than fetching every archive and writing every row at once. The other
settings are how often to look (`repository-docs-schedule`) and what to leave out
(`repository-docs-exclude`, whole names or patterns such as `acme/*` or `*-archive`; archived
repositories are left out anyway). The catalogue's own listing takes an `offset` beside its `limit`,
so more repositories than one page holds can be read in full.

A space can also be filled from a git repository uploaded as a `.zip` (a `git` source; `POST
api/git?space=…&owner=…`, or **A git repository (.zip)** on the Sources page): its Markdown becomes
pages, a `.git` directory zipped with it gives the source its name (from `origin`), branch and
commit, and what its top `.gitignore` leaves out is left out; uploading it again updates it. Any
import keeps the images and files a page links to that are in the archive beside it — PNG, JPEG,
GIF, WebP, SVG (only ever downloaded), PDF, text, CSV, JSON and YAML, 5 MiB each, 200 an import — as
the page's attachments, served at `/p/kb/attachments/<space>/<page path>/<name>`. `api/imports`
takes a `.zip` as well as a `.tar.gz`.

DOC's administrators link spaces to a service (`PUT` and `DELETE api/services/<service>/spaces/<key>`,
or on the service's page): every page in a linked space is the service's documentation, counted in
the Software Catalogue's Docs column and shown on its page and panel, beside the pages that name it
and those of the repositories it is built from.

DOC's administrators rename a space with **Rename** on its page (`ui/spaces/<key>/name`). Its key
stays, so links to its pages keep working; the space records who renamed it and when
(`renamed_at`, `renamed_by`), and from then on a sync or a plugin writing there keeps that name
rather than the one it brings. Renaming is audited as `space.renamed`.

Only DOC's administrators take anything out of the Knowledge Base, and every step is audited.

**Nothing is deleted in one step.** A space, a source or a followed repository is **archived**
first: it is kept whole and only put out of sight — out of the listings, out of search, out of the
catalogue, its pages answering nothing at their own addresses, and syncing no more. Restoring it
puts all of that back, pages and sources alike. Deleting for good is offered **only on something
already archived**, and refuses with `archive the space first` otherwise; that is the one step that
cannot be undone.

- A space: `POST api/spaces/<key>/archive`, `…/restore`, then `DELETE api/spaces/<key>`. What is
  archived is listed at `GET api/spaces/archived`, and on the Spaces page under **Archived**, which
  is where it is restored or deleted.
- A source: `POST api/sources/<id>/archive`, `…/restore`, then `DELETE api/sources/<id>`. An
  archived source syncs no more and leaves the catalogue; **the pages it brought in stay in their
  space**, which is the space's to archive or not.
- A repository: `DELETE api/repositories/<owner>/<name>` stops following it and **archives** its
  space rather than deleting it, so stopping by mistake costs only the time to follow it again
  (`POST api/repositories/<owner>/<name>/follow`, listed at `GET api/repositories/unfollowed`, and
  on the Sources page).

A **page** is the exception, deleted outright by `DELETE api/documents/<space>/<path>`: one whose
source still has it comes back at the next sync anyway, so archiving it would say something untrue.

The catalogue is told of every page (`plugin.kb.document.removed`) and source
(`plugin.kb.documentation-source.removed`) that goes, archived or deleted, and of every page
(`plugin.kb.document.imported`) and source that comes back when something is restored.

The Process plugin's occurrence events also carry `occurrence`, `process_id` and `owner`, with
`assignees` as readable text, such as `alice, team payments-core`. It publishes
`plugin.process.occurrence.done` (adding `by` and `late`) and `plugin.process.occurrence.skipped`
(adding `by` and `note`) with the same fields.

Watercooler publishes `plugin.water.thread.created` (`thread`, `title`, `author`, `tags`, `url`)
and `plugin.water.reply.created` (adding `reply`, and `parent` when it answers a message rather
than the thread: replies nest to any depth). Only whoever wrote a message may change it, and the
thread's title with its first; each change publishes `plugin.water.message.edited` (`thread`,
`message`, `first`, `title`, `author`, `tags`, `url`). Liking a message publishes
`plugin.water.message.liked` (`thread`, `message`, `title`, `author`, `by`, `url`). Moderators —
DOC's administrators and whoever holds the `moderate` custom permission, which people and service
accounts can be given — delete any reply, whose own replies then answer what it answered, or with
the opening post the whole thread; each deletion is audited and publishes
`plugin.water.message.deleted` or `plugin.water.thread.deleted` (`thread`, `title`, `author`, `by`).
For events it publishes
`plugin.water.event.created` (`event`, `kind`, `title`, `team`, `organiser`, `starts_at`, `url`),
`plugin.water.hackathon.registered`, `.submitted` and `.announced` (with `winners`), and
`plugin.water.challenge.entered` (`entrant`, `score`). Its `kudos.given` also carries `kudos` and
`to_kind` (`user` or `team`), and `card.delivered` carries `card`, `kind` and `signatures`. A card
is delivered when its reveal time passes, and until then it is hidden from its recipient.

Infra publishes `plugin.infra.request.created`, `.active`, `.failed` and `.deleted` (`request`,
`team`, `url` and, once made, `name` and `vendor`), and `plugin.infra.resource.drifted` (`changes`)
when a resource was changed outside DOC; `resource.expiring` also carries `request`. It keeps each
resource it makes in Resource Definitions as a CloudResource named `<vendor>/<region>/<type>/<name>`,
connected to its team and service; `Team-to-CloudResources` is one of the default definitions.

**Machines.** Every vendor offers object storage and a machine, the type `vm`: a Linode instance,
an EC2 instance, a Compute Engine instance or an Azure virtual machine. A machine needs more than
a bucket does — an image, a size, a way in, and on Azure a network, a card and a public address —
so each of those is a setting of the plugin's with a sensible default, and the way in is a public
key. None of the vendors is told a password anybody knows: where one is insisted on, DOC makes one,
sends it and forgets it. Azure's network is made on first use and never written over, since another
subnet in it would go with it.

Only Linode answers with the machine itself; the others answer before there is one, so a request
stays `provisioning` until the `lifecycle` schedule finds it running **with an address**. That is
the moment it becomes `active`, because a machine nobody can reach is not one a name should point
at yet. Teardown on Azure is the reverse in order — machine, then card, then address — each waited
for, because Azure refuses a card a machine is still on.

**`plugin.infra.request.active` and `.deleted`** both carry `service`, `type`, `region`,
`environment` and `address` alongside the rest, so whatever cares about where a service runs hears
it the same way whether the resource came from a software template or from the Infra page.

**A name for each service.** With the DNS plugin's **A name for each service** feature on and a
domain set, DOC answers for `<service>.<domain>` pointing at the address of whatever Infra stood up
for that service. The record is written when the resource starts answering and removed when it is
torn down. They are ordinary records: DOC writes a note on the ones it keeps and touches only
those, so a name somebody wrote at the page is left where it is, and one DOC wrote can be edited
like any other.

This is how a software template names what it stands up. A template's `infra` step asks Infra for
a resource and says which service it is for; the step is done in a moment, and the name follows
minutes later when the machine answers. A template that already knows an address — one that did not
come from Infra — uses the `dns` step instead, which writes the record there and then.

A document changes only the fields it names. The kinds listed by name in its `connections`, such as
`{"Users": [...]}`, belong to the sync: its connections to those kinds that are no longer listed
are removed. Connections anyone else made are kept. Anything a sync can't resolve, such as a user
who has not signed in, is left out. Sync events can't connect roles or service accounts, because
people make those connections.

```json
{ "name": "acme/card-gateway", "title": "card-gateway", "metadata": { "default_branch": "main" },
  "connections": { "Teams": ["payments-core"] } }
```

### 9.3 `services`: the Service Bus

```json
{ "address": "core.teams", "subject": "members", "payload": { "team": "payments-core" },
  "deadline_ms": 5000, "queue": false }
```

- This call **requires the `x-doc-context` of the call being handled**. The request is made as that
  call's principal, which is how a plugin acts for a user without holding their credentials. A
  missing, made-up, expired or finished context, or another plugin's, gets `403`.
- `queue: false` waits for a reply: `{"payload": <any JSON>}`. `deadline_ms` defaults to 10 s and is
  capped at 60 s.
- `queue: true` sends a queue message without waiting, and answers `{"payload": null}`. A queued
  message is delivered even if the plugin it is for is busy or restarting when it is sent. To queue
  a POST to another plugin's `discovery/<route>`, send `{"method": "POST", "body": {…}}`, as the Rust
  SDK's `Backend::queue` does. Automations triggered by a queue listen on the Automation plugin's
  `discovery/queues/<queue>`.
- Addresses are `core.<service>` or `plugin.<id>`, with segments `[a-z0-9-]+`. A plugin's `internal/`
  routes answer core alone, so plugins cannot call each other's internal routes.
- To use another plugin's `api/<route>`, send subject `api/<route>` to `plugin.<id>` with
  `{"method": "GET", "query": "a=b", "body": {…}}`. Core checks that call's principal exactly as it
  would over HTTP, then answers `{"status": 200, "body": …}`. A refusal comes back the same way, for
  example `{"status": 403, "body": {"detail": "needs plugin:rbac:user:ro"}}`. The Rust SDK's
  `Backend::ask` does this.
- An `api/` call names the plugin that sent it to the plugin it reaches, as `via` on its caller
  (#8.2). The optional `guard` (`read-only` or `not-production`) limits what the call may change;
  core keeps the stricter of it and the guard on the call being handled, so a limit cannot be stepped
  round by asking another plugin to ask. The Rust SDK's `Backend::guarded` sends one with every call.
- A plugin asking an `api/` route **as itself** — from a schedule, an event or its long-running run —
  holds nothing, and is refused, unless it has the `service-account` capability: then it is checked as
  its own service account (#4.2), holding whatever administrators granted that.
- `discovery/<route>` works the same way, but the request is always made as the sending plugin itself,
  whoever that plugin is acting for. Core checks no permissions, so a `discovery/` route must check
  `caller.id` itself. Use it for what one plugin provides to another, such as the GitHub plugin's
  archive links. The Rust SDK's `Backend::discovery` does this.
- **Calendar events.** A plugin puts events on a resource's calendar by asking `calendar-events`
  for `POST discovery/events` with `{"on": "service:card-gateway", "title": …, "start":
  "2026-10-05T09:00", "timezone": "Europe/London", …}`. `rrule`, `exdates`, `reminder_minutes`,
  `all_day`, `end`, `location`, `link`, `attendees` and `resource` may be added. The calendar is
  made if it is new. It changes its own events with `PATCH discovery/events/<id>` and deletes them
  with `POST discovery/events/<id>/delete`; people cannot edit them. Times are wall-clock times in the
  event's zone, or instants with an offset. A person's own calendar, `"on": "user:<login>"`, takes
  events only from the plugins in Calendar's **personal-events-from** setting (by default `rota`);
  any other plugin is refused with `403`.
- `core.access` answers the principal's access to core and every plugin, as `/api/v1/me/access`
  does, for example to check whether they can write to `rbac`.
- Errors: `404` `no-handler`, `504` `deadline`, and `502` `remote-error` when the handler failed.

### 9.4 `cache`: the Cache Bus

Each plugin has its own namespace, `plugin.<id>`. Keys are 1 to 256 bytes. Entries expire after
24 hours unless `ttl_ms` says otherwise. A namespace holds up to 10,000 entries.

| Request | Answer |
|---|---|
| `{"op": "get", "key": "k"}` | `{"value": <JSON or null>, "version": 7, "applied": true}` |
| `{"op": "set", "key": "k", "value": …, "ttl_ms": 60000}` | `{"value": null, "version": 8, "applied": true}` |
| `{"op": "delete", "key": "k"}` | `{"applied": <whether it existed>}` |
| `{"op": "compare-and-set", "key": "k", "value": …, "version": 8}` | `{"version": 9, "applied": true}`. `applied: false` means the version had moved on: a lost race, not an error. `"version": null` writes only if the key does not exist yet |

### 9.5 `tasks`: background runs

```json
{ "payload": { "source": "confluence" }, "max_attempts": 3 }
```

Answer: `{"task": "0192b0c8-…"}`. This queues a background `run` of **this** plugin, with
`max_attempts` from 1 to 10 (default 3). Each attempt has the async deadline of 30 s, whatever the
plugin's classification, except that a one-shot plugin's runs get 10 minutes. The task is started by the principal of the context token
if there is one, and by the plugin otherwise. When the task finishes, core publishes
`plugin.<id>.run.completed`:

```json
{ "task": "0192b0c8-…", "plugin": "kb", "state": "succeeded", "result": { "payload": { } } }
```

`state` is `succeeded`, `failed` (with `error`) or `cancelled`. Subscribe to the topic to hear how
your own runs went.

**Work longer than one deadline** runs as a chain: each run does a batch, saves its place, and
queues the next batch before it returns. A task queued while one of your task runs is going joins
that run's chain, named by the chain's first task. `GET /api/v1/tasks/{first}` then adds `chained`:
`running` while any part is queued or running, then `failed` or `cancelled` if any part was, and
otherwise `succeeded`, with counts and the first error. So whoever started the work, such as
`cli tasks wait` or `cli kb import --wait`, waits for all of it through the one ID they were given.

**Work done later as someone**, such as an automation that runs as its owner, needs their leave,
given during a call they made. `POST /plugin/v1/delegations` with the context token of that call:

```json
{ "op": "grant", "purpose": "automation 0192b0c9-… on service:card-gateway" }
```

Answer: `{"delegation": "0192b0ca-…"}`. Only a person or a service account can give one, so a
context issued for the plugin itself is refused with `403`. Keep the ID with whatever it is for.
Then `{"payload": …, "as": "0192b0ca-…"}` on `tasks` queues a run started by whoever gave it, from
any call, and the run's context acts for them. They are checked as they are when the task is
queued and again when it runs, so a disabled or deleted account starts nothing, and the run can do
only what they can do at that moment. `{"op": "revoke", "delegation": "0192b0ca-…"}` ends it, and a
revoked delegation is refused with `403`. Granting and revoking are written to the audit log. The
Rust SDK's `Backend::delegate`, `Backend::task_as` and `Backend::revoke` do this.

### 9.6 `state`: durable key-value

`{"op": "get" | "set" | "delete", "key": "k", "value": …}` answers `{"value": <JSON or null>}`. Keys
are 1 to 256 bytes and values at most 1 MiB. State is stored in Postgres and survives restarts and
new versions. Use it for small facts such as cursors and last-sync times. Use your collections for
anything larger.

### 9.7 `audit`

```json
{ "action": "greeting.created", "subject": "greeting:42", "detail": { "name": "Ada" } }
```

Answer: `{"recorded": true}`. `action` is 1 to 64 characters from `a-z`, `0-9`, `.` and `-`, and
cannot start or end with `.`. It is recorded as `plugin.<id>.<action>`, so a plugin's entries never
look like the platform's own. If a context token is present, `detail.on_behalf_of` records whom the
plugin acted for, and `detail.via` the plugin that relayed the call, such as `plugin:agent` for
something Agent Smith changed for somebody.

### 9.8 `status`

`{"state": "cancelled" | "running" | "error", "error": "…"}` answers `{"state": "<new state>"}`. A
plugin uses this to report its own work stopping, resuming or failing. `loading` and `unloading`
belong to the backend (`403`), and a transition #3.1 doesn't allow is `409`.

### 9.9 `identity`: identity providers only

Three calls, each needing the `identity-provider` capability and each only for the plugin's own
accounts: `provider` must be the plugin's own ID, or the call is `403`. `external_id` is the
provider's immutable ID for the account, such as GitHub's numeric user ID. Never use the login for
it, which can be renamed and then taken by someone else. A user is a person who can hold one
account per provider.

**Signing someone in**, `POST /plugin/v1/identity`:

```json
{ "provider": "github", "external_id": "583231", "login": "octocat",
  "name": "The Octocat", "email": "octocat@example.com",
  "first_name": "Mona", "surname": "Octocat",
  "organisations": ["octo-org"], "teams": ["octo-org/octo-team"] }
```

Answer: `{"user_id": "…", "session_token": "…", "expires_at": "…", "first": true}`. The account is
found by `provider` and `external_id` alone, never by a login or an email address. If nobody has it
yet, a new user is made with it. `login` and the profile, `name`, `email`, `first_name` and
`surname`, are brought up to date on the account and on the user, but a missing one never clears
what is there. The user's own login follows the account's while the two match. `first` means this
is the user's first sign-in; the frontend follows it with the accounts plugins need.

People sign in with their own organisation's providers. Each organisation chooses its
identity providers on its page, and each provider serves one organisation. A provider no
organisation chose signs nobody in, a new user joins the organisation that chose it, and anyone of
another organisation is `403`, as is a disabled user.

**A sign-in form.** A provider whose manifest's `sign_in.kind` is `password` gets a username and
password form on the sign-in page, posted as JSON to its public route `sign-in`:

```json
{ "username": "ada", "password": "…", "return_to": "/p/kb/" }
```

It answers as a redirect provider's callback does: `{"token": "…", "expires_at": "…",
"first": false, "return_to": "/p/kb/"}`. For a one-time password it answers
`{"change_password": true, "ticket": "…", "username": "ada", "return_to": "/p/kb/"}` instead,
and the page asks for a new password, which it posts with the ticket to the public route
`password`: `{"ticket": "…", "password": "…", "return_to": "/p/kb/"}`. That answers with the
session. A refusal is a problem; `401` reads as a wrong username or password.

`organisations` and `teams` are optional; write teams as `<organisation>/<team>`. Every sign-in is
published on `platform.iam.user.signed-in` with them and with whether it is the user's first, and
the RBAC plugin's onboarding rules match on them.

**Linking an account** to someone who is signed in. It starts from their account page, where core
gives the provider a ticket through its `internal/link` route:

```json
{ "ticket": "doc_lnk_…", "return_to": "/account?linked=github" }
```

The provider answers `{"location": "https://…"}`, which is where to send the browser to sign in.
It keeps the ticket until its sign-in callback, as it keeps any sign-in's state, and there calls
`POST /plugin/v1/identity/link` in place of `identity`:

```json
{ "ticket": "doc_lnk_…", "provider": "github", "external_id": "583231", "login": "octocat" }
```

Answer: `{"user_id": "…", "merged": null}`. A ticket names who asked and which provider, is good
for one use within ten minutes, and is `403` otherwise. **Whoever holds a ticket can link an
account to that person**, so it goes nowhere but core and the provider, and never into a URL:

- An account that belongs to someone who has never signed in brings that user's record over.
  `merged` is their old ID.
- An account someone else has signed in with is `409`, for an admin to decide.

The callback then answers the frontend with `{"linked": true, "return_to": "…"}` rather than a
session.

**Describing a directory**, `POST /plugin/v1/users`, with `provider`, `external_id`, `login`, and
optionally the profile and `organisation`, the name of the organisation a new user joins; without
it they join the one that signs in with the provider. A team provider (#9.10) may make this call
too. Answer:
`{"user_id": "…", "created": true}`. This is the user the account belongs to, made with it if
nobody has it, and placed in every default team like anyone new. It signs no one in, so people can
be given access before they first sign in.

**Somebody who has left**, `POST /plugin/v1/identity/deprovision`, with
`{"external_id": "583231", "reason": "left the company"}`. The provider says only that the account
is gone from its directory; core decides nothing by itself. Answer:
`{"user_id": "…", "login": "ada"}`, or `{}` when no account here matches, which is not an error —
a directory may speak of people this platform never knew. Core audits `iam.user.deprovisioned` and
announces it, and the offboarding rules (#9.11) decide what happens to them. Nothing else changes
for them here on this call alone.

Core announces changes to users on `platform.iam.*`:

| Topic | When |
|---|---|
| `user.created` | A user is made before they sign in, by an admin or a provider |
| `user.identity.linked`, `user.identity.unlinked` | An account is linked or unlinked |
| `user.merged` | `{"from", "into", "login"}`: a user who never signed in was merged into another. A plugin that keeps anything per user moves it from `from` to `into`, as RBAC moves grants |
| `user.deprovisioned` | `{"user", "provider", "login", "reason"}`: a provider says they are gone from its directory (#9.11) |
| `user.offboarded` | `{"user", "disabled", "identities_removed", "memberships_removed", "tokens_revoked", "reason"}`: what was done about them |

### 9.10 `teams`: team providers only

A team provider, such as GitHub, keeps its own teams in core, with the
`team-provider` capability. It changes only the teams and memberships it made, so its sync and an
admin's changes never undo each other: people an admin adds to its teams stay through every sync,
and an admin cannot rename, move or delete its teams, only mark them default. It makes users for
its accounts with `POST /plugin/v1/users` (#9.9) before it names them as members.

**A team**, `POST /plugin/v1/teams`:

```json
{ "organisation": "acme", "external_id": "acme/qa", "name": "qa", "title": "QA",
  "description": "", "parent": "acme/platform" }
```

Answer: `{"team_id": "…", "created": true}`. The team is keyed by `external_id`, which the
provider chooses and never reuses. The organisation must exist; it is named by its `name`, or left
empty for the one that signs in with this provider, as making a user does (#9.9) — a provider of
both is told which organisation it works for once. A team does not move between organisations, so
naming another one later is `409`. The
`parent`, if given, is another of the provider's teams, by its key, in the same organisation. A name
taken by another team in the organisation is `409`. Calling it again brings the team up to date.

**Its members**, `POST /plugin/v1/teams/members`, with `{"external_id": "acme/qa", "users": […]}`
and every member as a DOC user ID. Answer: `{"added": […], "removed": […]}`. The provider's
memberships of the team become exactly `users`, and nobody else's change. Someone who is not a user
is left out.

**A team it no longer has**, `POST /plugin/v1/teams/remove`, with `{"external_id": "acme/qa"}`.
Answer: `{"removed": true, "released": false}`. Its memberships go. The team goes too, unless
something made in DOC depends on it, such as people added by hand, a sub-team, a service account it
owns or its default mark. In that case it stays as a team made in DOC (`released`). A team with
sub-teams of the provider's own is `409`: remove those first.

A provider that also signs people in says who has gone from its directory with
`POST /plugin/v1/identity/deprovision` (#9.9); the offboarding rules then decide what happens to
them (#9.11). A sync that no longer lists somebody is exactly that.

Core announces changes to organisations and teams on `platform.organisation.created`, `.changed`
and `.deleted`, and `platform.team.created`, `.changed` and `.deleted`. A change of members is a
`platform.team.changed` with `{"id", "members": {"added": […], "removed": […]}}`; a new lead is
`{"id", "lead"}`, a change to the team's positions `{"id", "positions": [names]}`, and the position
someone holds `{"id", "member_position": {"user", "position"}}`. A change to an organisation's
positions is a `platform.organisation.changed` with `{"id", "positions": [names]}`.

### 9.11 `offboard`: offboarding plugins only

`POST /plugin/v1/offboard` carries out what an offboarding rule decided about somebody who has
left, and needs the `offboarding` capability. It is one call so that it is one audited act:

```json
{ "user": "…", "disable": true, "remove_identity": "oidc",
  "remove_provided_memberships": "oidc", "remove_memberships": false,
  "revoke_tokens": true, "reason": "ada left oidc (left the company)" }
```

Every part is optional, so a rule does as much or as little as it says:

| Field | What it does |
|---|---|
| `disable` | They can no longer sign in, whatever they hold |
| `remove_identity` | Takes away their account with that provider, so signing in with it makes a new user rather than reaching this one |
| `remove_provided_memberships` | Takes away the team memberships that provider gave them; what an administrator added stays |
| `remove_memberships` | Takes away every team membership, however it came about |
| `revoke_tokens` | Their sessions and personal access tokens stop working at once |

Answer: `{"disabled": true, "identities_removed": 1, "memberships_removed": 0,
"tokens_revoked": 1}`, counting what actually changed. Core audits `iam.user.offboarded` with those
counts and the `reason`, and announces it. Nothing is decided here: a platform with no offboarding
rule offboards nobody.

### 9.12 `organisations/write` and `teams/write`: writing the platform's teams

Organisations and teams are the platform's, not any plugin's. A plugin that shows them
as part of what it keeps — the Catalogue does, so that services, repositories and roles can be
connected to a team — makes and changes them here, with the `team-writer` capability. What it
writes is a team made in DOC, which admins rename, move and delete as they do any other; it is not
a provider's team, and a provider's team is left as the provider has it.

**An organisation**, `POST /plugin/v1/organisations/write`:

```json
{ "name": "payments", "title": "Payments", "description": "Taking card payments." }
```

Answer: `{"organisation_id": "…", "created": true}`. It is found by `name`; its title and
description are brought up to date, and nothing else about it is touched.

**A team**, `POST /plugin/v1/teams/write`:

```json
{ "organisation": "payments", "name": "payments-core", "title": "Payments Core",
  "description": "Owns the card gateway.", "email": "payments-core@acme.example",
  "parent": "payments" }
```

Answer: `{"team_id": "…", "created": true}`, as #9.10. The team is found by `name` in its
organisation, which is the one named, else the one that signs in with this plugin, else the only
one there is; with more than one and none named it is `409`. `parent`, if given, is another team of
that organisation, by name. For a team a provider keeps, only `email` is written: the rest is the
provider's.

**Who may ask.** A team decides who holds what, so when the plugin is acting for somebody — it
sends the context token of the call it is answering (#5.1) — that person must administer identity
themselves, as they must to use `/api/v1/teams`; otherwise it is `403`. With nobody asking, the
plugin writes as itself, which is how it moves records it kept before into core. Both are audited,
naming the plugin, and announced on `platform.organisation.*` and `platform.team.*` as #9.10.

### 9.13 Picking a resource

Wherever a plugin's form asks somebody to name a resource, it offers the Catalogue's picker rather
than asking them to remember `Kind:name`. The field is a `doc-combo`, and its options come from the
Catalogue's `ui/options` route, which answers with the resources the viewer can see:

```html
<div class="doc-combo" data-doc-combo>
  <input class="doc-input doc-combo__input" id="resource" name="resource" value=""
         autocomplete="off" role="combobox" aria-expanded="false" aria-autocomplete="list"
         aria-controls="resource-options"
         hx-get="/p/resources/options?field=resource" hx-params="resource"
         hx-trigger="focus, input changed delay:200ms" hx-target="#resource-options"
         hx-swap="innerHTML" />
  <div class="doc-combo__list" id="resource-options" role="listbox" hidden></div>
</div>
```

`field` names the form field whose value is the search text, since a field carries its own name;
`kinds`, such as `kinds=Service,Team`, narrows it to what the field takes. The frontend's
`doc-combo.js` opens and closes the list, moves through it with the arrow keys and puts the chosen
`Kind:name` in the field. Without the script the field is still a text box somebody can type into,
and what it submits is the same.

**Picking a person.** A field that names somebody uses the platform's people picker,
`hx-get="/people/options?field=<the field's name>"`, which offers their login and name. A field
that needs their user ID adds `values=id`. **An ID is never what anyone reads**: every option
carries the ID as `data-value` and the login as `data-label`, and `doc-combo.js` shows the label
while the form is sent the ID in its place — as long as the field still says what was chosen, so a
template typed over it, such as `{{ payload.to }}`, is sent as written. A page drawing a field
whose person is already chosen draws their login with the ID beside it:

```html
<input class="doc-input doc-combo__input" name="user" value="acolwill"
       data-doc-combo-chosen="0192…" data-doc-combo-shown="acolwill" … />
```

A field that must never be sent anything but an ID puts an `<input type="hidden"
class="doc-combo__value">` in the combo instead: the ID goes there, and typing clears it. The same
goes for anything else with an ID of its own: an option whose `data-value` is not what a person
would call it carries a `data-label`.

### 9.14 `settings`: the plugin's own settings

`POST /plugin/v1/settings` with no body answers what this plugin is configured with. The plugin is
the one its registration token names, so this can only ever hand a plugin its own settings.

```json
{
  "values": { "base-url": "https://acme.atlassian.net", "projects": ["OPS", "PLAT"] },
  "secrets": { "api-token": "…" },
  "features": { "sync": true },
  "missing": []
}
```

Every setting the manifest declares is there, at its stored value or its default, so a plugin never
has to ask whether anybody set one. `missing` is the required settings nothing has set: say so and
serve what does not need them, rather than failing. A duration arrives as the seconds it comes to,
and a list as an array of lines. The SDK reads this before every `load` and again whenever core
says the settings changed, so `backend.settings()` and `backend.feature("sync")` cost nothing.

### 9.15 `access-requests`: asking an administrator

A plugin refused something that only another plugin's settings decide asks for it here, rather
than failing with instructions nobody reads. What can be asked for is narrow: to add **the calling
plugin's own ID** to a `list` setting the other plugin declared `requestable`, such as `github`'s
`archive-plugins`. Nothing else about another plugin's settings can be asked for or changed.

`POST /plugin/v1/access-requests`, `backend.request_access(plugin, setting, reason)` in the SDK:

```json
{ "plugin": "github", "setting": "archive-plugins", "reason": "To scan repositories with ccc." }
```

Answer: `{"state": "pending", "id": "0192…", "raised": true, "decided_by": null, "decided_at": null}`.

| `state` | Means |
|---|---|
| `granted` | The plugin is in the list already; try again |
| `pending` | An administrator has been asked. `raised` is true only for the call that asked |
| `denied` | An administrator said no; asking again tells nobody |

There is one request per plugin, target and setting, so asking every time access is refused is
safe. The first ask puts a notification in the inbox of everyone who may change the target's
settings (`plugin:<target>:settings:rw`, and platform admins), from the plugin that asked, linking
to the target's Settings page. That page lists the requests, waiting ones first, with **Approve**
and **Deny** for whoever may change them: `POST /api/v1/plugins/{target}/access-requests/{id}/approve`
or `…/deny`. Approving adds the plugin to the list as it stands, keeping what was there, through an
ordinary save of the setting, so it is checked, audited and passed to the target like any other; a
decision is audited as `plugin.access.approved` or `plugin.access.denied`. A request is decided
once (`409` after). One approved and then taken out of the list is raised again the next time the
plugin asks. Refusals: `400 not-requestable`, `400 bad-request` for a plugin asking itself, and
`404` for a target that is not a plugin here.

Core announces each change to the plugin that asked, which may subscribe to its own:

| Topic | Published when | Fields |
|---|---|---|
| `platform.plugin.<id>.access.requested` | Its request was raised | `id`, `plugin` (the target), `setting`, `state` |
| `platform.plugin.<id>.access.approved` | An administrator approved it | the same, and `decided_by` |
| `platform.plugin.<id>.access.denied` | An administrator denied it | the same, and `decided_by` |

### 9.16 `tokens/scoped`: token issuers only

A **scoped token** (`doc_scp_…`) belongs to a person and is limited to plugins' user permissions,
such as `plugin:kb:user:ro`, for minutes: 1 to 480, 60 unless asked. It is how a person hands an
agent just enough of their access for one job. It holds, on each plugin its scopes name, the lesser
of the scope and what its holder holds there at the time, and nothing anywhere else:

- it reaches only `/api/v1/plugins/{id}/api|ui/…` for the plugins its scopes name; every core
  route, `/api/v1/me` included, is `403`;
- it is never an administrator, whoever holds it, and custom permissions and groups do not come
  with it;
- a call made with it is not passed on to another plugin as its holder (`services`, other than a
  `discovery/` route, is `403`), a task started during it runs as the plugin, and it cannot delegate or
  mint another token;
- it ends by itself, and is revoked like any of its holder's tokens.

People mint their own with `POST /api/v1/tokens/scoped` (`{"name", "scopes", "expires_in_minutes"}`,
answered once with the token), list the ones in force with `GET /api/v1/tokens/scoped`, and revoke
them with `DELETE /api/v1/tokens/{id}`; the **Access tokens** page lists them too.

A plugin with the `token-issuer` capability mints one for the person its call is for, during that
call, with `POST /plugin/v1/tokens/scoped`, `backend.scoped_token(request)` in the SDK:

```json
{ "name": "Data Vacuum: Engineering wiki", "scopes": ["plugin:vacuum:user:rw", "plugin:kb:user:ro"],
  "expires_in_minutes": 60 }
```

Answer: `{"id", "token", "scopes", "expires_at"}`. The token is in this answer and nowhere else;
a plugin should hand it straight to the person and keep only its `id`. Every scope must be a
plugin's user permission its holder holds at least in part (`400` otherwise); core, `*`, groups and
custom permissions never are. It is audited as `auth.token.scoped.created` under the person, naming
the plugin. `POST /plugin/v1/tokens/scoped/revoke` with `{"id"}`, `backend.revoke_scoped_token(id)`,
revokes one the plugin minted, whoever holds it, and answers `{"revoked": true}` if it was in force.

### 9.17 `seal`, `open` and `secrets/changed`: the secret store only

The plugin with the `secret-store` capability keeps credentials for the rest of the platform
without ever holding a key: core seals and opens its values under the settings key (#4.4), with
the plugin's ID and a `label` naming the record bound in, so a value opens for that plugin and
that record and nothing else.

`POST /plugin/v1/seal`, `backend.seal(label, value)`:

```json
{ "label": "secret/0192…", "value": "…" }
```

Answer: `{"sealed": {"key_id": "2", "nonce": "…", "ciphertext": "…"}}`, base64 for the last two.
The plugin keeps `sealed` in its own collections: the database holds ciphertext only.

`POST /plugin/v1/open` with `{"label", "sealed"}`, `backend.open(label, sealed)`, answers
`{"value": "…", "stale": false}`. `stale` is true for a value sealed under a key older than the
current one, which the plugin seals again, so a `rotate-settings-key` reaches its values too. A
label the value was not sealed with is `400`; a key the platform no longer has is `409`.

`POST /plugin/v1/secrets/changed` with `{"secrets": ["0192…"], "loaded": false}`,
`backend.secrets_changed(secrets, loaded)`, says which of the store's secrets changed. Core tells
every plugin whose settings point at one that its settings changed (#7.10), and answers
`{"told": ["jira"]}`. `loaded` says the store has just started, so the plugins that could not reach
it are told too.

Core asks the store in turn, as the platform, on two `internal/` routes: `resolve` with
`{"plugin", "secrets": [ids]}`, answered `{"secrets": {"<id>": {"label", "value"} or {"label",
"problem"}}}`, and `shared` with `{"plugin"}`, answered `{"secrets": [{"id", "label", "hint"}]}` for
the Settings page.

### 9.18 `people`: adding somebody, for a person

A plugin with `team-writer` adds somebody by email address for the person its call is for —
never for itself — under exactly the rules `POST /api/v1/people` holds that person to:
a team's lead adds into the teams they arrange, an identity manager anywhere, and the address's
domain must be one the organisation approved.

`POST /plugin/v1/people`, `backend.add_person(request)`:

```json
{ "email": "grace@acme.com", "name": "Grace Hopper", "team": "0192…" }
```

Answer: what `POST /api/v1/people` answers. Without a person behind the call it is `403`.

### 9.19 Telemetry: a week, and no more

DOC keeps **seven days** of telemetry about any service or plugin: the fine-grained series it
samples itself — how often each thing was checked, how long it was watched, how it answered. It is
a platform for seeing how things stand, not a time-series database, and a week is what answers
"what happened last night" without becoming the place an organisation's history lives.

A plugin may declare a setting for how long to keep its own telemetry, and **must hold that
setting to seven days** unless a plugin with the `telemetry-sink` capability is registered, which
it reads from `core.plugins.telemetry`. While one is, what is kept here has stopped being the only
copy, and the setting stands as written.

What is *derived* from telemetry is not telemetry and is not held to the week: a day's totals, an
outage, a deployment, a grade. Reliability keeps five minutes at a time for a week and the day's
totals for as long as it keeps outages, so its year-long views lose nothing when the week rolls
over — only its hour-long ones need what is kept finely.

Core holds its own status history to the same rule: seven days, or thirty once a sink is
registered.

## 10. Errors

Every error body, in both directions, is an RFC 9457 problem with `content-type:
application/problem+json`:

```json
{ "type": "/problems/forbidden", "title": "forbidden", "status": 403,
  "detail": "needs plugin:hello:pluginuser:greetings:rw", "plugin": "hello", "version": "1.2.0" }
```

`plugin` and `version` are optional. The backend shows `detail` to operators when a plugin's call
fails, so write it for a person.

## 11. Building the UI

### 11.1 How fragments are shown

A plugin's `ui/*` routes return **HTML fragments, not pages**. The frontend shows
`/p/{plugin}/{path}` by fetching `ui/{path}` as the signed-in person and wrapping the fragment in
the DOC layout (header, navigation and toasts). A request from HTMX gets the fragment on its own.

- **No `<html>`, `<head>`, `<body>`, `<script>` or `<style>`, and no `style=""` attributes.** The
  Content Security Policy allows only the platform's own assets, and plugins cannot ship CSS.
  Everything visual comes from the `doc-*` classes below. They are the complete styling contract,
  and each one works without any other class.
- **Escape every value** you put in HTML, including names, log lines and error messages.
- Requests from HTMX carry `hx-request`, and `hx-*` headers pass in both directions. A fragment can
  refresh itself or load more with `hx-get` pointing at another `ui/` route of the same plugin.
- **Nothing that HTMX 4 evaluates as code.** The policy has no `unsafe-eval`, so `hx-on:*`
  handlers, `js:` or `javascript:` values in `hx-vals` and `hx-headers`, and `hx-trigger` filters
  in `[...]` do nothing. Anything that changes data must go through HTMX (`hx-post`, `hx-put`,
  `hx-patch`, `hx-delete`), since the layout adds the page's CSRF token to those as a header and a
  fragment cannot know it; a plain `<form method="post">` from a plugin is refused.
- Link between your own pages with `/p/<id>/<path>`.
- **Live updates:** publish to `plugin.<id>.ui.<name>` and every open page of someone who can read
  your plugin hears it. An element with `hx-trigger="doc-plugin-ui-<id> from:body"` then refreshes
  itself, and `doc-plugin-state-<id>` fires when your plugin changes state.
- Return `content-type: text/html; charset=utf-8`.

### 11.2 Choosing a presentation

Choose by the shape of the data, not by where it came from:

| The data is… | Show it as | Classes |
|---|---|---|
| A few headline numbers | Stat tiles | `doc-stat-group`, `doc-stat` |
| One record's fields | A summary list | `doc-summary` |
| Many records with the same fields | A table | `doc-table` |
| A collection of things to open | Cards | `doc-card-group`, `doc-card` |
| An ordered list of steps or actions: a pipeline, an approval flow, a history | A timeline | `doc-timeline` |
| Output from a process: a CI log, a deployment or a command | A terminal | `doc-terminal` |
| A file, request, config or JSON value to read or copy | A code block | `doc-code` |
| Files, and the directories holding them | A tree | `doc-tree` (#11.20) |
| An identifier, hash, version or path inside text | Inline mono | `doc-mono` |
| A lifecycle or health state | A badge | `doc-badge` |
| Things and how they connect | A graph | `doc-graph` |
| Something put together from ready-made parts, in columns | A builder | `doc-builder` (#11.21) |
| How numbers move over time | A chart | `doc-chart` with `data-chart` (#11.22) |
| Where things are in the world | A map | `doc-geomap` with `data-geomap` (#11.24) |
| Things placed by kind and by stage, such as a tech radar | A radar | `doc-radar` |
| Narrowing what one large thing shows | Filters beside it | `doc-filters` |
| Things spread over days, such as releases or support windows | A timeline of bars | `doc-gantt` (#11.23) |
| A document written elsewhere, as HTML | Prose | `doc-prose` |
| Nothing yet | An empty state | `doc-empty` |
| Input | A form | `doc-form-group`, `doc-label`, `doc-input`, `doc-select`, `doc-textarea`, `doc-button` |
| One choice among a few | Radios | `doc-radios` |
| Any number of a few | Checkboxes | `doc-checkboxes` |
| Input that names something as it is typed | Suggestions | `doc-suggest` with `docSuggest` (#11.13) |
| What something is about: resources, labels | Tags, chosen from suggestions | `doc-tags` with `docTags` (#11.15) |

A chart goes with the stat tiles it explains, never instead of them: the tiles say where things
stand, the chart how they got there.

### 11.3 Stat tiles

```html
<div class="doc-stat-group">
  <div class="doc-stat">
    <span class="doc-stat__label">Builds today</span>
    <span class="doc-stat__value">128</span>
    <span class="doc-stat__note">12 more than yesterday</span>
  </div>
  <div class="doc-stat">
    <span class="doc-stat__label">Success rate</span>
    <span class="doc-stat__value">96.1%</span>
    <span class="doc-stat__note">Last 7 days</span>
  </div>
</div>
```

Use at most four or five tiles, each a number the reader acts on. Put units in the value
(`4m 12s`, `96.1%`). The tiles wrap onto new rows on narrow screens.

### 11.4 Summary list

```html
<dl class="doc-summary">
  <div class="doc-summary__row">
    <dt class="doc-summary__key">Service</dt>
    <dd class="doc-summary__value">payments-api</dd>
  </div>
  <div class="doc-summary__row">
    <dt class="doc-summary__key">State</dt>
    <dd class="doc-summary__value"><strong class="doc-badge doc-badge--ready">Running</strong></dd>
  </div>
  <div class="doc-summary__row">
    <dt class="doc-summary__key">Commit</dt>
    <dd class="doc-summary__value"><code class="doc-mono">9f2c1e7</code></dd>
  </div>
</dl>
```

Use a summary list for one record, where a table would have a single row. Keys and values stack on
narrow screens.

### 11.5 Table

```html
<table class="doc-table">
  <caption class="doc-table__caption">Recent builds</caption>
  <thead class="doc-table__head">
    <tr class="doc-table__row">
      <th class="doc-table__header" scope="col">Build</th>
      <th class="doc-table__header" scope="col">State</th>
      <th class="doc-table__header doc-table__header--numeric" scope="col">Duration</th>
    </tr>
  </thead>
  <tbody class="doc-table__body">
    <tr class="doc-table__row">
      <td class="doc-table__cell"><a href="/p/ci/builds/4812">#4812</a></td>
      <td class="doc-table__cell"><strong class="doc-badge doc-badge--error">Failed</strong></td>
      <td class="doc-table__cell doc-table__cell--numeric">4m 12s</td>
    </tr>
  </tbody>
</table>
```

Right-align numbers with the `--numeric` modifiers. Put a table inside a card with
`doc-card-group--wide` when it sits alongside others.

### 11.6 Timeline: steps and actions

Use a timeline for anything that happens in order: pipeline stages, an approval flow, a
provisioning run or a record's history. Give each item one state modifier, and the marker is drawn
from it:

| Modifier | Marker | Use for |
|---|---|---|
| `doc-timeline__item--done` | Green tick | Finished successfully |
| `doc-timeline__item--running` | Purple ring | In progress. Have at most one |
| `doc-timeline__item--failed` | Red cross | Failed. Say why in the body |
| `doc-timeline__item--skipped` | Dashed dash, muted text | Didn't run because of an earlier step or a condition |
| `doc-timeline__item--pending` | Empty circle, muted text | Not reached yet |

```html
<ol class="doc-timeline">
  <li class="doc-timeline__item doc-timeline__item--done">
    <span class="doc-timeline__title">Request submitted</span>
    <span class="doc-timeline__meta">Ada Lovelace · 09:14</span>
  </li>
  <li class="doc-timeline__item doc-timeline__item--running">
    <span class="doc-timeline__title">Provisioning</span>
    <span class="doc-timeline__meta">Started 09:41</span>
    <div class="doc-timeline__body"><p>Creating the database cluster in eu-west-2.</p></div>
  </li>
  <li class="doc-timeline__item doc-timeline__item--failed">
    <span class="doc-timeline__title">DNS record</span>
    <span class="doc-timeline__meta">Failed 09:43</span>
    <div class="doc-timeline__body">
      <p>The zone <code class="doc-mono">internal.example</code> refused the change.</p>
    </div>
  </li>
  <li class="doc-timeline__item doc-timeline__item--pending">
    <span class="doc-timeline__title">Hand over to owner</span>
  </li>
</ol>
```

- List steps **oldest first**, in the order they run.
- The title says what the step *is* (`DNS record`), not what happened to it. The modifier and the
  meta line say what happened.
- The meta line says who and when: `Ada Lovelace · 09:14`, `Started 09:41` or `Took 2m 3s`.
- Put detail in `doc-timeline__body`: an error, a link to the log, or a nested `doc-terminal`.
- For a pipeline that is still running, refresh the list with
  `hx-get="/p/<id>/runs/42/steps" hx-trigger="every 5s"` and stop polling when nothing is `running`.

### 11.7 Terminal: logs and process output

Use a terminal for output a process wrote, such as a CI runner's log, a deployment or a command's
output, so that it reads as it did in a terminal. It stays dark on purpose.

```html
<figure class="doc-terminal doc-terminal--numbered">
  <figcaption class="doc-terminal__header">
    <span>build #4812 · test</span>
    <strong class="doc-badge doc-badge--error">Failed</strong>
  </figcaption>
  <pre class="doc-terminal__body">
    <span class="doc-terminal__line doc-terminal__line--command"><span class="doc-terminal__prompt">$ </span>cargo test --workspace</span>
    <details class="doc-terminal__group">
      <summary>Compiling 214 crates</summary>
      <span class="doc-terminal__line doc-terminal__line--muted">   Compiling serde v1.0.228</span>
      <span class="doc-terminal__line doc-terminal__line--muted">   Compiling tokio v1.48.0</span>
    </details>
    <span class="doc-terminal__line"><span class="doc-terminal__time">09:14:07 </span>running 38 tests</span>
    <span class="doc-terminal__line doc-terminal__line--success">test ledger::balances_add_up ... ok</span>
    <span class="doc-terminal__line doc-terminal__line--warning">warning: unused variable: `retry`</span>
    <span class="doc-terminal__line doc-terminal__line--error">test ledger::refunds_are_idempotent ... FAILED</span>
  </pre>
  <div class="doc-terminal__footer">
    <a href="/p/ci/builds/4812/log" hx-get="/p/ci/builds/4812/log?from=1" hx-target="closest .doc-terminal" hx-swap="outerHTML">Show the full log (2,310 lines)</a>
  </div>
</figure>
```

| Class | Use for |
|---|---|
| `doc-terminal__line` | **One line of output per element.** Whitespace inside it is kept. Newlines between lines are ignored, so indent your template freely |
| `--command` + `doc-terminal__prompt` | A command the runner executed, shown bold with a `$ ` prompt |
| `--success`, `--warning`, `--error` | A line's severity. Error lines are also highlighted |
| `--muted` | Noise that is useful only when digging: compiler progress, downloads |
| `doc-terminal__time` | A timestamp at the start of a line |
| `doc-terminal__group` | A `<details>` with a `<summary>` that folds one step's lines away. Leave the failing step `open` |
| `doc-terminal--numbered` | Adds line numbers |
| `doc-terminal--wrap` | Wraps long lines instead of scrolling sideways. Use it for prose-like logs |
| `doc-terminal__header`, `__footer` | The title with a state badge; links to the full or raw log |

Converting raw output:

- **Escape every line** before wrapping it. Logs contain `<`, `&` and sometimes deliberate HTML.
- **Convert ANSI colour codes to line classes, and strip the rest.** Treat red as `--error`, yellow
  as `--warning`, green as `--success`, and dim or grey as `--muted`. Remove cursor movement and
  other escape sequences. Plugins cannot colour individual words.
- Turn runner step markers into groups. For example, GitHub Actions' `##[group]…##[endgroup]` and
  GitLab's `section_start`/`section_end` each become one `doc-terminal__group`.
- **Show the tail, not the whole log.** Render the last 200–500 lines, open on the failing step, and
  let a footer link load the rest. Keep fragments under about 1 MiB. The body scrolls after 32 rem.
- For a live log, append with `hx-get="…/log?from=<next line>" hx-trigger="every 2s"
  hx-swap="beforeend"` targeting the body, and stop when the run ends.

### 11.8 Code blocks and inline mono

```html
<pre class="doc-code"><code>{
  "service": "payments-api",
  "replicas": 3
}</code></pre>

<p>Deployed <code class="doc-mono">payments-api@1.4.2</code> from <code class="doc-mono">9f2c1e7</code>.</p>
```

A code block is content to read or copy, such as a manifest, a request body or a config file. It
is drawn dark, on VS Code's own editor background, because that is what most people read code on.
A terminal is output that a process wrote, and keeps its own dark. Pretty-print JSON before
escaping it. Use `doc-mono` for identifiers inside sentences and table cells, never for whole
paragraphs; `code.doc-code` is the same thing and stays light, since it sits in a sentence.

**Syntax is coloured by the platform, not by the plugin.** Say what a block holds and the frontend
marks it up:

```html
<pre class="doc-code" data-doc-language="rust"><code>let flags = Flags::start(&amp;config).await?;</code></pre>
<pre class="doc-code" data-doc-filename="deploy/kubernetes/deployment.yaml"><code>…</code></pre>
```

`data-doc-language` names it; `data-doc-filename` lets the name say it, which is what a plugin
showing a file already has. Either one is enough. Go, Rust, Python, C++, TypeScript, shell, YAML,
TOML, JSON, protobuf, SQL, Dockerfiles, Makefiles and env files are known, under the names and
extensions people write; anything else is left plain, as is a block that says nothing.

Seven things are coloured, in **VS Code's Dark+ palette**:

| Class | Is | |
|---|---|---|
| `doc-code__comment` | A comment | 5.0:1 |
| `doc-code__keyword` | A word of the language | 5.7:1 |
| `doc-code__string` | A string | 6.3:1 |
| `doc-code__type` | A name with a capital letter, taken for a type | 8.2:1 |
| `doc-code__number` | A number | 9.8:1 |
| `doc-code__key` | A setting's name, before its value | 11.2:1 |
| `doc-code__function` | A name with a bracket after it, taken for a call | 11.8:1 |

The last column is each one's contrast against the block, so every one of them clears AA and most
clear AAA. A type and a call are read off the shape of the name rather than understood — a
constant in capitals is drawn as a type — which is the cost of having no grammar, and it costs a
colour rather than a meaning.

**Colour never carries meaning here**: it only repeats what the word already is, so a block reads
the same before the script runs, on paper, where it prints as dark text on the page's own grey,
and under `prefers-contrast: more`, where the hues drop out. A plugin never writes `doc-code__*`
itself.

### 11.9 Badges

`<strong class="doc-badge doc-badge--ready">Running</strong>`. Choose the modifier by meaning:

| Modifier | Meaning |
|---|---|
| `--ready`, `--up` | Healthy, succeeded or running |
| `--loading` | Starting, queued or in progress |
| `--unloading` | Stopping or draining |
| `--degraded` | Working with problems, or a warning |
| `--error`, `--down` | Failed or unavailable |
| `--unknown` | No information |

The badge text says the state in words, because colour alone is not enough.

### 11.10 Cards, empty states, forms, buttons and tabs

```html
<div class="doc-card-group">
  <div class="doc-card">
    <h3 class="doc-card__heading"><a href="/p/kb/spaces/platform">Platform</a></h3>
    <div class="doc-card__content"><p>214 documents · synced 5 minutes ago</p></div>
    <div class="doc-card__footer">Owned by platform-team</div>
  </div>
</div>

<div class="doc-empty">
  <p>No builds have run for this service yet.</p>
  <a class="doc-button doc-button--secondary" href="/p/ci/builds/new">Run a build</a>
</div>

<form hx-post="/p/hello/greetings" hx-target="this" hx-swap="outerHTML">
  <div class="doc-form-group">
    <label class="doc-label" for="name">Name</label>
    <span class="doc-hint">Who to greet</span>
    <input class="doc-input" id="name" name="name" type="text">
  </div>
  <div class="doc-button-group">
    <button class="doc-button" type="submit">Greet</button>
    <a class="doc-button doc-button--secondary" href="/p/hello/">Cancel</a>
  </div>
</form>
```

- An empty state says why the list is empty and offers the next action. Never show an empty table.
- A form with an error adds `doc-form-group--error` to the group, a
  `<span class="doc-error-message">` above the field, and the field's `--error` modifier
  (`doc-input--error`, `doc-select--error` or `doc-textarea--error`).
- Buttons: `doc-button` for the main action, `--secondary` for the others, `--warning` for
  destructive actions and `--small` inside tables.
- Tabs split one page into views, each with its own address. They are links: render only the
  chosen view, and mark its tab with `aria-current="page"`.
- A plugin's sections (its lists, its templates, its settings) are tabs, never a row of buttons.
  Buttons are for actions, such as "New event" or "Edit", and sit beside the heading they act on,
  at the container's right edge, in a heading row. A form's own submit button stays at its end,
  and a page that is a form offers no other action.
- The layout draws breadcrumbs above a page's tabs, or above its heading when it has none. For a
  plugin's page it takes the page's name from its first `<h2>`, and its place from the first
  `doc-tabs__tab` marked `aria-current="page"`, so start each page with its tabs and one `<h2>`.
  A page deeper than its tabs names the steps between with hidden links, in order, which go after
  the tab: the Catalogue's page for a repository carries
  `<a class="doc-trail" href="/p/resources/kinds/repository" hidden>Repository</a>`, so it reads
  Home › Platform › Catalogue › Repository › acme/card-gateway. Only links within the platform count.
- Every menu on the bar is a page of its own at `/menu/<name>`, listing what is in it as cards with
  each entry's description — a plugin's `described(…)` on its navigation entry. The breadcrumb on
  every page under a menu leads there, and the menu itself opens with a link to it, so a menu is
  somewhere to go rather than only a word above a page.
- A page that presents information rather than actions, such as a document, puts its
  collection's contents on the left in `doc-with-contents`, marks the page being read with
  `aria-current="page"`, and ends with `doc-pagination` to the pages either side. The contents
  are a `<details class="doc-contents" open>`: open without script, folded on a phone. Name the
  collection in a `<p class="doc-contents__heading">`, not a heading, so the page's own heading
  still names it in the breadcrumbs. A tab whose sections are long enough to be pages is made of
  pages the same way, one section each, with `doc-pagination` to the ones either side: a plugin's
  Settings tab is, one page per heading it grouped its settings under (`?section=<heading>`), plus
  a page for the requests other plugins have made and one for the credentials it holds by name.
  The settings a plugin grouped under nothing are **General**, and a plugin with one page, or
  none, is left as one column. Each page saves only itself — a form says which page sent it, and
  a setting that page did not draw is left alone, or a checkbox nobody was shown would be stored
  as off by a form that never mentioned it.

```html
<div class="doc-with-contents">
  <details class="doc-contents" open>
    <summary class="doc-contents__summary">Contents</summary>
    <nav aria-label="Contents of Payments docs">
      <p class="doc-contents__heading"><a href="/p/kb/spaces/ops">Payments docs</a></p>
      <ul class="doc-contents__list">
        <li class="doc-contents__item"><a class="doc-contents__link" href="/p/kb/docs/ops/index">Overview</a></li>
        <li class="doc-contents__item"><span class="doc-contents__folder">Guides</span>
          <ul class="doc-contents__list">
            <li class="doc-contents__item"><a class="doc-contents__link" href="/p/kb/docs/ops/guides/roll-back" aria-current="page">Rolling back</a></li>
          </ul>
        </li>
      </ul>
    </nav>
  </details>
  <div class="doc-with-contents__main">
    <h2>Rolling back</h2>
    <nav aria-label="Pages either side">
      <ul class="doc-pagination">
        <li class="doc-pagination__item doc-pagination__item--next"><a class="doc-pagination__link" href="/p/kb/docs/ops/faq" rel="next"><span class="doc-pagination__direction">Next</span> <span class="doc-pagination__title">Questions</span></a></li>
      </ul>
    </nav>
  </div>
</div>
```
- A long form asks one thing per page, as a wizard: a `doc-step` line ("Step 2 of 5"), the
  question as the `<h2>`, `doc-radios` for a choice, and a last page that lists every answer in a
  `doc-summary` with a "Change" `doc-link-button` beside each (`doc-summary__row--action`).

```html
<fieldset class="doc-radios">
  <legend class="doc-radios__legend">Sync</legend>
  <div class="doc-radios__item">
    <input class="doc-radios__input" id="sync-daily" name="sync" type="radio" value="daily" />
    <label class="doc-radios__label" for="sync-daily">Every day</label>
    <span class="doc-radios__hint">At 03:00 UTC.</span>
  </div>
</fieldset>
```
- Any number of a few is `doc-checkboxes`, built the same way, every input sharing one `name` so
  the answer arrives as a list. `doc-checkboxes__divider` writes "or" between two groups that are
  alternatives to each other, such as the teams a space belongs to and the organisation.

```html
<fieldset class="doc-checkboxes">
  <legend class="doc-checkboxes__legend">Who looks after this space</legend>
  <div class="doc-checkboxes__item">
    <input class="doc-checkboxes__input" id="owner-0" name="owners" type="checkbox" value="organisation:acme" />
    <label class="doc-checkboxes__label" for="owner-0">Acme</label>
    <span class="doc-checkboxes__hint">Organisation</span>
  </div>
  <p class="doc-checkboxes__divider">or</p>
  <div class="doc-checkboxes__item">
    <input class="doc-checkboxes__input" id="owner-1" name="owners" type="checkbox" value="team:payments" />
    <label class="doc-checkboxes__label" for="owner-1">Payments</label>
    <span class="doc-checkboxes__hint">Team</span>
  </div>
</fieldset>
``` With askama, put an empty
  block in each tab link of `base.html` (`href="/p/hello/"{% block tab_home %}{% endblock %}`) and
  fill it in each page (`{% block tab_home %} aria-current="page"{% endblock %}`).

```html
<div class="doc-heading-row">
  <h2>Your processes</h2>
  <div class="doc-heading-row__actions">
    <a class="doc-button doc-button--secondary doc-button--small" href="/p/process/processes/new">New process</a>
  </div>
</div>

<nav class="doc-tabs" aria-label="ada">
  <ul class="doc-tabs__list">
    <li class="doc-tabs__item"><a class="doc-tabs__tab" href="/p/rbac/principals/user/1" aria-current="page">Details</a></li>
    <li class="doc-tabs__item"><a class="doc-tabs__tab" href="/p/rbac/principals/user/1?tab=permissions">Permissions</a></li>
  </ul>
</nav>
```

### 11.11 Graph

```html
<svg class="doc-graph" viewBox="0 0 520 140" role="img" aria-label="card-gateway and its teams">
  <text class="doc-graph__heading" x="10" y="20">Services</text>
  <text class="doc-graph__heading" x="270" y="20">Teams</text>
  <path class="doc-graph__edge doc-graph__edge--near" data-from="service:card-gateway" data-to="team:payments-core" d="M 210 60 C 240 60, 240 60, 270 60" />
  <path class="doc-graph__edge doc-graph__edge--derived doc-graph__edge--near" data-from="service:card-gateway" data-to="team:risk" d="M 210 60 C 240 60, 240 110, 270 110" />
  <a href="/p/resources/r/service/card-gateway" data-node="service:card-gateway">
    <rect class="doc-graph__node doc-graph__node--focus" x="10" y="42" width="200" height="36" rx="6" />
    <text class="doc-graph__label doc-graph__label--focus" x="22" y="65">card-gateway</text>
  </a>
  <a href="/p/resources/r/team/payments-core" data-node="team:payments-core">
    <rect class="doc-graph__node" x="270" y="42" width="200" height="36" rx="6" />
    <text class="doc-graph__label" x="282" y="65">payments-core</text>
  </a>
  <a href="/p/resources/r/team/payments-core?add">
    <rect class="doc-graph__node doc-graph__node--ghost" x="270" y="92" width="200" height="36" rx="6" />
    <text class="doc-graph__label doc-graph__label--ghost" x="282" y="115">Add a team</text>
  </a>
  <text class="doc-graph__note" x="270" y="125">and 3 more</text>
</svg>
```

A graph shows things and how they connect. Draw it as inline SVG: geometry goes in attributes,
and everything visual comes from these classes. `--focus` marks the node the graph is about, and
`--derived` marks a connection read from somewhere else, and `--ghost` draws dashed a node that is
not there yet, such as a place to add one. The Service Map plugin lays graphs out in columns and
returns the positions, so you don't have to: send `discovery/draw` your items and links, with
`headings` when a column's heading is not its name, an `order` on the items of a column whose
sequence means something, and `ghost` on any not there yet.

A node says which node it is with `data-node`, and an edge says which two it joins with
`data-from` and `data-to`. Hovering or focusing a node then lights the lines that reach it and
outlines the nodes at their far end, and fades the rest of the map back behind them —
`doc-graph.js` does that for every graph on the page, so no plugin needs a line of JavaScript for
it. `--near` marks an edge that reaches whatever the graph is of, drawn in colour without anyone
reaching for it; the Service Map puts it on every edge touching a `--focus` node.

### 11.12 Prose

```html
<div class="doc-prose">
  <h1>Payment flow</h1>
  <p>Card payments go through <code>card-gateway</code> first.</p>
  <pre><code>cli kb search refunds</code></pre>
</div>
```

Prose holds HTML that has no classes of its own, such as a page imported into the Knowledge Base.
Its headings, lists, code, quotes, tables and images are styled by element. Sanitise the HTML
before you return it, since it came from somewhere else.

### 11.13 Suggestions while typing

```html
<div class="doc-suggest" x-data="docSuggest">
  <textarea class="doc-textarea" id="body" name="body" x-ref="field"
            @input.debounce.250ms="lookup" @keydown="navigate" @blur="close"></textarea>
  <input type="hidden" name="typed" form="doc-suggest-none" x-ref="query" hx-get="/p/water/suggest"
         hx-trigger="doc-suggest" hx-target="next .doc-suggest__list" hx-swap="innerHTML">
  <ul class="doc-suggest__list" role="listbox" x-ref="list" x-show="open"
      @mousedown.prevent="choose"></ul>
</div>
```

The layout loads Alpine.js (its CSP build) and one component of its own, `docSuggest`. The field
can be a `doc-input` or a `doc-textarea`. When someone types `/` and at least two more characters,
the component puts those characters in the `query` element and fires `doc-suggest` on it, so HTMX
fetches options from your route. The `form` attribute keeps `typed` out of the form around it.
Answer with list items:

```html
<li><button type="button" class="doc-suggest__option" tabindex="-1"
            data-value="/service:card-gateway">card-gateway <span class="doc-hint">Service</span></button></li>
<li class="doc-suggest__empty">Nothing is called card-x</li>
```

The arrow keys move between options, and Enter, Tab or a click replaces the `/` word with the
option's `data-value` and a space. Escape closes the list. Other Alpine components can't be used,
because the policy runs no code a fragment brings.

### 11.14 Radar

```html
<svg class="doc-radar" viewBox="0 0 760 808" role="img" aria-label="The tech radar">
  <circle class="doc-radar__ring doc-radar__ring--shaded" cx="380" cy="404" r="122" />
  <path class="doc-radar__axis" d="M 20 404 H 740 M 380 44 V 764" />
  <text class="doc-radar__ring-label" x="319" y="398" text-anchor="middle">Adopt</text>
  <text class="doc-radar__quadrant doc-radar__colour--1" x="20" y="26">Techniques</text>
  <a class="doc-radar__entry doc-radar__colour--1" href="/p/radar/entries/trunk-based-development">
    <title>1. Trunk-based development (Adopt, new)</title>
    <circle class="doc-radar__halo" cx="340" cy="370" r="13" />
    <circle class="doc-radar__blip" cx="340" cy="370" r="9" />
    <text class="doc-radar__number" x="340" y="370" text-anchor="middle" dominant-baseline="central">1</text>
  </a>
  <a class="doc-radar__entry doc-radar__entry--faded doc-radar__colour--2" href="/p/radar/entries/kafka">…</a>
</svg>
```

A radar places things in four quadrants and in concentric rings, drawn as inline SVG like a
graph. `doc-radar__colour--1` to `--4` give each quadrant its colour, which its blips, halos and
labels draw with through `currentColor`. The modifiers also work on HTML beside the radar, such as a
legend's headings. `--shaded` alternates the rings, `doc-radar__halo` rings a blip (the Tech Radar
plugin marks new entries this way), and `--faded` pushes an entry back so that the one a page is
about stands out. **It is drawn at twice the width it is shown at**, so the whole of it is taken
in at a glance rather than scrolled past: everything on it — blips, the room between them, the
labels — is sized in the drawing for that halving, which is what a segment holding six entries
apart rather than sixteen buys. A legend is an `<ol class="doc-radar__legend" start="…">` per ring, numbered
as the blips are, with `doc-radar__moved` for a note such as "moved in". The Tech Radar plugin draws
the whole radar, so read its `draw.rs` before drawing one yourself.

**Filters** take the same column where a page's content is one large thing — a map, a board — so
the rest of the width is left to it:

```html
<div class="doc-with-contents">
  <details class="doc-filters" open>
    <summary class="doc-filters__summary">Filters</summary>
    <form method="get" hx-get="/p/service-map/map" hx-trigger="change" hx-target="#service-map">
      <h2 class="doc-filters__heading">Filters</h2>
      <div class="doc-form-group">…a select…</div>
      <details class="doc-filters__more">
        <summary>Show &mdash; 6 of 8</summary>
        <fieldset class="doc-filters__choices">
          <label class="doc-filters__choice"><input type="checkbox" name="kinds" value="Team" /> Team</label>
        </fieldset>
      </details>
      <noscript><button class="doc-button doc-button--secondary doc-button--small" type="submit">Show</button></noscript>
    </form>
  </details>
  <div class="doc-with-contents__main">…the map…</div>
</div>
```

They apply as they change, so there is no Show button but the one in `<noscript>`, which submits
the form to the page instead. `doc-filters__more` folds away a list that is set once and read
rarely, such as which kinds of thing to draw; say in its summary how many are on, so it does not
have to be opened to be read. On a narrow screen the whole column folds above the content, which
is what its own `__summary` is for.

### 11.15 Tags

```html
<div class="doc-tags" x-data="docTags" data-name="tags">
  <ul class="doc-tags__list" x-ref="chosen" @click="remove">
    <li class="doc-tags__tag" data-value="service:card-gateway">
      <span class="doc-tags__label">card-gateway</span> <span class="doc-tags__kind">Service</span>
      <input type="hidden" name="tags" value="service:card-gateway" />
      <button type="button" class="doc-tags__remove" aria-label="Remove card-gateway">×</button>
    </li>
  </ul>
  <input class="doc-input" id="tags" type="text" autocomplete="off" role="combobox"
         aria-autocomplete="list" x-bind:aria-expanded="open" x-ref="field" @focus="lookup"
         @click="lookup" @input.debounce.200ms="lookup" @keydown="navigate" @blur="close" />
  <input type="hidden" name="typed" form="doc-suggest-none" x-ref="query" hx-get="/p/water/tag-options"
         hx-trigger="doc-suggest" hx-target="next .doc-suggest__list" hx-swap="innerHTML" />
  <ul class="doc-suggest__list" role="listbox" x-ref="list" x-show="open"
      @mousedown.prevent="choose"></ul>
</div>
```

`docTags` is the layout's second component. Tags show as badges, and the field suggests them.
Focusing or clicking the field, or typing in it, puts what it holds (perhaps nothing) in the
`query` element and fires `doc-suggest` on it, so HTMX fetches options from your route. Answer with
the same list items as #11.13, adding a `data-label` and a `data-hint` for the badge:

```html
<li><button type="button" class="doc-suggest__option" tabindex="-1" data-value="service:card-gateway"
            data-label="card-gateway" data-hint="Service">card-gateway <span class="doc-hint">Service</span></button></li>
```

Choosing an option, with a click, Enter or Tab after the arrow keys, adds a badge whose hidden
input is named by `data-name` and holds the option's `data-value`, so the form sends every tag
under that name. Options already chosen are hidden. The × button, or Backspace in the empty field,
removes a badge. `data-max="1"` makes a single choice that replaces the last. Render tags chosen
before (after a refused form, say) as badges in the list, as above. To show tags read-only, use the
list alone, with each label a link and no remove button or input. Resource Definitions' `search`
ranks fuzzily and answers with no `q`, so a route can offer resources before anything is typed.

### 11.16 Listings, results and an A to Z

Anything a person scans down to choose from — search results, your records, a catalogue of
collections — is a listing rather than a grid of cards: a title to click, a quiet line saying what
it is, a sentence or two, and a rule between one and the next. This is how the platform's own
search is drawn, so a plugin's list of things reads the same as everything else in DOC.

```html
<p class="doc-results__count">14 results</p>
<ul class="doc-results">
  <li class="doc-results__item">
    <a class="doc-results__title" href="/p/kb/d/payments">Payments platform</a>
    <p class="doc-results__meta"><span>Documentation</span><span>Updated 12 Sep 2026</span></p>
    <p class="doc-results__description">What the payments platform is, and how to call it.</p>
  </li>
</ul>
```

Each `doc-results__meta` child is separated by a dot the stylesheet draws, so put each fact in its
own `<span>` rather than punctuating them yourself. Use `doc-results__snippet` in place of the
description when the words that matched are marked with `<mark>`. Inside a card or a panel, add
`doc-results--plain`, so the only edges are the card's.

For a listing too long to scroll, offer an A to Z over it and a group heading for each letter:

```html
<ul class="doc-az">
  <li><a class="doc-az__letter" href="#a">A</a></li>
  <li><span class="doc-az__letter doc-az__letter--empty" aria-label="Nothing begins with B">B</span></li>
</ul>
<h2 class="doc-results__group" id="a">A</h2>
```

Every letter is shown, and one with nothing under it is plainly not a link, so the shape of the
whole list can be read at a glance.

### 11.17 Content pages and page contents

A page written to be read rather than worked in — guidance, a policy, your plugin's own
explanation of itself — is laid out as NHS England Digital lays its content pages out: a heading
block, one measured column, and plainly separated sections.

```html
<div class="doc-content-page">
  <h2>Syncing from GitHub</h2>
  <div class="doc-content-page__header">
    <p class="doc-content-page__meta">Last changed 12 September 2026</p>
  </div>
  <nav class="doc-page-contents" aria-label="Page contents">…</nav>
  <h2 id="sync">What it syncs</h2>
  …
</div>
```

The breadcrumb above it, the header band it is headed by (#11.19) and the footer below are the
platform's, as they always are.

**Page contents** is that page's own sections, at the top of its reading column. (The side panel of
#11.12's prose pages is a different thing: that is for a document among other documents.)

```html
<nav class="doc-page-contents" aria-label="Page contents">
  <p class="doc-page-contents__heading">Page contents</p>
  <ul class="doc-page-contents__list">
    <li class="doc-page-contents__item"><a class="doc-page-contents__link" href="#sync">Sync</a></li>
    <li class="doc-page-contents__item"><a class="doc-page-contents__link" href="#history">History</a></li>
  </ul>
</nav>
```

Give every section it names an `id`, and put it above the first of them. A page with two or three
short sections does not need one.

**A list somebody puts in order** is a `doc-table doc-arrange` whose `<tbody>` is marked
`data-reorder`: each row carries a `doc-arrange__handle` button (`data-reorder-handle`, named by
`data-reorder-name`) and a readonly `doc-arrange__order` field (`data-reorder-order`). The platform
drags the rows and moves them with the up and down arrow keys, renumbering the order fields as it
goes, so **the form submits exactly as it would with no script at all** — a browser without one
still chooses, it just cannot drag. It is used by the landing page, the navbar layout and the
Catalogue's pinned insights; a plugin page may use it, since the script is on every signed-in page.

```html
<tbody class="doc-table__body" data-reorder>
  <tr class="doc-table__row">
    <td class="doc-table__cell doc-arrange__grip">
      <button class="doc-arrange__handle" type="button" data-reorder-handle data-reorder-name="Success rate"
              aria-label="Move Success rate: drag it, or use the up and down arrow keys">…</button>
    </td>
    <td class="doc-table__cell">
      <input class="doc-input doc-arrange__order" data-reorder-order name="order.0" type="text"
             inputmode="numeric" value="1" readonly aria-label="Where Success rate is, counted from the top" />
      <input type="hidden" name="insight.0" value="cicd:success-rate" />
    </td>
    <th class="doc-table__header" scope="row">Success rate</th>
    <td class="doc-table__cell"><input type="checkbox" name="pin.0" checked aria-label="Pin Success rate" /></td>
  </tr>
</tbody>
```

The server reads `order.N` and sorts by it; a row left unticked is simply left out.

### 11.18 Where it came from

A page that shows somebody else's data says where it came from in tags, not in a sentence: the
catalogue resource it documents, the space it lives in, the plugin that holds it. The colour is the
kind's, so the same kind is the same colour wherever it is shown, and each tag says its kind in
words as well — colour is never the only thing carrying the meaning.

```html
<ul class="doc-kinds">
  <li class="doc-kind doc-kind--repository">
    <span class="doc-kind__label">Repository</span>
    <a href="/p/resources/r/repository/acme/payments-docs">acme/payments-docs</a>
  </li>
  <li class="doc-kind doc-kind--space">
    <span class="doc-kind__label">docs</span>
    <a href="/p/kb/docs/acme-payments-docs/README">acme/payments-docs</a>
  </li>
</ul>
```

The Knowledge Base names a space's pages `docs` and the space's name, leading to the space's README
where it has one and to the space otherwise.

The modifier is the kind as a URL names it: `--service`, `--repository`, `--team`,
`--organisation`, `--documentation`, `--documentation-source`, `--cloud-resource`, `--role`,
`--permission`, `--attribute`, `--user`, `--service-account`, and `--space` and `--plugin` for
where something is kept rather than what it is. A kind with no colour of its own is grey, which is
a reasonable thing to be.

### 11.19 Page headers

Every page in DOC opens with the same band: a stripe across the window holding what the page is
called, a sentence on what it is for, and anything the whole page offers. This is how NHS England
Digital opens its pages, and it is what makes a set of pages read as one platform rather than as a
heading dropped on top of each screen.

**A plugin writes nothing to get one.** The platform takes the page's first `<h1>` or `<h2>` — the
same heading it already names the page by in the breadcrumbs — lifts it out of the fragment and
draws it in the band. So a plugin page opens with its own heading, as it always has, and is headed
like the rest of the platform:

```html
<h2>Sources</h2>
<p class="doc-hint">Where each space's pages come from.</p>
```

Where that heading stood in a `doc-heading-row`, what the row offered beside it — its
`doc-heading-row__actions` — goes up with it, to the right of the band where every other page's
actions are. Everything else stays where it is. A page with nothing to head it falls back to the
plugin's own navigation label.

**A page that swaps itself** — an HTMX response replacing the element the page was drawn into —
has its heading row taken out too, and nothing put back: the band above the swapped content was
made from that row when the page first loaded and is still there, so a fragment carrying the row
again would draw the heading and its buttons twice. Only a heading that stood in a
`doc-heading-row` is taken, that row being what says "this names the page", so a fragment of
something smaller — a panel, a list, a form — is passed through untouched.

The band itself is `doc-page-header`, and a core page fills it through the layout rather than by
writing the markup: `page_caption` for where the page sits, `page_lede` for a summary with markup
in it, `page_actions` for what the page as a whole does. A page that opens with something of its
own instead — the landing page's hero — is built with `Chrome::bare()`.

```html
<div class="doc-page-header">
  <div class="nhsuk-width-container">
    <div class="doc-page-header__inner">
      <div class="doc-page-header__text">
        <span class="doc-page-header__caption"><a href="/service-accounts">Service accounts</a></span>
        <h1 class="doc-page-header__title">payments-ci</h1>
        <p class="doc-page-header__lede">Builds and releases the payments services.</p>
      </div>
      <div class="doc-page-header__actions">
        <a class="doc-button doc-button--secondary" href="/service-accounts/…/edit">Change</a>
      </div>
    </div>
  </div>
</div>
```

Pages are laid out on the 1200px container NHS England Digital uses, not the 960px of the service
pattern, which is what gives a listing room for a description beside its own side navigation.

### 11.20 Trees of files

Files and the directories holding them: what a template will write, what a repository holds, what
an archive is made of. A directory folds; a file opens to whatever is worth reading inside it,
usually a `doc-code`. Both are `details`, so all of it works with no script at all.

```html
<div class="doc-tree">
  <ul class="doc-tree__list">
    <li class="doc-tree__item">
      <details class="doc-tree__directory" open>
        <summary class="doc-tree__row">
          <span class="doc-tree__name">src/platform</span>
          <span class="doc-tree__meta">4 files</span>
        </summary>
        <ul class="doc-tree__list">
          <li class="doc-tree__item">
            <details class="doc-tree__file">
              <summary class="doc-tree__row">
                <span class="doc-tree__name">flags.rs</span>
                <span class="doc-tree__meta">4.1 kB</span>
              </summary>
              <div class="doc-tree__body">
                <pre class="doc-code" data-doc-filename="src/platform/flags.rs"><code>…</code></pre>
              </div>
            </details>
          </li>
          <li class="doc-tree__item">
            <p class="doc-tree__row">
              <span class="doc-tree__name">logo.png</span>
              <span class="doc-tree__meta">12.4 kB</span>
            </p>
          </li>
        </ul>
      </details>
    </li>
  </ul>
  <p class="doc-tree__footer">13 files · 42.1 kB</p>
</div>
```

| Class | Is |
|---|---|
| `doc-tree` | The whole thing, with its border |
| `doc-tree__list` | A `ul` of items. A list inside a list is indented from the one above it, with a rail saying what it belongs to |
| `doc-tree__item` | One `li`, holding a `details` or a row on its own |
| `doc-tree__directory` | A `details` for a directory. Its name takes the separator and the weight that say it is one |
| `doc-tree__file` | A `details` for a file with something to open |
| `doc-tree__row` | The line itself: a `summary` inside a `details`, or a `p` for a file that opens nothing |
| `doc-tree__name` | What it is called — the name on its own, since the directory above it is on the page |
| `doc-tree__meta` | How big, how many, what changed: at the end of the row |
| `doc-tree__body` | What a file opens to |
| `doc-tree__footer` | How many there are in all |

Nesting is as deep as you draw it, but a path read whole — `deploy/kubernetes` on one row — is
often plainer than three folds to reach it. A row that opens nothing keeps the caret's space, so
every name on a level starts in the same column.

### 11.21 Builders

```html
<form hx-post="/p/automation/new">
  <div class="doc-builder" data-doc-builder>
    <div class="doc-builder__palette">
      <h4 class="doc-builder__palette-heading">Ready-made steps</h4>
      <h5 class="doc-builder__group">Then</h5>
      <ul class="doc-builder__items">
        <li class="doc-builder__item" data-doc-builder-item="then" draggable="true">
          <span class="doc-builder__item-label">Post to Slack</span>
          <button class="doc-button doc-button--secondary doc-button--small" type="button" data-doc-builder-add
                  hx-get="/p/automation/new/step?zone=then&amp;action=slack" hx-target="#builder-then" hx-swap="beforeend">Add</button>
        </li>
      </ul>
    </div>
    <div class="doc-builder__chain">
      <section class="doc-builder__column">
        <h4 class="doc-builder__heading">Then</h4>
        <ul class="doc-builder__zone" id="builder-then" data-doc-builder-zone="then" data-doc-builder-empty="Drop an action here">
          <li class="doc-builder__step" data-doc-builder-step="s1a2b3c4d-">
            <input type="hidden" name="then" value="s1a2b3c4d-" />
            <span class="doc-builder__handle" data-doc-builder-handle tabindex="0" role="button" aria-label="Move Post to Slack: up and down arrow keys"></span>
            <a class="doc-builder__node" href="#step-s1a2b3c4d-" data-doc-builder-select="s1a2b3c4d-">Post to Slack</a>
          </li>
        </ul>
      </section>
    </div>
  </div>
  <div id="builder-forms">
    <section class="doc-builder__form" id="step-s1a2b3c4d-" data-doc-builder-form="s1a2b3c4d-">
      <h3>Then: Post to Slack</h3>
      <!-- the step's own fields, every name starting s1a2b3c4d- -->
      <button class="doc-button doc-button--secondary doc-button--small" type="button" data-doc-builder-remove="s1a2b3c4d-">Remove this step</button>
    </section>
  </div>
  <p class="nhsuk-u-visually-hidden" aria-live="polite" data-doc-builder-live></p>
</form>
```

A builder puts something together from ready-made parts: a palette of them, and columns they go
in, drawn like a graph's (#11.11) so what is being built looks like what it will become. An
automation's new page is one.

- **A drop only presses Add.** Each palette item has its own button, which fetches the step with
  HTMX; dropping the item on a column whose `data-doc-builder-zone` matches its
  `data-doc-builder-item` presses it, and a column that does not match refuses the drop. Anything
  dragging can do, the keyboard can do.
- **A step is a node and a form.** Answer `Add` with the node, and the form out of band
  (`<div hx-swap-oob="beforeend:#builder-forms">…</div>`). Put a hidden input in the node, so the
  order of the nodes is the order the steps are sent in, and start every field in its form with a
  prefix of its own, so several steps of one kind share the form.
- **Once `doc-builder.js` runs**, one form is shown at a time — the chosen node's, marked
  `--selected` — and each step moves by its handle, dragged or with the up and down arrow keys,
  within its own column. Moves, additions and removals are announced in `data-doc-builder-live`.
  A form whose node has gone is taken out, so it is not sent. Without the script, every form shows
  and each node links to its own.
- Mark a form to show first with `data-doc-builder-selected`, such as the step a save refused.

### 11.22 Charts

```html
<div class="doc-card-group doc-card-group--wide">
  <div class="doc-card">
    <h3 class="doc-card__heading">Deployments</h3>
    <div class="doc-card__content">
      <canvas class="doc-chart" role="img" aria-label="Deployments to production each week"
              data-chart="{&quot;type&quot;:&quot;bar&quot;,&quot;data&quot;:{…},&quot;options&quot;:{…}}"></canvas>
    </div>
  </div>
</div>
```

A chart is a `<canvas>` carrying a [Chart.js 4](https://www.chartjs.org/docs/4.5.1/) configuration
as JSON in `data-chart`, escaped like any other attribute value. The platform draws it: every
signed-in page has `doc-charts.js`, which fetches Chart.js the first time a page has a chart and
draws each canvas as it appears, including those HTMX swaps in later, and lets go of those swapped
away. The fragment carries no script.

- **Data only.** The configuration is JSON, so it holds no callbacks: tick and tooltip formatters
  are not available. Put units in axis titles, and round values before sending them.
- **Say what it shows** in `aria-label`, and keep every figure a chart draws in a table or list on
  the same page or one it links to: a canvas is not read out.
- **Colour with meaning:** the brand's `#5b00b8` for the measure itself, `#d5281b` for failure,
  `#768692` for something secondary such as a percentile. Straight lines (`"tension": 0`) over
  curves, which suggest values nobody measured. Turn `animation` off.
- Put charts two to a row in `doc-card-group--wide`, each in its own card with a heading.

### 11.23 Timelines of bars

```html
<svg class="doc-gantt" viewBox="0 0 960 90" width="100%" role="img" aria-label="Each release coming up">
  <line class="doc-gantt__tick" x1="400" y1="24" x2="400" y2="90"/>
  <text class="doc-gantt__tick-label" x="403" y="14">Oct 2026</text>
  <a href="/p/roadmap/release?id=jira:102"><text class="doc-gantt__label" x="4" y="47.5">PAY 2.4</text></a>
  <rect class="doc-gantt__bar doc-gantt__bar--degraded" x="300" y="36" width="220" height="14" rx="3"><title>At risk</title></rect>
  <polygon class="doc-gantt__mark doc-gantt__mark--degraded" points="520,36 527,43 520,50 513,43"><title>Due 7 Oct 2026</title></polygon>
  <line class="doc-gantt__today" x1="430" y1="18" x2="430" y2="90"/>
</svg>
```

Where a chart draws numbers, a timeline draws stretches of days: a lane a row, labelled on the left
and linked where it has a page of its own, with bars from one day to another, marks on a day and a
dashed line at today. A plugin draws it as inline SVG on the server, placing every shape by its
attributes, since a page carries no style; `doc-gantt` classes colour it. `doc-gantt__stripe` shades
every other lane.

- **Tones mean what badges do:** `--ready`, `--degraded` and `--error` as the badges of those names,
  `--security` blue for something going on but winding down, `--unknown` grey, `--done` dark slate,
  and `--muted` pale grey for what has passed.
- **Say what it shows** in `aria-label`, give every bar and mark a `<title>`, and keep what it
  shows in a table beside it, as with a chart.
- `eol` draws each release's support on one, and `roadmap` each release from start to due date.

### 11.24 Maps

```html
<figure class="doc-geomap" aria-label="Requests by city over the last hour"
        data-geomap="{&quot;points&quot;:[{&quot;lat&quot;:51.51,&quot;lon&quot;:-0.13,&quot;label&quot;:&quot;London&quot;,&quot;value&quot;:1200,&quot;shown&quot;:&quot;1200 req/s&quot;},{&quot;country&quot;:&quot;IN&quot;,&quot;value&quot;:410}],&quot;fit&quot;:&quot;points&quot;}"></figure>
```

A map is a `<figure class="doc-geomap">` carrying what to show as JSON in `data-geomap`, escaped like
any other attribute value. The platform draws it: every signed-in page has `doc-geomap.js`, which
fetches the world the first time a page has a map and draws each map as it appears, including those
HTMX swaps in later. The world is Natural Earth's 1:110m countries (public domain), kept by DOC and
drawn in the Equal Earth projection, so the whole world shows at once with every area true. Nothing
is fetched from anywhere but DOC: there are no map tiles, and the fragment carries no script.

| Key | Holds |
|---|---|
| `points` | Places, each at `lat` and `lon`, or at its `country`'s label point. Each may have a `label`, a `value` that sizes it by area against the largest, `shown` — the value in words with its unit, for the tooltip — a `tone` and an `href` |
| `regions` | Countries to colour, each by its `country` and either a `tone` or a `value`, which shades it from pale to the brand's purple against the largest. Each may have a `label`, `shown` and an `href` |
| `fit` | `world`, the default, or `points`, which starts framed on the places and countries given |

- **A country** is its ISO 3166 code, two letters or three (`GB`, `GBR`), or its name as Natural
  Earth writes it (`United Kingdom`). A place given only by its country is called by the
  country's name.
- **Tones mean what badges do**: `ready`, `degraded`, `error`, `unknown` and `security`.
- **Moving about.** Dragging pans; the buttons under the map zoom in, out and back to the start;
  Ctrl with the wheel, or a pinch, zooms where the pointer is, and the wheel alone still scrolls the
  page. With the map focused, the arrow keys pan, `+` and `-` zoom and `0` goes back. Hovering or
  focusing a place says its label and what it shows.
- **What cannot be placed is said**, under the map, by name — a country DOC does not know, a
  latitude past 90.
- **Say what it shows** in `aria-label`, and keep every place in a table on the same page or one it
  links to, as with a chart.
- The world is 150 KB, fetched once and cached. `crates/core/frontend/assets/geo/build.py` writes it
  from a pinned Natural Earth release whose hash it checks.

## 12. Writing a plugin in Rust

Depend on `doc-plugin-sdk`, implement `Plugin` and hand it to `main!`. The SDK handles everything in
#5–#7, including the secret check, deadlines, worker limit, panics, liveness, re-registration and
exit. It also fills in `version` from `Cargo.toml` and hashes the binary.

```rust
use async_trait::async_trait;
use doc_plugin_sdk::{
    Backend, Classification, CustomPermission, Manifest, Nav, Plugin, PluginError, Request,
    Response, RunInput, RunOutput,
};
use serde_json::{Value, json};

#[derive(Default)]
struct Hello;

#[async_trait]
impl Plugin for Hello {
    async fn load(&mut self, _backend: &Backend, _previous: Option<Value>) -> Result<(), PluginError> {
        Ok(())
    }
    async fn unload(&mut self, _backend: &Backend) -> Result<Option<Value>, PluginError> {
        Ok(None)
    }
    async fn run(&self, backend: &Backend, input: RunInput) -> Result<RunOutput, PluginError> {
        let name = input.payload["name"].as_str().unwrap_or("world");
        backend.insert("greetings", json!({ "name": name })).await?;
        Ok(RunOutput { payload: json!({ "greeting": format!("hello, {name}") }) })
    }
    async fn cancel(&self, _backend: &Backend) -> Result<(), PluginError> {
        Ok(())
    }
    async fn handle(&self, backend: &Backend, request: Request) -> Response {
        match (request.method.as_str(), request.path.as_str()) {
            ("POST", "api/greetings") if !backend.allows("greetings", true) => {
                Response::problem(403, "forbidden", "needs plugin:hello:pluginuser:greetings:rw")
            }
            ("GET", "ui" | "ui/") => Response::html("<div class=\"doc-card\">…</div>"),
            _ => Response::not_found(),
        }
    }
}

doc_plugin_sdk::main!(Hello, Manifest {
    id: "hello".into(),
    classification: Classification::Synchronous,
    custom_permissions: vec![CustomPermission::user("greetings")],
    nav: vec![Nav::new("Hello", "/")],
    ..Manifest::default()
});
```

`Backend` has a method for every call in #9:
- data: `get`, `query`, `aggregate`, `insert`, `update`, `upsert`, `delete` and `batch`, plus
  `query_all` (every page, for small collections), `change` (read, change and write back, again
  whenever another writer got there first) and `delete_where`
- events and the Service Bus: `publish`, `publish_once`, `request` and `send`
- cache: `cache_get`, `cache_set`, `cache_delete` and `cache_compare_and_set`
- `task`, `state_get`, `state_set`, `state_delete`, `audit` and `set_state`
- identity providers: `identity`, `link` and `provide_user`
- team providers: `provide_user`, `provide_team`, `provide_members` and `remove_team`

A refused call is a `PluginError`: `problem()` gives its status and kind, and `is_duplicate()` and
`is_version_conflict()` answer the two a plugin usually handles. It also has `caller`, `allows`,
`require` and `attribute` for permissions. The full example, with a feature for each classification,
is [`examples/hello`](crates/core/plugin-sdk/examples/hello/src/main.rs). Build it with
`just example-plugin <variant>`.

## 13. Writing a plugin in Go

There is no Go SDK module. A Go plugin implements #5–#7 itself, which takes a few hundred lines
with [quic-go](https://github.com/quic-go/quic-go) (tested with 0.59.1, which needs Go 1.24). There
are two starting points:

- **The New DOC plugin template** (`templates`, [software-templates.md](docs/software-templates.md#doc-plugins))
  creates a repository whose `internal/doc` package does it: registration, liveness, the
  `/host/v1` gate and every call in #7, and a `Backend` for #9's data, events, settings, discovery and
  `api/` calls and notifications. The plugin is a manifest, an `http.ServeMux` that forwarded
  requests arrive at as `/ui/…`, `/api/…` and `/discovery/…`, and whichever of `Load`, `Run`, `Cancel`,
  `OnEvent` and `CheckSettings` it needs. Its guided path teaches it with a tutorial. The package
  lives in [`scaffolds/go/plugin-base`](crates/plugins/software-templates/scaffolds/go/plugin-base).
- **The reference**, [`crates/core/plugin-sdk/examples/go-hello/main.go`](crates/core/plugin-sdk/examples/go-hello/main.go),
  is one self-contained file, commented against the sections of this document. Copy it and replace
  the `hello` type with your plugin.

The reference builds, passes `go vet`, and has been run against a live backend: it registers with
its data declaration, the backend creates its `greetings` collection with its index and full-text
search, and each run stores a greeting through the data API.

### 13.1 Transport setup

```go
quicConfig := &quic.Config{MaxIdleTimeout: 3 * time.Second, KeepAlivePeriod: time.Second}

// Serving /host/v1/*: present plugin-<id>'s certificate.
cert, _ := tls.LoadX509KeyPair(secrets+"/certs/plugin-"+id+".pem", secrets+"/certs/plugin-"+id+".key")
server := &http3.Server{
	Addr:       bind, // DOC_PLUGIN_BIND
	TLSConfig:  http3.ConfigureTLSConfig(&tls.Config{Certificates: []tls.Certificate{cert}, MinVersion: tls.VersionTLS13}),
	QUICConfig: quicConfig,
	Handler:    runtime, // an http.Handler implementing #7
}
go server.ListenAndServe()

// Dialling /plugin/v1/*: trust only the bootstrap CA, and expect the name "backend".
roots := x509.NewCertPool()
roots.AppendCertsFromPEM(caPEM) // secrets + "/ca/ca.pem"
client := &http.Client{Transport: &http3.Transport{
	TLSClientConfig: &tls.Config{RootCAs: roots, ServerName: "backend", MinVersion: tls.VersionTLS13},
	QUICConfig:      quicConfig,
}}
// POST "https://" + DOC_BACKEND_QUIC + "/plugin/v1/register", with authorization: Bearer <token>
```

### 13.2 The `/host/v1` gate

```go
func (rt *runtime) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	path := r.URL.EscapedPath()
	if !strings.HasPrefix(path, "/host/v1/") {
		w.WriteHeader(http.StatusNotFound)
		return
	}
	secret := rt.secret.Load() // stored from the registration response
	presented, _ := strings.CutPrefix(r.Header.Get("authorization"), "Bearer ")
	if secret == nil || subtle.ConstantTimeCompare([]byte(presented), []byte(*secret)) != 1 {
		w.WriteHeader(http.StatusServiceUnavailable) // empty body: "not ready", the backend retries
		return
	}
	// …then the worker limit, x-doc-deadline-ms as a context timeout, recover() for panics,
	// and a switch on path: health, load, unload, run, cancel, event, exit, request/…
}
```

### 13.3 Go-specific notes

- **Deadlines:** wrap `r.Context()` in `context.WithTimeout` from `x-doc-deadline-ms`, and pass that
  context to every backend call made while handling the request.
- **Panics:** `recover()` in the handler, answer `500` with a problem, and set the state to `error`
  if the path was `load` or `unload`. quic-go runs each request in its own goroutine, so one panic
  doesn't affect the others.
- **Context tokens:** copy `x-doc-context` into a per-request backend client (`backend.For(token,
  caller)` in the reference), not into shared state. It stops working when the call returns.
- **`load`/`unload` and `run`:** hold a `sync.RWMutex` shared by each `run` and exclusively by `load`
  and `unload`, so reloading waits for runs in progress. Call your cancel logic before taking it in
  `unload`.
- **JSON:** use `json.RawMessage` for `previous`, `payload` and anything opaque, so you pass it on
  unchanged. Send `null` rather than omitting fields the backend reads, such as liveness `error`.
- **Exit:** answer `204`, then `server.Shutdown(ctx)` with a short timeout, and return from `main`
  with code 0.
- **The binary hash:** hash `os.Executable()`. Set the version at build time, for example
  `go build -ldflags "-X main.version=1.2.0"`, and never deploy two different builds with the same
  version.
- **Containers:** a static build (`CGO_ENABLED=0`) runs in `gcr.io/distroless/static` or `scratch`.
  Mount the secrets volume read-only at `/secrets`, join the `doc` network, set the restart policy
  to `on-failure`, and don't publish the UDP port.

## 14. Conformance checklist

Run these checks against a development backend (`just dev`) before shipping a plugin in any
language. The Rust SDK passes them all. A Go plugin must too.

**Registration and lifecycle**

- [ ] Started before the backend, the plugin keeps retrying and registers within one backoff
      interval of the backend coming up.
- [ ] `/host/v1/health` with no secret, and with a wrong secret, answers `503` with an empty body.
      A path outside `/host/v1` answers `404`.
- [ ] After registering, the plugin reaches `running`, and its collections answer `query`.
- [ ] A new version with an incompatible data change (a changed type, or a field removed without
      being deprecated first) is refused at registration with `400` `incompatible-data`, and the old
      version keeps serving.
- [ ] Liveness reports arrive every `liveness_ms`. Restart the backend and the plugin registers
      again after its next report.
- [ ] Registering a new version while the old one serves hands over. The old one's `unload` state
      arrives in the new one's `load`, the old process exits 0, and no requests fail apart from
      documented `503`s.
- [ ] `kill -9` on the plugin: the backend shows it in `error` within 30 s, and the backend itself
      is unaffected.

**Calls**

- [ ] A `run` with a short `x-doc-deadline-ms` that the plugin overruns is cut off with `504`.
- [ ] A panic in a route fails that request with `500`, and the next request succeeds.
- [ ] A panic in `load` puts the plugin in `error`, with the message as the reason.
- [ ] An event is acknowledged once. Returning an error causes redelivery with the same `id`, and the
      plugin processes it once.
- [ ] A request from a caller without the custom permission gets `403` from the plugin. The same
      request from an admin succeeds.

**Settings** (#4.4)

- [ ] A required setting nobody has set leaves the plugin running, with `missing` naming it, and the
      routes that need it say so rather than failing.
- [ ] Saving a setting the plugin declared `number`, `url`, `choice` or `cron` with something else
      is refused against that field, and nothing is stored.
- [ ] A secret is never in an answer, a page, an export or a log; the page says only whether it is
      set. Setting one and reading `POST /plugin/v1/settings` back gives the plugin the value.
- [ ] `DOC_<PLUGIN>_<KEY>` fills a field nobody has set, and the page says where it came from.
      Saving something there wins over it, and clearing what was saved falls back to it again.
- [ ] After a save, the plugin sees the new values, without a restart.
- [ ] Turning a feature off stops the schedules that name it, and turning it on starts them again.

**Isolation** (each must be refused)

- [ ] Writing to another plugin's collection, or to a `core.*` collection: `403`.
- [ ] Reading another plugin's collection that isn't exported to this plugin: `403`.
- [ ] Reading `core.tasks` returns only this plugin's own tasks.
- [ ] Publishing to `platform.x` or `plugin.<other>.x`: `403`.
- [ ] A `services` call with no context, or with a context from a call that has finished: `403`.
- [ ] `identity`, `identity/link` or `users` without the capability, or for another provider's
  accounts: `403`.
- [ ] `organisations/write` or `teams/write` without `team-writer`: `403`. With it, but acting for
      somebody who does not administer identity: `403`.
- [ ] `teams`, `teams/members` or `teams/remove` without `team-provider`: `403`. Another
  provider's team, or a team made in DOC: `404`.
- [ ] A caller holding `plugin:<id>:user:rw` but not `plugin:<id>:settings` is refused the Settings
      and Features tabs: `403`.
- [ ] Asking another plugin's `api/` as itself, without `service-account`: `403`. With a
      `read-only` guard, any route that writes: `403`, never reaching that plugin.
- [ ] A plugin that keeps things per environment, asked to change one of the `production` ones with
      a `not-production` guard, or every environment at once: `403`.

**UI**

- [ ] Every `ui/` route returns a fragment with no `<script>`, `<style>` or `style=""`.
- [ ] Every value from users or logs is escaped. Try a name of `<b>x</b>`.
- [ ] Lists have an empty state. Logs show the tail with a link to the rest.

## 15. Rules and limits

**Rules**

1. **Keep `load` and `unload` short.** Routing is paused while they run, and requests waiting longer
   than 5 s get `503`.
2. **Hand over only small state through `unload`,** under 1 MiB. Keep everything else in your collections
   or in `state`.
3. **Persist before acknowledging.** Anything held only in memory is lost when the process dies.
4. **Keep requests short.** Long work belongs in `run` tasks, because in-flight requests hold up
   handovers.
5. **Make `run` and event handlers idempotent.** Tasks are retried and requeued during handovers, and
   events arrive at least once.
6. **Honour `x-doc-deadline-ms`.**
7. **Don't send bulk data through plugin calls.** The transport carries about 100 MB/s. Link large
   downloads directly.
8. **Batch writes.** Each backend call costs about 0.2 ms before any work is done, so use `batch`
   and page through large reads instead of looping over single records.
9. **Change data declarations in compatible steps.** Add fields and collections freely. Deprecate
   before removing, and never change a field's type (#4.3).
10. **Don't rely on a fixed address.** Advertise your own, and let the certificate prove who you are.
11. **Never log** the registration token, instance secret or context tokens. The Rust SDK holds
    them, and every credential a plugin reads, in `Secret<T>`, which cannot be printed or
    serialised without saying so (`expose`, or `serialize_with = "doc_secret::exposed"`).

**Limits**

| Limit | Value |
|---|---|
| Plugin ID | 32 characters, `^[a-z][a-z0-9-]{0,31}$` |
| Forwarded request or response body | 16 MiB |
| Request deadline (routes, synchronous `run`) | 30 s |
| Background `run`, per attempt: async (and queued tasks) / one-shot | 30 s / 10 min, both maximums |
| `load` / `unload`, `cancel`, `event`, `health`, `exit` deadlines | 60 s / 30 s / 30 s / 5 s / 5 s |
| Concurrent calls handled by the SDK | 32 (then `429`) |
| Registrations per plugin | 10 a minute (then `429` with `Retry-After`) |
| Calls on a plugin's public routes, per client | 120 a minute (then `429` with `Retry-After`) |
| Liveness report / grace | Every 5 s / `error` 20–30 s after the last report |
| Handover: requests wait / old version drains | 5 s / 10 s |
| Data: collections per plugin / fields per collection / indexes per collection | 64 / 64 / 16 |
| Data: record size / records per `query` page / writes per `batch` / time per request | 1 MiB / 1,000 / 100 / 5 s |
| Data: `aggregate` groups / `group_by` fields | 1,000 / 3 |
| Service Bus request deadline | 10 s default, 60 s maximum |
| Cache key / default TTL / entries | 256 bytes / 24 h / 10,000 |
| State key / value | 256 bytes / 1 MiB |
| Audit action | 64 characters |
| Task attempts | 1–10, default 3 |
| Event redelivery | 0.5 s, doubling to 60 s |

Deadlines and timeouts are the defaults in `config/doc.toml`, and an operator can change them. The
two background-run limits are the exception: an operator can only lower those.
