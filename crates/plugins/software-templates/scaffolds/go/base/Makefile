BINARY := {{ values.name }}

.PHONY: build test run tidy image

build:
	go build -o bin/$(BINARY) ./cmd/$(BINARY)

test:
	go test ./...

run: build
	./bin/$(BINARY)

tidy:
	go mod tidy

image:
	docker build -t $(BINARY):dev .
