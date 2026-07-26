BINARY := {{ values.name }}
CONTROLLER_GEN ?= go run sigs.k8s.io/controller-tools/cmd/controller-gen@v0.17.2

.PHONY: build test run generate manifests install deploy image

# The deep-copy functions every Kubernetes type needs.
generate:
	$(CONTROLLER_GEN) object:headerFile=/dev/null paths=./api/...

# The CRD in config/crd/bases, written from the markers on the types.
manifests:
	$(CONTROLLER_GEN) crd rbac:roleName=$(BINARY) paths=./... output:crd:artifacts:config=config/crd/bases

build: generate
	go build -o bin/$(BINARY) ./cmd/$(BINARY)

test:
	go test ./...

run: build
	./bin/$(BINARY)

install:
	kubectl apply -f config/crd/bases

deploy:
	kubectl apply -k deploy/kubernetes

image:
	docker build -t $(BINARY):dev .
