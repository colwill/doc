BINARY  := {{ values.name }}
VERSION ?= 0.1.0
# Where the platform's secrets are: the `secrets` directory of the DOC checkout it runs beside.
SECRETS ?=
BACKEND ?= 127.0.0.1:4433
PORT    ?= 4490

.PHONY: build run vet test image

build:
	go build -ldflags "-X main.version=$(VERSION)" -o bin/$(BINARY) .

# Registers with a DOC backend running beside it; `make run PORT=4491` runs a second version.
run: build
	@test -n "$(SECRETS)" || { echo "Say where DOC's secrets are: make run SECRETS=path/to/doc/secrets"; exit 1; }
	DOC_SECRETS_DIR=$(SECRETS) DOC_BACKEND_QUIC=$(BACKEND) \
	DOC_PLUGIN_BIND=127.0.0.1:$(PORT) DOC_PLUGIN_ADVERTISE=127.0.0.1:$(PORT) ./bin/$(BINARY)

vet:
	go vet ./...

test:
	go test ./...

image:
	docker build --build-arg VERSION=$(VERSION) -t doc/$(BINARY):$(VERSION) .
