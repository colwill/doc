BINARY := {{ values.name }}

.PHONY: build test run proto tidy image

# The generated code is not committed: `make proto` writes it into gen/ before the first build.
proto:
	buf generate

build: proto
	go build -o bin/$(BINARY) ./cmd/$(BINARY)

test:
	go test ./...

run: build
	./bin/$(BINARY)

tidy:
	go mod tidy

image:
	docker build -t $(BINARY):dev .
