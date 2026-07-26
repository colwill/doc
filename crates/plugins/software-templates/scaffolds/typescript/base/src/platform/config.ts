// What the service reads from its environment at start-up. Anything that should change without a
// deployment belongs in Flags instead, which is read while it runs.

export interface Config {
  service: string;
  environment: string;
  address: string;
  port: number;
  flagsUrl: string;
  flagsToken: string | undefined;
  flagsPollSeconds: number;
}

/** The environment, falling back to what DOC knew when this service was created. */
export function loadConfig(): Config {
  return {
    service: text("OTEL_SERVICE_NAME", "{{ values.name }}"),
    environment: text("DOC_ENVIRONMENT", "{{ telemetry.environment }}"),
    address: text("ADDRESS", "0.0.0.0"),
    port: number("PORT", 8080),
    flagsUrl: text("DOC_FLAGS_URL", "{{ flags.url }}"),
    flagsToken: process.env.DOC_FLAGS_TOKEN || undefined,
    flagsPollSeconds: number("DOC_FLAGS_POLL_SECONDS", 30),
  };
}

function text(name: string, fallback: string): string {
  const value = process.env[name];
  return value === undefined || value === "" ? fallback : value;
}

function number(name: string, fallback: number): number {
  const value = Number(process.env[name]);
  return Number.isFinite(value) && value > 0 ? value : fallback;
}
