PACKAGE := {{ scaffold.package }}

.PHONY: proto run test lint

# The generated code is not committed: `make proto` writes it into src/$(PACKAGE)/gen.
proto:
	python -m grpc_tools.protoc -Iproto \
		--python_out=src/$(PACKAGE)/gen \
		--grpc_python_out=src/$(PACKAGE)/gen \
		--pyi_out=src/$(PACKAGE)/gen \
		proto/service.proto
	touch src/$(PACKAGE)/gen/__init__.py

run: proto
	python -m $(PACKAGE)

test:
	pytest -q

lint:
	ruff check src
