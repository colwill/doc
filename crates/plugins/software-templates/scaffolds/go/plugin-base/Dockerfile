FROM golang:1.24-alpine AS build
ARG VERSION=0.1.0
WORKDIR /src
COPY go.mod go.sum ./
RUN go mod download
COPY . .
RUN CGO_ENABLED=0 go build -trimpath -ldflags "-X main.version=${VERSION}" -o /out/{{ values.name }} .

# The secrets volume is mounted read-only at /secrets, as for every DOC plugin.
FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=build /out/{{ values.name }} /{{ values.name }}
USER nonroot:nonroot
ENTRYPOINT ["/{{ values.name }}"]
