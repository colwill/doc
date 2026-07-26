// The platform's telemetry stack, as this service exports to it. Traces and metrics go to
// {{ telemetry.endpoint }} over {{ telemetry.protocol }}, and Node's own libraries are instrumented
// with it. Nothing here names the collector: it is read from OTEL_EXPORTER_OTLP_ENDPOINT, so
// pointing this elsewhere is a variable and not a change.

import { getNodeAutoInstrumentations } from "@opentelemetry/auto-instrumentations-node";
import { OTLPMetricExporter } from "@opentelemetry/exporter-metrics-otlp-grpc";
import { OTLPTraceExporter } from "@opentelemetry/exporter-trace-otlp-grpc";
import { resourceFromAttributes } from "@opentelemetry/resources";
import { PeriodicExportingMetricReader } from "@opentelemetry/sdk-metrics";
import { NodeSDK } from "@opentelemetry/sdk-node";
import {
  ATTR_SERVICE_NAME,
  ATTR_SERVICE_NAMESPACE,
} from "@opentelemetry/semantic-conventions";

import type { Config } from "./config.js";

/** Starts it, and answers with the flush to call before the process exits. */
export function startTelemetry(config: Config): () => Promise<void> {
  const sdk = new NodeSDK({
    resource: resourceFromAttributes({
      [ATTR_SERVICE_NAME]: config.service,
      [ATTR_SERVICE_NAMESPACE]: "{{ telemetry.namespace }}",
      "deployment.environment.name": config.environment,
    }),
    traceExporter: new OTLPTraceExporter(),
    metricReader: new PeriodicExportingMetricReader({
      exporter: new OTLPMetricExporter(),
      exportIntervalMillis: 30_000,
    }),
    instrumentations: [getNodeAutoInstrumentations()],
  });

  sdk.start();
  return async () => {
    await sdk.shutdown();
  };
}
