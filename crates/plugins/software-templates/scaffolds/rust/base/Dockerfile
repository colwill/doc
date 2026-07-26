FROM rust:1.85-slim AS build
WORKDIR /src
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config && rm -rf /var/lib/apt/lists/*
COPY . .
RUN cargo build --release --locked

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /src/target/release/{{ values.name }} /{{ values.name }}
USER nonroot:nonroot
ENTRYPOINT ["/{{ values.name }}"]
