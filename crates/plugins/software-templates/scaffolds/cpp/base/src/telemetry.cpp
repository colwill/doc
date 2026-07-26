#include "{{ scaffold.package }}/telemetry.hpp"

#include <memory>

#include "opentelemetry/exporters/otlp/otlp_grpc_exporter_factory.h"
#include "opentelemetry/exporters/otlp/otlp_grpc_metric_exporter_factory.h"
#include "opentelemetry/metrics/provider.h"
#include "opentelemetry/sdk/metrics/meter_provider_factory.h"
#include "opentelemetry/sdk/metrics/export/periodic_exporting_metric_reader_factory.h"
#include "opentelemetry/sdk/resource/resource.h"
#include "opentelemetry/sdk/trace/batch_span_processor_factory.h"
#include "opentelemetry/sdk/trace/tracer_provider_factory.h"
#include "opentelemetry/trace/provider.h"

namespace {{ scaffold.package }} {
namespace otlp = opentelemetry::exporter::otlp;
namespace sdktrace = opentelemetry::sdk::trace;
namespace sdkmetrics = opentelemetry::sdk::metrics;

Telemetry::Telemetry(const Config& config) : service_(config.service) {
  opentelemetry::sdk::resource::ResourceAttributes attributes;
  attributes.SetAttribute("service.name", config.service);
  attributes.SetAttribute("service.namespace", std::string("{{ telemetry.namespace }}"));
  attributes.SetAttribute("deployment.environment.name", config.environment);
  auto resource = opentelemetry::sdk::resource::Resource::Create(attributes);

  // The endpoint comes from OTEL_EXPORTER_OTLP_ENDPOINT, which the container already has.
  auto spans = otlp::OtlpGrpcExporterFactory::Create();
  auto processor = sdktrace::BatchSpanProcessorFactory::Create(std::move(spans), {});
  std::shared_ptr<opentelemetry::trace::TracerProvider> traces =
      sdktrace::TracerProviderFactory::Create(std::move(processor), resource);
  opentelemetry::trace::Provider::SetTracerProvider(traces);

  auto measures = otlp::OtlpGrpcMetricExporterFactory::Create();
  sdkmetrics::PeriodicExportingMetricReaderOptions reading;
  auto reader = sdkmetrics::PeriodicExportingMetricReaderFactory::Create(std::move(measures), reading);
  auto metrics = sdkmetrics::MeterProviderFactory::Create(
      std::make_unique<sdkmetrics::ViewRegistry>(), resource);
  metrics->AddMetricReader(std::move(reader));
  std::shared_ptr<opentelemetry::metrics::MeterProvider> measuring(std::move(metrics));
  opentelemetry::metrics::Provider::SetMeterProvider(measuring);
}

Telemetry::~Telemetry() {
  // Flush whatever has not been sent yet.
  std::shared_ptr<opentelemetry::trace::TracerProvider> traces;
  opentelemetry::trace::Provider::SetTracerProvider(traces);
  std::shared_ptr<opentelemetry::metrics::MeterProvider> metrics;
  opentelemetry::metrics::Provider::SetMeterProvider(metrics);
}

opentelemetry::nostd::shared_ptr<opentelemetry::trace::Tracer> Telemetry::tracer() const {
  return opentelemetry::trace::Provider::GetTracerProvider()->GetTracer(service_);
}

opentelemetry::nostd::shared_ptr<opentelemetry::metrics::Meter> Telemetry::meter() const {
  return opentelemetry::metrics::Provider::GetMeterProvider()->GetMeter(service_);
}

}  // namespace {{ scaffold.package }}
