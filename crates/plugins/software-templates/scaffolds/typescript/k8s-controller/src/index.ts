// {{ values.name }} — {{ values.description }}

import * as k8s from "@kubernetes/client-node";

import { loadConfig, Flags, startTelemetry } from "./platform/index.js";
import { Controller } from "./controller.js";

const config = loadConfig();
const shutdown = startTelemetry(config);
const flags = await Flags.start(config);

const kube = new k8s.KubeConfig();
kube.loadFromDefault();

const controller = new Controller(kube, config, flags);

for (const signal of ["SIGINT", "SIGTERM"] as const) {
  process.once(signal, () => {
    void (async () => {
      console.log("stopping");
      flags.stop();
      await shutdown();
      process.exit(0);
    })();
  });
}

await controller.run();
