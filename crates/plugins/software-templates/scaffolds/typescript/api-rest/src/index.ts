// {{ values.name }} — {{ values.description }}

import { loadConfig, Flags, startTelemetry } from "./platform/index.js";
import { build } from "./api.js";

const config = loadConfig();
const shutdown = startTelemetry(config);
const flags = await Flags.start(config);
const api = build(config, flags);

for (const signal of ["SIGINT", "SIGTERM"] as const) {
  process.once(signal, () => {
    void (async () => {
      api.log.info("stopping");
      await api.close();
      flags.stop();
      await shutdown();
    })();
  });
}

await api.listen({ host: config.address, port: config.port });
