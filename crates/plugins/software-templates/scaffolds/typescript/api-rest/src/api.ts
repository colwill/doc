// {{ values.name }}'s HTTP API. Every request is traced and counted through the platform's own
// telemetry stack, and what it answers follows the flags DOC holds for this service.

import { metrics, trace } from "@opentelemetry/api";
import Fastify, { type FastifyInstance } from "fastify";

import type { Config, Flags } from "./platform/index.js";

const tracer = trace.getTracer("{{ values.name }}");
const meter = metrics.getMeter("{{ values.name }}");
const requests = meter.createCounter("http.server.requests", {
  description: "Requests this service answered",
});

/** Everything {{ values.name }} answers. */
export function build(config: Config, flags: Flags): FastifyInstance {
  const api = Fastify({ logger: true });

  // Liveness: the process is up. Kubernetes restarts it when this stops answering.
  api.get("/healthz", async () => ({ status: "ok" }));

  // Readiness: it is up and willing to take traffic, which a flag can withdraw.
  api.get("/readyz", async (_request, reply) => {
    if (!flags.boolean("accepting-traffic", true)) {
      return reply.code(503).send({ status: "draining" });
    }
    return { status: "ready" };
  });

  api.get("/api/v1/hello", async () =>
    tracer.startActiveSpan("hello", (span) => {
      requests.add(1, { "http.route": "/api/v1/hello" });

      // What the service says is decided in DOC, with a fallback it keeps working on.
      const greeting = flags.string("greeting", "Hello");
      span.setAttribute("greeting", greeting);
      span.end();
      return {
        message: greeting,
        service: config.service,
        environment: config.environment,
      };
    }),
  );

  return api;
}
