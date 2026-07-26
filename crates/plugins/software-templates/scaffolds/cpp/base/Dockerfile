FROM debian:bookworm-slim AS build
RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential cmake git ca-certificates libcurl4-openssl-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN cmake -S . -B build -DCMAKE_BUILD_TYPE=Release && cmake --build build --parallel

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libcurl4 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 10001 --create-home service
COPY --from=build /src/build/{{ values.name }} /usr/local/bin/{{ values.name }}
USER 10001
ENTRYPOINT ["/usr/local/bin/{{ values.name }}"]
