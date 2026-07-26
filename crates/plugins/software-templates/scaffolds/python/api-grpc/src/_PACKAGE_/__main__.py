"""{{ values.name }} — {{ values.description }}

Run `make proto` before the first start: the generated code is not committed.
"""

from __future__ import annotations

import logging
from concurrent import futures

import grpc
from grpc_health.v1 import health, health_pb2, health_pb2_grpc
from grpc_reflection.v1alpha import reflection
from opentelemetry.instrumentation.grpc import GrpcInstrumentorServer

from .gen import service_pb2, service_pb2_grpc
from .rpc import Service
from .runtime import Config, Flags, start_telemetry

logger = logging.getLogger(__name__)


def main() -> None:
    config = Config.load()
    shutdown = start_telemetry(config)
    GrpcInstrumentorServer().instrument()
    flags = Flags.start(config)

    server = grpc.server(futures.ThreadPoolExecutor(max_workers=16))
    service_pb2_grpc.add_{{ scaffold.pascal }}ServiceServicer_to_server(Service(config, flags), server)

    # Health and reflection are what everything else expects a gRPC service to answer.
    checks = health.HealthServicer()
    health_pb2_grpc.add_HealthServicer_to_server(checks, server)
    checks.set("", health_pb2.HealthCheckResponse.SERVING)
    reflection.enable_server_reflection(
        (
            service_pb2.DESCRIPTOR.services_by_name["{{ scaffold.pascal }}Service"].full_name,
            health_pb2.DESCRIPTOR.services_by_name["Health"].full_name,
            reflection.SERVICE_NAME,
        ),
        server,
    )

    address = f"{config.address}:9090"
    server.add_insecure_port(address)
    server.start()
    logger.info("listening on %s as %s", address, config.service)
    try:
        server.wait_for_termination()
    finally:
        checks.set("", health_pb2.HealthCheckResponse.NOT_SERVING)
        server.stop(grace=10).wait()
        flags.stop()
        shutdown()


if __name__ == "__main__":
    main()
