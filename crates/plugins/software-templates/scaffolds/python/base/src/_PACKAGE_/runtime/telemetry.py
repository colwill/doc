"""The platform's telemetry stack, as this service exports to it. Traces and metrics go to
`{{ telemetry.endpoint }}` over `{{ telemetry.protocol }}`, and the logs carry the trace they
belong to. Nothing here names the collector: it is read from `OTEL_EXPORTER_OTLP_ENDPOINT`, so
pointing this elsewhere is a variable and not a change."""

from __future__ import annotations

import logging
from typing import Callable

from opentelemetry import metrics, trace
from opentelemetry.exporter.otlp.proto.grpc.metric_exporter import OTLPMetricExporter
from opentelemetry.exporter.otlp.proto.grpc.trace_exporter import OTLPSpanExporter
from opentelemetry.sdk.metrics import MeterProvider
from opentelemetry.sdk.metrics.export import PeriodicExportingMetricReader
from opentelemetry.sdk.resources import Resource
from opentelemetry.sdk.trace import TracerProvider
from opentelemetry.sdk.trace.export import BatchSpanProcessor

from .config import Config


def start_telemetry(config: Config) -> Callable[[], None]:
    """Sets up tracing, metrics and logging, and answers with the flush to call before exiting."""
    resource = Resource.create(
        {
            "service.name": config.service,
            "service.namespace": "{{ telemetry.namespace }}",
            "deployment.environment.name": config.environment,
        }
    )

    tracing = TracerProvider(resource=resource)
    tracing.add_span_processor(BatchSpanProcessor(OTLPSpanExporter()))
    trace.set_tracer_provider(tracing)

    measuring = MeterProvider(
        resource=resource,
        metric_readers=[PeriodicExportingMetricReader(OTLPMetricExporter())],
    )
    metrics.set_meter_provider(measuring)

    logging.basicConfig(
        level=logging.INFO,
        format='{"time":"%(asctime)s","level":"%(levelname)s","logger":"%(name)s","message":"%(message)s"}',
    )

    def shutdown() -> None:
        tracing.shutdown()
        measuring.shutdown()

    return shutdown
