//! The platform's telemetry stack, as this service exports to it. Traces and metrics go to
//! `{{ telemetry.endpoint }}` over `{{ telemetry.protocol }}`, and the logs are structured on
//! stdout with the trace they belong to. Nothing here names the collector: it is read from
//! `OTEL_EXPORTER_OTLP_ENDPOINT`, so pointing this elsewhere is a variable, not a change.

use anyhow::{Context, Result};
use opentelemetry::KeyValue;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{MetricExporter, SpanExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, fmt};

use super::Config;

/// Everything that has to be flushed before the process exits.
pub struct Telemetry {
    traces: SdkTracerProvider,
    metrics: SdkMeterProvider,
}

impl Telemetry {
    /// Sets up tracing, metrics and logging as one stack.
    pub fn start(config: &Config) -> Result<Self> {
        let resource = Resource::builder()
            .with_service_name(config.service.clone())
            .with_attributes([
                KeyValue::new("deployment.environment.name", config.environment.clone()),
                KeyValue::new("service.namespace", "{{ telemetry.namespace }}"),
            ])
            .build();

        let spans = SpanExporter::builder()
            .with_tonic()
            .build()
            .context("connecting to the trace collector")?;
        let traces = SdkTracerProvider::builder()
            .with_batch_exporter(spans)
            .with_resource(resource.clone())
            .build();

        let measures = MetricExporter::builder()
            .with_tonic()
            .build()
            .context("connecting to the metric collector")?;
        let metrics = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(measures).build())
            .with_resource(resource)
            .build();
        opentelemetry::global::set_meter_provider(metrics.clone());

        let tracer = traces.tracer(config.service.clone());
        tracing_subscriber::registry()
            .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
            .with(fmt::layer().json().with_current_span(true))
            .with(tracing_opentelemetry::layer().with_tracer(tracer))
            .init();

        Ok(Self { traces, metrics })
    }

    /// Flushes everything that has not been sent yet.
    pub fn shutdown(&self) {
        if let Err(err) = self.traces.shutdown() {
            eprintln!("the traces could not be flushed: {err}");
        }
        if let Err(err) = self.metrics.shutdown() {
            eprintln!("the metrics could not be flushed: {err}");
        }
    }
}
