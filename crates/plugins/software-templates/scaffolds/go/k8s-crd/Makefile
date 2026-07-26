BINARY := {{ values.name }}
CONTROLLER_GEN ?= go run sigs.k8s.io/controller-tools/cmd/controller-gen@v0.17.2

.PHONY: build test generate manifests install sample

# The deep-copy functions the API types need.
generate:
	$(CONTROLLER_GEN) object:headerFile=/dev/null paths=./api/...

# config/crd/bases, written from the markers on the types. Keep the two in step by running this
# after every change to the types rather than editing the YAML.
manifests:
	$(CONTROLLER_GEN) crd paths=./api/... output:crd:artifacts:config=config/crd/bases

build: generate
	go build -o bin/$(BINARY) ./cmd/$(BINARY)

test:
	go test ./...

install:
	kubectl apply -f config/crd/bases

sample: install
	kubectl apply -f config/samples
