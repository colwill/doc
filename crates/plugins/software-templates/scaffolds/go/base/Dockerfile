FROM golang:1.24-alpine AS build
WORKDIR /src
COPY go.mod go.sum* ./
RUN go mod download
COPY . .
RUN CGO_ENABLED=0 go build -trimpath -o /out/{{ values.name }} ./cmd/{{ values.name }}

FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=build /out/{{ values.name }} /{{ values.name }}
USER nonroot:nonroot
ENTRYPOINT ["/{{ values.name }}"]
