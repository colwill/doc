//! Traces, metrics and logs for a plugin process, exported over OTLP as `doc-plugin-<id>` when
//! `OTEL_EXPORTER_OTLP_ENDPOINT` is set, and structured logs either way.

use std::sync::OnceLock;

use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;

pub use doc_telemetry::Telemetry;

/// Safe to call more than once: a second call leaves the first subscriber in place.
pub fn init(id: &str, version: &str) -> Telemetry {
    let service = format!("doc-plugin-{id}");
    let telemetry = doc_telemetry::init(&service, version, "info,quiche=warn,tokio_quiche=warn");
    tracing::info!(plugin = id, version, "telemetry ready");
    telemetry
}

/// Counts one call to an outside service (`github`, `confluence`, `google`, `aws`, `slack`...),
/// as `doc.external.calls`, so failing ones show on the external APIs dashboard.
pub fn external(api: &str, operation: &str, status: Option<u16>, ok: bool) {
    static CALLS: OnceLock<Counter<u64>> = OnceLock::new();
    let calls = CALLS.get_or_init(|| {
        doc_telemetry::meter()
            .u64_counter("doc.external.calls")
            .with_description("Calls to outside services, by API, operation and outcome")
            .build()
    });
    let status = status.map_or_else(|| "none".to_string(), |status| status.to_string());
    let outcome = if ok { "ok" } else { "error" };
    let labels = [
        KeyValue::new("api", api.to_string()),
        KeyValue::new("operation", operation.to_string()),
        KeyValue::new("status", status),
        KeyValue::new("outcome", outcome),
    ];
    calls.add(1, &labels);
}

/// Counts a call to an outside service from what sending it gave back.
#[cfg(feature = "reqwest")]
pub fn sent(api: &str, operation: &str, sent: &Result<reqwest::Response, reqwest::Error>) {
    match sent {
        Ok(answer) => {
            let status = answer.status();
            external(
                api,
                operation,
                Some(status.as_u16()),
                !status.is_client_error() && !status.is_server_error(),
            )
        }
        Err(err) => external(api, operation, err.status().map(|status| status.as_u16()), false),
    }
}
