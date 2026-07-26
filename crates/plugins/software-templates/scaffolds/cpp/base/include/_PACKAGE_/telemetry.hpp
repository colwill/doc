// The platform's telemetry stack, as this service exports to it. Traces and metrics go to
// {{ telemetry.endpoint }} over {{ telemetry.protocol }}. Nothing here names the collector: it is
// read from OTEL_EXPORTER_OTLP_ENDPOINT, so pointing this elsewhere is a variable, not a change.
#pragma once

#include <memory>
#include <string>

#include "opentelemetry/metrics/meter.h"
#include "opentelemetry/trace/tracer.h"

#include "{{ scaffold.package }}/config.hpp"

namespace {{ scaffold.package }} {

class Telemetry {
 public:
  // Sets up tracing and metrics as one stack; the destructor flushes both.
  explicit Telemetry(const Config& config);
  ~Telemetry();

  Telemetry(const Telemetry&) = delete;
  Telemetry& operator=(const Telemetry&) = delete;

  opentelemetry::nostd::shared_ptr<opentelemetry::trace::Tracer> tracer() const;
  opentelemetry::nostd::shared_ptr<opentelemetry::metrics::Meter> meter() const;

 private:
  std::string service_;
};

}  // namespace {{ scaffold.package }}
