// What every service on this platform has: its settings, its telemetry and its feature flags.
// DOC wrote this when the service was created; it is yours to change.

export { loadConfig, type Config } from "./config.js";
export { Flags } from "./flags.js";
export { startTelemetry } from "./telemetry.js";
