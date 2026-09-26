# Getting started

Run the gateway locally against the scheme simulator:

```sh
make simulator
cargo run -p card-gateway -- --scheme http://localhost:9400
```

Payments arrive on `POST /v1/payments`. Read the [architecture overview](architecture/overview.md)
before changing how they are routed.

- [x] Clone the repository
- [ ] Ask payments-core for access to the scheme sandbox
