"""{{ values.name }}'s HTTP API. Every request is traced and counted through the platform's own
telemetry stack, and what it answers follows the flags DOC holds for this service."""

from __future__ import annotations

from fastapi import FastAPI, Response
from opentelemetry import metrics, trace
from opentelemetry.instrumentation.fastapi import FastAPIInstrumentor

from .runtime import Config, Flags

tracer = trace.get_tracer("{{ values.name }}")
meter = metrics.get_meter("{{ values.name }}")
requests = meter.create_counter("http.server.requests", description="Requests this service answered")


def build(config: Config, flags: Flags) -> FastAPI:
    """Everything {{ values.name }} answers."""
    api = FastAPI(title="{{ values.name }}", description="{{ values.description }}", version="0.1.0")

    @api.get("/healthz")
    def healthz() -> dict[str, str]:
        """Liveness: the process is up. Kubernetes restarts it when this stops answering."""
        return {"status": "ok"}

    @api.get("/readyz")
    def readyz(response: Response) -> dict[str, str]:
        """Readiness: it is up and willing to take traffic, which a flag can withdraw."""
        if not flags.boolean("accepting-traffic", True):
            response.status_code = 503
            return {"status": "draining"}
        return {"status": "ready"}

    @api.get("/api/v1/hello")
    def hello() -> dict[str, str]:
        requests.add(1, {"http.route": "/api/v1/hello"})
        with tracer.start_as_current_span("hello") as span:
            # What the service says is decided in DOC, with a fallback it keeps working on.
            greeting = flags.string("greeting", "Hello")
            span.set_attribute("greeting", greeting)
            return {
                "message": greeting,
                "service": config.service,
                "environment": config.environment,
            }

    FastAPIInstrumentor.instrument_app(api)
    return api
