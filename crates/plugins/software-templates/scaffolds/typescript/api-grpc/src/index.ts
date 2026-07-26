// {{ values.name }} — {{ values.description }}
//
// The contract is loaded from proto/service.proto at start-up, so there is no generation step: the
// .proto file is the only copy of the contract.

import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import * as grpc from "@grpc/grpc-js";
import * as loader from "@grpc/proto-loader";
import { HealthImplementation } from "grpc-health-check";

import { loadConfig, Flags, startTelemetry } from "./platform/index.js";
import { greet } from "./service.js";

const here = dirname(fileURLToPath(import.meta.url));
const definition = loader.loadSync(join(here, "..", "proto", "service.proto"), {
  keepCase: false,
  longs: String,
  enums: String,
  defaults: true,
  oneofs: true,
});
const contract = grpc.loadPackageDefinition(definition) as unknown as {
  {{ scaffold.package }}: { v1: { {{ scaffold.pascal }}Service: grpc.ServiceClientConstructor } };
};

const config = loadConfig();
const shutdown = startTelemetry(config);
const flags = await Flags.start(config);

const server = new grpc.Server();
server.addService(contract.{{ scaffold.package }}.v1.{{ scaffold.pascal }}Service.service, {
  greet: greet(config, flags),
});

// Health is what everything else expects a gRPC service to answer.
const health = new HealthImplementation({ "": "SERVING" });
health.addToServer(server);

const address = `${config.address}:9090`;
server.bindAsync(address, grpc.ServerCredentials.createInsecure(), (problem) => {
  if (problem !== null) {
    console.error("the port could not be taken", problem);
    process.exit(1);
  }
  console.log(`listening on ${address} as ${config.service}`);
});

for (const signal of ["SIGINT", "SIGTERM"] as const) {
  process.once(signal, () => {
    console.log("stopping");
    health.setStatus("", "NOT_SERVING");
    server.tryShutdown(() => {
      flags.stop();
      void shutdown();
    });
  });
}
