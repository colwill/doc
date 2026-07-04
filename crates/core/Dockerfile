# syntax=docker/dockerfile:1.7
# Shared image recipe - pass BIN to pick which binary to build.
ARG RUST_VERSION=1.98.1

FROM rust:${RUST_VERSION}-slim-trixie AS builder
ARG BIN=doc-backend
# A plugin outside the workspace, such as the SDK's hello example, builds from its own manifest.
ARG MANIFEST=Cargo.toml
ARG FEATURES=""
WORKDIR /build
# cmake, make, a C++ toolchain and libclang build the BoringSSL that quiche vendors.
RUN apt-get update \
    && apt-get install -y --no-install-recommends git cmake make g++ perl clang libclang-dev \
    && rm -rf /var/lib/apt/lists/*
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/build/target,sharing=locked \
    cargo build --release --locked --manifest-path ${MANIFEST} --target-dir /build/target \
        --bin ${BIN} ${FEATURES:+--features ${FEATURES}} \
    && install -Dm0755 "target/release/${BIN}" /out/status

# ccc, for the Repository Insights plugin, which runs it over repositories. Only a build that names
# CCC_VERSION gets it, and the download must match the checksum given for this architecture.
FROM debian:trixie-slim AS tools
ARG CCC_VERSION=""
ARG CCC_SHA256_AMD64=""
ARG CCC_SHA256_ARM64=""
ARG TARGETARCH
RUN mkdir -p /out \
    && if [ -n "$CCC_VERSION" ]; then \
        case "${TARGETARCH:-amd64}" in \
            amd64) asset=ccc-linux-x86_64; sum="$CCC_SHA256_AMD64" ;; \
            arm64) asset=ccc-linux-aarch64; sum="$CCC_SHA256_ARM64" ;; \
            *) echo "no ccc build is published for ${TARGETARCH}" >&2; exit 1 ;; \
        esac \
        && if [ -z "$sum" ]; then echo "no checksum is given for $asset" >&2; exit 1; fi \
        && apt-get update \
        && apt-get install -y --no-install-recommends ca-certificates curl \
        && curl -fsSL --retry 2 -o /out/ccc \
            "https://github.com/colwill/ccc/releases/download/${CCC_VERSION}/${asset}" \
        && echo "${sum}  /out/ccc" | sha256sum -c - \
        && chmod 0755 /out/ccc; \
    fi

FROM debian:trixie-slim AS runtime
# git goes with ccc: Repository Insights commits two scans' files to work out what changed.
ARG CCC_VERSION=""
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates ${CCC_VERSION:+git} \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --user-group --home-dir /app --shell /usr/sbin/nologin doc \
    # A fresh named volume mounted at /data inherits this ownership, so services can write to it.
    && mkdir -p /app /data && chown doc:doc /app /data
COPY --from=builder /out/status /usr/local/bin/status
COPY --from=tools /out/ /usr/local/bin/
USER 10001:10001
WORKDIR /app
ENTRYPOINT ["/usr/local/bin/status"]
