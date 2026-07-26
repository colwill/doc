package platform

import (
	"context"
	"errors"
	"fmt"
	"strings"
	"time"

	"go.opentelemetry.io/otel"
	"go.opentelemetry.io/otel/attribute"
	"go.opentelemetry.io/otel/exporters/otlp/otlpmetric/otlpmetricgrpc"
	"go.opentelemetry.io/otel/exporters/otlp/otlpmetric/otlpmetrichttp"
	"go.opentelemetry.io/otel/exporters/otlp/otlptrace/otlptracegrpc"
	"go.opentelemetry.io/otel/exporters/otlp/otlptrace/otlptracehttp"
	"go.opentelemetry.io/otel/propagation"
	sdkmetric "go.opentelemetry.io/otel/sdk/metric"
	"go.opentelemetry.io/otel/sdk/resource"
	sdktrace "go.opentelemetry.io/otel/sdk/trace"
	semconv "go.opentelemetry.io/otel/semconv/v1.26.0"
	"go.opentelemetry.io/otel/trace"
)

// Telemetry is the platform's stack, as this service exports to it. Traces and metrics go to
// {{ telemetry.endpoint }} over {{ telemetry.protocol }}; the endpoint and everything else can be
// overridden by the usual OTEL_ variables, so nothing here has to change to point it somewhere else.
type Telemetry struct {
	Tracer   trace.Tracer
	shutdown []func(context.Context) error
}

// StartTelemetry sets up tracing and metrics and returns them with a shutdown that flushes both.
func StartTelemetry(ctx context.Context, config Config) (*Telemetry, error) {
	resources, err := resource.Merge(resource.Default(), resource.NewWithAttributes(
		semconv.SchemaURL,
		semconv.ServiceName(config.Service),
		semconv.DeploymentEnvironmentName(config.Environment),
		attribute.String("service.namespace", "{{ telemetry.namespace }}"),
	))
	if err != nil {
		return nil, fmt.Errorf("describing this service: %w", err)
	}

	telemetry := &Telemetry{}
	http := strings.HasPrefix(config.OTLPProtocol, "http")

	var traces sdktrace.SpanExporter
	if http {
		traces, err = otlptracehttp.New(ctx)
	} else {
		traces, err = otlptracegrpc.New(ctx)
	}
	if err != nil {
		return nil, fmt.Errorf("connecting to the trace collector: %w", err)
	}
	tracing := sdktrace.NewTracerProvider(
		sdktrace.WithBatcher(traces),
		sdktrace.WithResource(resources),
	)
	otel.SetTracerProvider(tracing)
	otel.SetTextMapPropagator(propagation.NewCompositeTextMapPropagator(
		propagation.TraceContext{}, propagation.Baggage{},
	))
	telemetry.shutdown = append(telemetry.shutdown, tracing.Shutdown)

	var metrics sdkmetric.Exporter
	if http {
		metrics, err = otlpmetrichttp.New(ctx)
	} else {
		metrics, err = otlpmetricgrpc.New(ctx)
	}
	if err != nil {
		return nil, fmt.Errorf("connecting to the metric collector: %w", err)
	}
	measuring := sdkmetric.NewMeterProvider(
		sdkmetric.WithReader(sdkmetric.NewPeriodicReader(metrics, sdkmetric.WithInterval(30*time.Second))),
		sdkmetric.WithResource(resources),
	)
	otel.SetMeterProvider(measuring)
	telemetry.shutdown = append(telemetry.shutdown, measuring.Shutdown)

	telemetry.Tracer = otel.Tracer(config.Service)
	return telemetry, nil
}

// Shutdown flushes everything that has not been sent yet. Call it before the process exits.
func (t *Telemetry) Shutdown(ctx context.Context) error {
	var problems error
	for _, stop := range t.shutdown {
		problems = errors.Join(problems, stop(ctx))
	}
	return problems
}
