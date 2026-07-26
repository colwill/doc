"""{{ values.name }}'s gRPC service."""

from __future__ import annotations

import grpc
from opentelemetry import trace

from .gen import service_pb2, service_pb2_grpc
from .runtime import Config, Flags

tracer = trace.get_tracer("{{ values.name }}")


class Service(service_pb2_grpc.{{ scaffold.pascal }}ServiceServicer):
    """Answers {{ scaffold.pascal }}Service."""

    def __init__(self, config: Config, flags: Flags) -> None:
        self._config = config
        self._flags = flags

    def Greet(  # noqa: N802 - the name is the protobuf method's
        self,
        request: service_pb2.GreetRequest,
        context: grpc.ServicerContext,
    ) -> service_pb2.GreetResponse:
        with tracer.start_as_current_span("greet"):
            who = request.who or "world"
            # What it says is a flag in DOC, with the value this service falls back to beside it.
            greeting = self._flags.string("greeting", "Hello")
            if self._flags.boolean("shout", False):
                greeting = greeting.upper()
            return service_pb2.GreetResponse(
                message=f"{greeting}, {who}",
                service=self._config.service,
                environment=self._config.environment,
            )
