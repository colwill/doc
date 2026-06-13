//! Traces, metrics and logs for every DOC process, exported over OTLP when
//! `OTEL_EXPORTER_OTLP_ENDPOINT` is set, and trace context carried from one process to the next as
//! a W3C `traceparent`, whether over HTTP, QUIC, the Service Bus or a background task.

use std::collections::HashMap;

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::{Histogram, Meter};
use opentelemetry::propagation::{Extractor, Injector};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{LogExporter, MetricExporter, SpanExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

/// The header, and the Service Bus and task field, that carries a trace from one process to the next.
pub const TRACEPARENT: &str = "traceparent";

/// Targets that would otherwise trace the exporter exporting itself.
const QUIET: &str =
    ",opentelemetry=warn,opentelemetry_sdk=warn,reqwest=warn,hyper=warn,hyper_util=warn,h2=warn";

/// Keeps the exporters going; dropping it flushes what they still hold.
pub struct Telemetry {
    providers: Option<(SdkTracerProvider, SdkMeterProvider, SdkLoggerProvider)>,
}

impl Telemetry {
    pub fn exporting(&self) -> bool {
        self.providers.is_some()
    }
}

impl Drop for Telemetry {
    fn drop(&mut self) {
        if let Some((tracer, meter, logger)) = self.providers.take() {
            let _ = tracer.shutdown();
            let _ = meter.shutdown();
            let _ = logger.shutdown();
        }
    }
}

/// JSON logs filtered by `RUST_LOG`, or `filter` without it, and OTLP export when configured.
pub fn init(service: &str, version: &str, filter: &str) -> Telemetry {
    let filter = || {
        EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new(filter))
            .add_directive("opentelemetry=warn".parse().expect("a fixed directive parses"))
    };
    let logs = tracing_subscriber::fmt::layer().json().with_current_span(true);
    let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok().filter(|url| !url.is_empty());
    let Some(endpoint) = endpoint else {
        let _ = tracing_subscriber::registry().with(filter()).with(logs).try_init();
        return Telemetry { providers: None };
    };
    match providers(service, version, &endpoint) {
        Ok((tracer, meter, logger)) => {
            global::set_text_map_propagator(TraceContextPropagator::new());
            global::set_tracer_provider(tracer.clone());
            global::set_meter_provider(meter.clone());
            let traces =
                tracing_opentelemetry::layer().with_tracer(tracer.tracer(service.to_string()));
            let bridged = OpenTelemetryTracingBridge::new(&logger)
                .with_filter(EnvFilter::new(format!("info{QUIET}")));
            let _ = tracing_subscriber::registry()
                .with(filter())
                .with(logs)
                .with(traces)
                .with(bridged)
                .try_init();
            tracing::info!(%endpoint, service, version, "exporting traces, metrics and logs over OTLP");
            Telemetry { providers: Some((tracer, meter, logger)) }
        }
        Err(err) => {
            let _ = tracing_subscriber::registry().with(filter()).with(logs).try_init();
            tracing::warn!(%err, %endpoint, "telemetry cannot be exported; logging only");
            Telemetry { providers: None }
        }
    }
}

type Providers = (SdkTracerProvider, SdkMeterProvider, SdkLoggerProvider);

fn providers(service: &str, version: &str, endpoint: &str) -> Result<Providers, String> {
    // reqwest is built without a TLS provider of its own, so its exporter client needs one first.
    let _ = rustls::crypto::ring::default_provider().install_default();
    // Tells apart processes of one service, such as a bus's three nodes, which record the same series.
    let host = std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|host| host.trim().to_string())
        .filter(|host| !host.is_empty())
        .unwrap_or_else(|| "unknown".into());
    let instance = format!("{host}:{}", std::process::id());
    let resource = Resource::builder()
        .with_service_name(service.to_string())
        .with_attributes([
            KeyValue::new("service.version", version.to_string()),
            KeyValue::new("service.instance.id", instance),
        ])
        .build();
    let base = endpoint.trim_end_matches('/');
    let spans = SpanExporter::builder()
        .with_http()
        .with_endpoint(format!("{base}/v1/traces"))
        .build()
        .map_err(|err| err.to_string())?;
    let metrics = MetricExporter::builder()
        .with_http()
        .with_endpoint(format!("{base}/v1/metrics"))
        .build()
        .map_err(|err| err.to_string())?;
    let logs = LogExporter::builder()
        .with_http()
        .with_endpoint(format!("{base}/v1/logs"))
        .build()
        .map_err(|err| err.to_string())?;
    let tracer = SdkTracerProvider::builder()
        .with_batch_exporter(spans)
        .with_resource(resource.clone())
        .build();
    let meter = SdkMeterProvider::builder()
        .with_periodic_exporter(metrics)
        .with_resource(resource.clone())
        .build();
    let logger =
        SdkLoggerProvider::builder().with_batch_exporter(logs).with_resource(resource).build();
    Ok((tracer, meter, logger))
}

struct Carrier(HashMap<String, String>);

impl Injector for Carrier {
    fn set(&mut self, key: &str, value: String) {
        self.0.insert(key.to_string(), value);
    }
}

impl Extractor for Carrier {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(String::as_str).collect()
    }
}

/// The current span's context as a `traceparent` value, when it is being traced.
pub fn traceparent() -> Option<String> {
    let context = tracing::Span::current().context();
    let mut carrier = Carrier(HashMap::new());
    global::get_text_map_propagator(|propagator| propagator.inject_context(&context, &mut carrier));
    carrier.0.remove(TRACEPARENT)
}

/// Whether the current span is traced, so work outside any trace does not start one of its own.
pub fn in_trace() -> bool {
    !tracing::Span::current().is_none()
}

/// Makes `span` a child of the trace a `traceparent` names, when there is one.
pub fn adopt(span: &tracing::Span, traceparent: Option<&str>) {
    let Some(traceparent) = traceparent.filter(|value| !value.is_empty()) else { return };
    let carrier = Carrier(HashMap::from([(TRACEPARENT.to_string(), traceparent.to_string())]));
    let context = global::get_text_map_propagator(|propagator| propagator.extract(&carrier));
    let _ = span.set_parent(context);
}

/// The `traceparent` header of a request, if it carries one.
pub fn header(headers: &http::HeaderMap) -> Option<&str> {
    headers.get(TRACEPARENT).and_then(|value| value.to_str().ok())
}

/// The meter every DOC metric is recorded with.
pub fn meter() -> Meter {
    global::meter("doc")
}

/// Bucket edges for durations in seconds; the SDK's defaults suit milliseconds.
const SECONDS: [f64; 16] = [
    0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 300.0,
];

/// A histogram of durations in seconds.
pub fn seconds(name: &'static str, description: &'static str) -> Histogram<f64> {
    meter()
        .f64_histogram(name)
        .with_unit("s")
        .with_description(description)
        .with_boundaries(SECONDS.to_vec())
        .build()
}

/// Middleware giving each request a server span, continuing the caller's trace, and its duration metric.
#[cfg(feature = "axum")]
pub async fn traced(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use tracing::Instrument;
    let method = request.method().to_string();
    let route = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map_or("unmatched", |path| path.as_str())
        .to_string();
    // Probes and the development reload poll these all day; they are counted, not traced.
    let quiet = matches!(route.as_str(), "/healthz" | "/readyz" | "/dev/boot");
    let span = if quiet {
        tracing::Span::none()
    } else {
        tracing::info_span!(
        target: "doc",
        "request",
        otel.name = %format!("{method} {route}"),
        otel.kind = "server",
        otel.status_code = tracing::field::Empty,
        http.request.method = %method,
        http.route = %route,
        http.response.status_code = tracing::field::Empty,
        url.path = %request.uri().path(),
        )
    };
    adopt(&span, header(request.headers()));
    let started = std::time::Instant::now();
    let response = next.run(request).instrument(span.clone()).await;
    let status = response.status();
    span.record("http.response.status_code", status.as_u16());
    if status.is_server_error() {
        span.record("otel.status_code", "ERROR");
    }
    served(&method, &route, status.as_u16(), started.elapsed().as_secs_f64());
    response
}

/// Records one served HTTP request, as `http.server.request.duration`.
pub fn served(method: &str, route: &str, status: u16, took: f64) {
    use std::sync::OnceLock;
    static DURATION: OnceLock<Histogram<f64>> = OnceLock::new();
    let histogram = DURATION.get_or_init(|| {
        seconds("http.server.request.duration", "How long HTTP requests took to answer")
    });
    histogram.record(
        took,
        &[
            KeyValue::new("http.request.method", method.to_string()),
            KeyValue::new("http.route", route.to_string()),
            KeyValue::new("http.response.status_code", i64::from(status)),
        ],
    );
}
