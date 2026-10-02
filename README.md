<p align="center" style="width:100%"><a href="https://github.com/colwill/rundoc" target="_blank"><img src="logo.png" alt="RUNDOC Logo"></a></p>


# DOC

DOC (RUNDOC) is a Development, Organisation and Coordination platform that brings an organisation's resources, access control,
knowledge, collaboration, processes, automation, software templates, feature flags and self-service
infrastructure together in one place. A small core authenticates, authorises, stores and routes; plugins provide the features.

The plugin protocol is set out in full in [DPS.md](DPS.md).

## What you need

- **Linux** with **Docker Engine and Compose v2**, and [**just**](https://just.systems). This is
  enough to run DOC.
- To build and run DOC on the host (`just dev`, `just check`, the `cli`):
  - [**rustup**](https://rustup.rs); the toolchain is pinned in `rust-toolchain.toml` and installs
    itself on first use,
  - the tools quiche's bundled BoringSSL builds with:
    `sudo apt install cmake make g++ clang libclang-dev perl`,
  - `cargo install cargo-watch` for `just dev`, and `cargo install --locked cargo-deny` for
    `just check`.
- A **GitHub organisation** to sign in with. Local sign-in works for development without one.

## Configure

1. **Copy the environment file** and change every password in it:

   ```sh
   cp .env.example .env
   ```

2. **Create a GitHub OAuth app** for sign-in. In GitHub, go to your organisation's (or your own)
   **Settings → Developer settings → OAuth Apps → New OAuth App**, and set:

   | Field | Value |
   |---|---|
   | Homepage URL | `http://127.0.0.1:8081` (DOC's frontend, `DOC_PUBLIC_URL`) |
   | Authorization callback URL | `http://127.0.0.1:8081/auth/github/callback` |

   Generate a client secret, then put the app's details in `.env`:

   ```sh
   DOC_GITHUB_CLIENT_ID=Iv1.0123456789abcdef
   DOC_GITHUB_CLIENT_SECRET=...
   DOC_GITHUB_ORGS=acme            # only members of these organisations can sign in
   DOC_GITHUB_TOKEN=...            # reads the organisations' teams and repositories for the sync
   ```

   The sync brings each organisation's teams into DOC as teams of its own, with their members and
   the people who have never signed in, and its repositories into the Catalogue. Its teams join the
   organisation that signs in with GitHub, or the one `DOC_GITHUB_DOC_ORGANISATION` names. The
   sync's token needs to read the organisation's members, teams and repositories (a classic
   token with `read:org` and `repo`, or a fine-grained one with read access to members and
   repository metadata). A GitHub App works instead (`DOC_GITHUB_APP_ID` and
   `DOC_GITHUB_APP_KEY_FILE`). If the organisation restricts third-party access, an owner must
   approve the OAuth app. GitHub Enterprise is the `ghe` plugin, configured the same way with
   `DOC_GHE_*` and started with `--profile ghe`.

   **Any OpenID Connect provider** — Entra ID, Google, Keycloak, Okta — is the `oidc`
   plugin instead. Give it an issuer, a client and a secret; it discovers the rest and reports
   what the ID token claims about each person:

   ```sh
   DOC_OIDC_ISSUER=https://login.example.com/realms/staff
   DOC_OIDC_CLIENT_ID=doc-platform
   DOC_OIDC_CLIENT_SECRET=...
   DOC_OIDC_TITLE="your work account"   # what the sign-in page calls it
   ```

   Register `${DOC_PUBLIC_URL}/auth/oidc/callback` as a redirect URI with the provider. To develop
   against one without touching a real directory, `just oidc-dev` runs a Keycloak with two people
   in it, and `just oidc-dev-down` stops it again.

   **Several providers at once.** Each one is the same plugin under its own ID, with settings of
   its own, its own callback (`${DOC_PUBLIC_URL}/auth/<id>/callback`) and its own button on the
   sign-in page. `google`, `entra`are ready to run:

   ```sh
   DOC_GOOGLE_CLIENT_ID=...             # DOC_GOOGLE_ISSUER is https://accounts.google.com
   DOC_ENTRA_ISSUER=https://login.microsoftonline.com/<tenant>/v2.0
   DOC_ENTRA_CLIENT_ID=...
   ```

   With Docker, start them by profile: `docker compose --profile google --profile entra up`. With
   `just dev`, name them in `DOC_OIDC_INSTANCES=google,entra`. For a provider of your own, pick any
   ID: run the plugin with `DOC_OIDC_PLUGIN_ID=okta` and settings under `DOC_OKTA_*`, and add
   `okta` to `ids` and to the capabilities in `config/doc.toml` — an ID that is not there cannot
   register. Each organisation then picks, on its own page, which of them its people sign in with,
   and somebody's account with one provider is separate from their account with another.

   Every part is optional. Without an OAuth app GitHub sign-in is simply off, and without
   organisations and a token there is no sync. The plugin still runs either way, and the Knowledge
   Base can import any **public** repository with no GitHub settings at all, within GitHub's limit of
   60 unauthenticated requests an hour. A private repository needs its organisation in
   `DOC_GITHUB_ORGS` and a token or App that can read it.

3. **Name the first admins.** In `config/doc.toml`, list the logins that are platform admins before
   anyone has been granted anything. The first is also the username of the first sign-in (below):

   ```toml
   [bootstrap]
   admins = ["your-login"]
   ```

   `config/doc.local.toml`, which git ignores, is laid over `config/doc.toml`, so a login of your
   own can go there instead.

## Start and stop

```sh
just up      # bootstrap, then storage, fabric, core, workers and plugins, in that order
just down    # stop everything; the volumes are kept
```

`just up` first runs the bootstrap job, which fills the secrets volume with a certificate
authority, a certificate for every service, bus node and plugin, the tokens they use, and the
**settings key** that every plugin secret stored in DOC is encrypted under — back that volume up,
because a secret whose key is lost has to be set again. (`doc-backend rotate-settings-key` writes
a new key and re-encrypts everything under it.) Then it
builds and starts Postgres, the nine bus nodes and telemetry, the backend and frontend, the workers
and every plugin. The first run compiles everything in Docker and takes a while.

| Where | What |
|---|---|
| <http://127.0.0.1:8081> | DOC |
| <http://127.0.0.1:8080> | The API (`/api/v1`), used by `cli` and MCP clients |
| <http://127.0.0.1:3900> | Grafana: traces, metrics, logs, and the DOC dashboards |

## The first sign-in

The bootstrap job writes the first sign-in to the secrets volume, as `first-sign-in`: a username
(the first bootstrap admin) and a one-time password for a **DOC account**, which DOC keeps itself
so that it can be tried without a company directory. Read it with
`docker compose -f crates/core/docker-compose.yaml run --rm --entrypoint cat backend /secrets/first-sign-in`,
or `cat secrets/first-sign-in` under `just dev`.

1. Open <http://127.0.0.1:8081> and sign in with that username and one-time password, then choose
   your own password. DOC then opens **Set up DOC**, a step at a time: its name, the tools your
   teams use, the plugins suggested for them, your organisation, how people sign in, connecting
   your tools and bringing your data in. Any step can be skipped; it stays under **Admin** ›
   **Set up DOC**, and the home page reminds administrators until it is finished. As a bootstrap
   admin you see everything, including the **Status** dashboard: every component, including each
   bus node, and every plugin, updating live.
   **DOC accounts** adds other people, each with a one-time password to hand them. Once GitHub
   sign-in is set up, choose it for your organisation on **Teams** › **Organisations**. If a plugin you can use needs an account elsewhere, such
   as GitHub, DOC first offers to link it. You can skip that and link it later from your account
   page, which your name in the header opens. On **Users**, you can add people before they sign in
   and give them access ahead of time. **Teams** holds your organisations and teams, and everyone
   starts in the two default teams, Leadership and Product.
2. Create a personal access token on **Your account** › **Your personal access tokens**
   (your name in the header); it is shown once.
3. Build the command line and log in with it:

   ```sh
   cargo build --release -p doc-cli
   target/release/cli login --url http://127.0.0.1:8080 --token doc_pat_...
   target/release/cli plugins list
   ```

   `cli` saves the URL and token in `~/.config/doc/credentials`. Every command has `--help`, and
   `--json` prints the API's own answer.

## Development

```sh
just dev            # backend, frontend and every plugin on the host, rebuilt on every edit
just dev cluster    # the same, against the bus clusters in Docker instead of in-memory buses
just check          # formatting, clippy, cargo deny and the endpoint unit tests
```

`just dev` keeps Postgres in Docker and fills `./secrets` with the same kind of CA, certificates
and tokens as the bootstrap job. The frontend reloads the page when its rebuilt process is serving.
Each plugin it starts reads `.env`, as it would under Compose, so settings such as the GitHub OAuth
app apply to a local run too; core's own values there are left alone, because they name hosts only
Docker can reach. A plugin whose crate is new needs a restart of `just dev`, which is where the list
of plugins to watch comes from.
`just plugin-deploy <plugin>` hot-reloads a plugin in Docker, and `just example-plugin <variant>`
builds the example `hello` plugin with one of its variants.

The repository is laid out as `crates/core` (backend, frontend, the plugin protocol and SDK,
transport, permissions), `crates/fabric` (the three buses and their consensus), `crates/workers`,
`crates/plugins` (every plugin and `cli`) and `crates/storage`.
## The demo walkthrough

This is the demo, step by step. It assumes the stack is up, you are signed in as the
admin, and GitHub has an `acme` organisation with a `payments-core` team whose member `alice` has not
signed in yet. Pages are under <http://127.0.0.1:8081>.

### Platform

1. `just up` starts everything; `cli plugins list` shows `rbac`, `github`, `resources`,
   `service-map`, `kb`, `automation`, `calendar`, `calendar-events`, `process`, `water`, `infra`,
   `templates`, `flags` and `hello`, all `running`, each at the version in its `Cargo.toml`.
2. The admin signs in. The **Status** dashboard shows every component `up`, including each bus
   node, and updates live.
3. Stop a bus node: `docker compose -f crates/fabric/docker-compose.yaml stop servicebus-2`. Nothing
   is interrupted; the dashboard shows the node `down`. `start servicebus-2` brings it back `up`.

### Access

4. In the RBAC pages (`/p/rbac/`), the admin creates a `payments-core` group in each of four
   plugins, giving `plugin:kb:user:rw`, `plugin:resources:user`, `plugin:water:user:rw`, and
   `plugin:infra:user:rw` with `plugin:infra:pluginuser:selfservice-aws`. Then an onboarding rule:
   GitHub team `acme/payments-core` joins those four groups, gets `plugin:rbac:user` and the
   attribute `team=payments-core`.
5. `alice` signs in for the first time. Her account is created with `plugin:github:user` and
   everything in the rule: `cli permissions list --user alice`.
6. Alice creates the service account `docs-bot` on **Service accounts** (or `cli sa create
   docs-bot`), issues it a token, and grants it `plugin:kb:service:wo`
   (`cli permissions grant --sa docs-bot plugin:kb:service:wo`). Granting it
   `plugin:resources:service:rw` is refused: she can only read `resources`.
7. The admin grants alice `plugin:hello:user:rw` and `plugin:hello:pluginuser:greetings:ro`. At
   `/p/hello/`, alice sees the greetings, but **Greet** is refused by the plugin itself. The admin
   can greet without any grant.

### Resources

8. The GitHub plugin syncs `acme` on its schedule, or now with `cli github sync`. The admin loads
   the example catalogue and assigns the service account:

   ```sh
   cli resources apply -f examples/catalog.yaml
   cli resources assign docs-bot service:card-gateway
   ```

   A single resource is added without writing YAML at all: **Catalogue → New resource** fills in the
   same document from a form, with its owner and everything it connects to chosen from the
   Catalogue as you type, and a resource's own page connects it to anything else.

   The organisations and teams in that file are the platform's: applying it makes them on **Teams**,
   in the organisation each names, and a team's Catalogue page shows what it owns and who is in it
   beside its page there. Making or changing one that way needs the same access as the Teams page
   itself.

   In **Catalogue**, alice opens the `payments` organisation and follows `Organisation-to-Users` and
   `Organisation-to-ServiceAccounts` down to people and service accounts.
9. **Service Map** shows `card-gateway` with its repositories, team, documentation and cloud
   resources, and its page's **Who can reach it** panel is the RBAC plugin's access map.

### Knowledge

10. In CI, as `docs-bot`: `cli kb import examples/mkdocs --resource service:card-gateway --wait`
    exits `0` when the import has finished. A Confluence space and a Google Drive folder are added
    as sources that sync on a schedule:

    ```sh
    cli kb sources add confluence --space platform --resource service:card-gateway \
        --flavour cloud --site https://acme.atlassian.net --space-key PAY --credential acme-cloud \
        --schedule "0 * * * *"
    cli kb sources add drive --space platform --resource service:card-gateway \
        --folder 1AbC... --credential acme-drive --schedule "0 * * * *"
    ```

    Their credentials are given to the `kb` plugin as `DOC_KB_CREDENTIAL_ACME_CLOUD`
    (`email:api-token`) and `DOC_KB_CREDENTIAL_ACME_DRIVE_FILE` (a service account's JSON key).
    All three appear in **Knowledge** under `card-gateway` and in alice's searches
    (`cli kb search "card tokenisation"`); `docs-bot` gets `403` if it searches.
11. Alice creates a personal access token and points an MCP client at
    `http://127.0.0.1:8080/api/v1/plugins/kb/api/mcp` with `Authorization: Bearer <token>`; searching
    through it finds the imported docs. Someone without `plugin:kb:user` is refused, and has no
    **Knowledge** link.

### Collaboration, process and automation

12. In **Watercooler**, alice starts a discussion; typing `/card` suggests the `card-gateway` tag,
    and the thread appears on that tag's page.
13. With `plugin:water:pluginuser:hackathon` from a group, a `payments-core` member creates a
    hackathon under **Events**. It appears in the team's calendar and its ICS feed, and teams
    register for it.
14. Alice signs a farewell card and gives the team kudos under **Cards** and **Kudos**. An
    automation made from the **Kudos to Slack** template posts the kudos to the team's Slack channel
    (`DOC_AUTOMATION_SLACK_TOKEN`).
15. The admin creates a weekly "on-call handover" process on `card-gateway` under **Processes**. Its
    occurrences appear on the service's calendar, reminders arrive through the **Process reminders**
    automation, and missed occurrences are flagged.
16. An automation triggered by `plugin.kb.document.imported` calls an external webhook, and an
    automation with a webhook trigger re-syncs the Confluence source when its URL is called.

### Self-service infrastructure

17. With vendor credentials set (`DOC_INFRA_*` in `.env`) and templates made under **Platform →
    Development**, where developer infrastructure is asked for — sandboxes, agent harnesses as VMs
    — alice asks for an AWS bucket:
    `cli infra request aws-dev-bucket --name fraud-scores --team payments-core --service card-gateway --wait`.
    It is made and appears in Resource Definitions connected to `payments-core`; a Linode request
    is refused, since her group allows AWS only. When its lifetime ends it is torn down.
    **Platform → Production** is the other half of the picture: what runs in production, to read
    rather than change. DOC does not make production infrastructure, and a request naming it is
    refused.

### Software templates and feature flags

Added after the walkthrough above, so these are not numbered with its steps.

- The admin configures the instance once on the templates plugin's Settings page: the OTLP endpoint
  every created service exports to, the environment it runs in, where DOC is, and a GitHub token to
  create repositories with.
- Under **Technology → Templates**, alice opens **New service** and **Create from this**. She answers
  the service's name, what it does, its owning team (chosen from the Catalogue) and what state it is
  in — an **Experiment**, in **Design**, or **Production ready** — then chooses a language — Go,
  Rust, Python, C++ or TypeScript — and what to build: a CLI, a REST or gRPC API, a Kubernetes
  controller or a set of CRDs. **Check your answers** renders every file as it will be committed,
  before anything exists.
- **Create it** queues a run and opens its page: the repository is created and the files committed
  in one commit, the repository and the service are applied to the Catalogue connected to each
  other, the service's first feature flags are created in DOC, and alice is told in her inbox. The
  steps tick over as a timeline and the log fills as it goes.
- What she said it was follows it: the Catalogue entry says `lifecycle: experiment`, the README
  says what that means, and `doc.lifecycle=experiment` is on every trace, metric and log it sends,
  so an experiment's telemetry is told apart from a supported service's. The Runs list shows it
  beside each run. **New tool** creates a command line tool the same way, in the same five
  languages, and gives it no Kubernetes manifests because nothing deploys it.
- Each step ran as alice: the Catalogue entries are hers, and one she could not make is refused with
  `403` and the run says which permission it wanted. The repository is the plugin's own
  (`DOC_TEMPLATES_GITHUB_TOKEN`), not her GitHub account's.
- What she got is wired to the platform already: it exports traces and metrics to the instance's
  collector, and reads its flags from DOC in one ETagged call with a fallback at every read. Under
  **Platform → Feature flags**, the admin turns `accepting-traffic` off and the service withdraws
  itself from traffic within seconds, without a deployment. Any OpenFeature SDK reads the same flags
  over OFREP, and an existing Unleash or Flagsmith is read beside DOC's own under **Providers**.

### Operations

1. Hot reload and failures:
    - Bump the version in `crates/plugins/knowledge-base/Cargo.toml` and run
      `just plugin-deploy kb`. The new process registers beside the old one, takes over its state
      and pending imports, and the old one exits; the UI keeps working, and `cli plugins show kb`
      shows the old version `unloading` and the new one `loading`, then `running`.
    - Bump the version in `crates/core/plugin-sdk/examples/hello/Cargo.toml` and run
      `HELLO_FEATURES=panic-on-load just plugin-deploy hello`. The backend records `error` with the
      panic message, and the previous version keeps serving.
    - Kill the `hello` container. It is marked `error` without affecting anything else; restarting
      it brings it back to `running`.

