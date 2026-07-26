#!/usr/bin/env node
// {{ values.name }} — {{ values.description }}

import { loadConfig, Flags, startTelemetry } from "./platform/index.js";
import { run } from "./commands.js";

const config = loadConfig();
// A command still runs when the collector cannot be reached; it is only not measured.
let shutdown: () => Promise<void> = async () => {};
try {
  shutdown = startTelemetry(config);
} catch (problem) {
  console.warn("telemetry is not being exported", problem);
}

const flags = await Flags.start(config);
try {
  process.exitCode = await run(config, flags, process.argv.slice(2));
} finally {
  flags.stop();
  await shutdown();
}
