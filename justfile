set shell := ["bash", "-euo", "pipefail", "-c"]

stacks := "storage fabric core workers plugins"
compose := "docker compose" + if path_exists(".env") == "true" { " --env-file .env" } else { "" }

# List the recipes
default:
    @just --list

kill-plugins:
    pkill -f "cargo-watch.*CLionProjects/akea"

# Format all Rust code
fmt:
    cargo fmt --all

# Run clippy with warnings as errors
lint:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

# Run the unit tests
test:
    cargo test --workspace --all-features

# RustSec advisories and licences for every crate in the build (deny.toml)
deny:
    @command -v cargo-deny >/dev/null || { echo "cargo-deny is not installed: cargo install --locked cargo-deny"; exit 1; }
    cargo deny check advisories licenses

# Formatting, lints, advisories and licences, and unit tests
check:
    cargo fmt --all --check
    just lint
    just deny
    just test

# Rebuild and restart the backend, frontend and every plugin on each edit, against Postgres in Docker
dev fabric="memory":
    @command -v cargo-watch >/dev/null || { echo "cargo-watch is not installed: cargo install cargo-watch"; exit 1; }
    {{ compose }} -f crates/storage/docker-compose.yaml up -d --wait
    {{ compose }} -f crates/storage/docker-compose.yaml run --rm bootstrap
    @if [ "{{ fabric }}" = "cluster" ]; then {{ compose }} -f crates/fabric/docker-compose.yaml up -d --build --wait; fi
    @{{ compose }} -f crates/core/docker-compose.yaml stop backend frontend 2>/dev/null || true
    just dev-secrets
    trap 'kill 0' EXIT INT TERM; \
    just dev-backend {{ fabric }} & \
    just dev-frontend {{ fabric }} & \
    just dev-workers {{ fabric }} & \
    just dev-plugins & \
    wait

# The bootstrap keeps what is there, so running it every time only adds what a new plugin ID needs.
# Fill ./secrets on the host, so a local run has the same CA, certificates and tokens as Docker
dev-secrets:
    @DOC_CONFIG=config/doc.toml DOC_SECRETS_DIR=secrets \
        cargo run --quiet -p doc-backend -- bootstrap

# Stops whatever of ours is already on `port`/`proto`, so a previous run that did not shut down
# cleanly — a closed terminal, a killed pane, a crash — does not block this one from binding it.
# Only a process running one of our own binaries, found by what is actually listening and never
# guessed from a name or a port alone, is touched.
_free port proto="tcp":
    #!/usr/bin/env bash
    set -euo pipefail
    root="{{ justfile_directory() }}"
    flag=t; [ "{{ proto }}" = udp ] && flag=u
    for pid in $(ss -H "-l${flag}np" "sport = :{{ port }}" 2>/dev/null \
            | grep -o 'pid=[0-9]*' | cut -d= -f2 | sort -u); do
        exe=$(readlink -f "/proc/$pid/exe" 2>/dev/null || true)
        case "$exe" in
            "$root"/target/*|"$root"/crates/core/plugin-sdk/examples/*/target/*)
                echo "just: stopping $(basename "$exe") still on {{ proto }}/{{ port }} (pid $pid)"
                kill "$pid" 2>/dev/null || true
                for _ in $(seq 1 20); do
                    kill -0 "$pid" 2>/dev/null || break
                    sleep 0.1
                done
                ;;
        esac
    done

# One part of `just dev`; run it alone to watch only the backend
dev-backend fabric="memory":
    @just _free 8080 tcp
    @just _free 4433 udp
    DOC_CONFIG=config/doc.toml \
    DOC_SECRETS_DIR=secrets \
    DOC_FABRIC_MODE={{ fabric }} \
    DOC_PLUGINS_ALLOW_REBUILDS=${DOC_PLUGINS_ALLOW_REBUILDS:-true} \
    DOC_EVENT_BUS_NODES=${DEV_EVENT_BUS_NODES:-127.0.0.1:14431,127.0.0.1:14432,127.0.0.1:14433} \
    DOC_SERVICE_BUS_NODES=${DEV_SERVICE_BUS_NODES:-127.0.0.1:14441,127.0.0.1:14442,127.0.0.1:14443} \
    DOC_CACHE_BUS_NODES=${DEV_CACHE_BUS_NODES:-127.0.0.1:14451,127.0.0.1:14452,127.0.0.1:14453} \
    RUST_LOG=${DOC_BACKEND_LOG:-info,openraft=warn,quiche=warn,tokio_quiche=warn} \
    cargo watch --why -w crates -i 'crates/core/frontend/**' -i 'crates/plugins/**' \
        -i 'crates/core/plugin-sdk/**' -w config \
        -x 'run --quiet -p doc-backend -- bootstrap' -x 'run -p doc-backend'

# One part of `just dev`: the cron and the probes, without which no schedule ever comes round
dev-workers fabric="memory":
    DOC_CONFIG=config/doc.toml \
    DOC_SECRETS_DIR=secrets \
    DOC_FABRIC_MODE={{ fabric }} \
    DOC_WORKER_NAME=${DOC_WORKER_NAME:-workers} \
    DOC_BACKEND_URL=${DOC_BACKEND_URL:-http://127.0.0.1:8080} \
    DOC_FRONTEND_URL=${DOC_FRONTEND_URL:-http://127.0.0.1:8081} \
    DOC_EVENT_BUS_NODES=${DEV_EVENT_BUS_NODES:-127.0.0.1:14431,127.0.0.1:14432,127.0.0.1:14433} \
    DOC_SERVICE_BUS_NODES=${DEV_SERVICE_BUS_NODES:-127.0.0.1:14441,127.0.0.1:14442,127.0.0.1:14443} \
    DOC_CACHE_BUS_NODES=${DEV_CACHE_BUS_NODES:-127.0.0.1:14451,127.0.0.1:14452,127.0.0.1:14453} \
    RUST_LOG=${DOC_WORKERS_LOG:-info,openraft=warn,quiche=warn,tokio_quiche=warn} \
    cargo watch --why -w crates/workers -w crates/fabric -w config/doc.toml \
        -x 'run -p doc-workers'

# One part of `just dev`; the page reloads itself once the rebuilt frontend is serving
dev-frontend fabric="memory":
    @just _free 8081 tcp
    DOC_CONFIG=config/doc-frontend.toml \
    DOC_SECRETS_DIR=secrets \
    DOC_FABRIC_MODE={{ fabric }} \
    DOC_DEV_RELOAD=1 \
    DOC_EVENT_BUS_NODES=${DEV_EVENT_BUS_NODES:-127.0.0.1:14431,127.0.0.1:14432,127.0.0.1:14433} \
    DOC_CACHE_BUS_NODES=${DEV_CACHE_BUS_NODES:-127.0.0.1:14451,127.0.0.1:14452,127.0.0.1:14453} \
    RUST_LOG=${DOC_FRONTEND_LOG:-info} \
    cargo watch --why -w crates/core/frontend -w config/doc-frontend.toml -x 'run -p doc-frontend'

# A plugin is any crate whose main calls `doc_plugin_sdk::main!`; each gets its own port from 4440.
# One part of `just dev`: every plugin, rebuilt and restarted when it or the SDK changes
dev-plugins:
    #!/usr/bin/env bash
    set -euo pipefail
    root="{{ justfile_directory() }}"
    sdk="$root/crates/core/plugin-sdk"
    shared=(-w "$sdk/src" -w "$sdk/Cargo.toml" -w "$root/crates/core/plugin-protocol/src" \
        -w "$root/crates/core/transport/src")
    port=${DEV_PLUGIN_PORT:-4440}
    instance=""
    DOC_OIDC_INSTANCES=${DOC_OIDC_INSTANCES:-}
    # A plugin reads its own settings (OAuth apps, tokens, schedules) from the environment, and the
    # README puts them in `.env`, which only `docker compose` reads. Without this, `just dev` runs
    # every plugin unconfigured: GitHub sign-in, for one, stays off however `.env` is filled in.
    # Core's own `.env` values are left alone, since they name hosts only Docker can reach.
    if [ -f "$root/.env" ]; then
        while IFS= read -r line || [ -n "$line" ]; do
            line=${line#export }
            case "$line" in
                ''|'#'*) continue ;;
                *=*) ;;
                *) continue ;;
            esac
            key=${line%%=*}
            case "$key" in
                ''|*[!A-Za-z0-9_]*) continue ;;
            esac
            value=${line#*=}
            case "$value" in
                \"*\") value=${value#\"}; value=${value%\"} ;;
                \'*\') value=${value#\'}; value=${value%\'} ;;
            esac
            export "$key"="$value"
        done < "$root/.env"
    fi
    # cargo watch runs each build in a process group of its own, which outlives cargo watch when it
    # is stopped. Each one gets a session of its own instead, and leaving ends every session.
    sessions=()
    trap 'for session in "${sessions[@]}"; do pkill -TERM -s "$session" 2>/dev/null; done' EXIT
    run() {
        local name=$1 port=$2
        shift 2
        just _free "$port" udp
        echo "dev-plugins: $name on 127.0.0.1:$port"
        DOC_SECRETS_DIR="$root/secrets" \
        DOC_BACKEND_QUIC=${DEV_BACKEND_QUIC:-127.0.0.1:4433} \
        DOC_PLUGIN_BIND=127.0.0.1:$port \
        DOC_PLUGIN_ADVERTISE=127.0.0.1:$port \
        DOC_OIDC_PLUGIN_ID=${instance:-} \
        RUST_LOG=${DOC_PLUGIN_LOG:-info,quiche=warn,tokio_quiche=warn} \
        setsid cargo watch --why "${shared[@]}" "$@" &
        sessions+=($!)
    }
    for main in "$root"/crates/plugins/*/src/main.rs; do
        grep -q 'doc_plugin_sdk::main!' "$main" || continue
        dir=$(dirname "$(dirname "$main")")
        package=$(sed -n 's/^name *= *"\(.*\)"/\1/p' "$dir/Cargo.toml" | head -1)
        run "$package" "$port" -w "$dir" -x "run -p $package"
        port=$((port + 1))
    done
    # A second OpenID Connect provider is the same binary under another ID, with settings of its
    # own: DOC_GOOGLE_* for `google`. Name them in DOC_OIDC_INSTANCES, comma separated, and put
    # each ID in config/doc.toml so it may register at all.
    for instance in ${DOC_OIDC_INSTANCES//,/ }; do
        run "doc-oidc as $instance" "$port" -w "$root/crates/plugins/oidc" -x "run -p doc-oidc"
        port=$((port + 1))
    done
    instance=""
    hello="$sdk/examples/hello"
    run doc-hello-plugin "$port" -C "$hello" -w "$hello/src" -w "$hello/Cargo.toml" -x run
    wait

# Start storage, the fabric, core, workers, then plugins
up:
    @[ -f .env ] || echo "note: no .env file; the compose files fall back to dev defaults (see .env.example)"
    for stack in {{ stacks }}; do \
        file="crates/$stack/docker-compose.yaml"; \
        [ -f "$file" ] || continue; \
        if {{ compose }} --profile bootstrap -f "$file" config --services | grep -x bootstrap >/dev/null; then \
            {{ compose }} -f "$file" run --rm --build bootstrap; \
        fi; \
    done
    for stack in {{ stacks }}; do \
        file="crates/$stack/docker-compose.yaml"; \
        if [ ! -f "$file" ]; then echo "skipping $stack: $file does not exist yet"; continue; fi; \
        if [ -n "$({{ compose }} -f "$file" config --services)" ]; then \
            {{ compose }} -f "$file" up -d --build --wait; \
        fi; \
    done

# Stop every stack, plugins first, including plugin containers `plugin-deploy` started
down:
    for stack in $(echo {{ stacks }} | tr ' ' '\n' | tac); do \
        file="crates/$stack/docker-compose.yaml"; \
        if [ -f "$file" ]; then {{ compose }} -f "$file" down --remove-orphans; fi; \
    done

# Build the example plugin, which lives outside the workspace on purpose. `variant` picks a
# classification or a failure mode: synchronous, long-running, one-shot, task, panic-on-load,
# hang-on-load, tour or trespass (combine them with commas, e.g. "tour,trespass").
example-plugin variant="synchronous":
    cd crates/core/plugin-sdk/examples/hello && \
        cargo build --no-default-features --features {{ variant }}

# Makes its token and certificate, restarts the backend so it records the ID, then builds and
# starts the plugin. `plugin` is its Compose service.
# Start a plugin whose ID is new to the configuration
plugin-add plugin:
    {{ compose }} --profile bootstrap -f crates/core/docker-compose.yaml run --rm --build bootstrap
    {{ compose }} -f crates/core/docker-compose.yaml up -d --no-deps --force-recreate --wait backend
    {{ compose }} -f crates/plugins/docker-compose.yaml up -d --build --wait {{ plugin }}

# Build a plugin and start it next to the running version, which hands over and exits
plugin-deploy plugin:
    {{ compose }} -f crates/plugins/docker-compose.yaml build {{ plugin }}
    # `compose run` ignores the service's restart policy, so the container is given it directly.
    docker update --restart on-failure \
        "$({{ compose }} -f crates/plugins/docker-compose.yaml run -d --no-deps --name "${DOC_STACK:-doc}-{{ plugin }}-$(date +%s)" {{ plugin }})"

# An OpenID Connect provider to develop the `oidc` plugin against, with two people in it
oidc-dev:
    {{ compose }} -f crates/plugins/oidc/dev/docker-compose.yaml up -d --wait
    @echo "issuer http://127.0.0.1:${OIDC_PORT:-8090}/realms/doc, client doc-platform, people ada and grace"

# Stop it again
oidc-dev-down:
    {{ compose }} -f crates/plugins/oidc/dev/docker-compose.yaml down

# Render docs/ARCH.md's Mermaid diagrams to the PNGs beside it
arch:
    #!/usr/bin/env bash
    set -euo pipefail
    command -v mmdc >/dev/null || { echo "mermaid-cli is not installed: npm i -g @mermaid-js/mermaid-cli"; exit 1; }
    root="{{ justfile_directory() }}/docs"
    work=$(mktemp -d)
    trap 'rm -rf "$work"' EXIT
    python3 - "$root/ARCH.md" "$work" <<'PY'
    import re, sys, pathlib
    text = pathlib.Path(sys.argv[1]).read_text()
    for block in re.finditer(r'```mermaid\n(.*?)```', text, re.S):
        heading = text.rfind('\n## ', 0, block.start())
        title = text[heading:block.start()].split('\n')[1][3:].strip().lower()
        slug = re.sub(r'[^a-z0-9]+', '-', title).strip('-')
        pathlib.Path(sys.argv[2], slug + '.mmd').write_text(block.group(1))
    PY
    for file in "$work"/*.mmd; do
        name=$(basename "$file" .mmd)
        mmdc -i "$file" -o "$root/arch-$name.png" -b white -w 1600 >/dev/null
        echo "docs/arch-$name.png"
    done

# Re-check the vendored frontend assets against their recorded SHA-256 hashes
verify-assets:
    cd crates/core/frontend/assets/vendor && \
    sed -nE 's/^ *\{ path = "([^"]+)".*sha256 = "([0-9a-f]{64})".*/\2  \1/p' MANIFEST.toml | sha256sum -c -
