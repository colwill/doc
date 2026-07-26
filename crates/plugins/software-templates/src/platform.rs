//! The instance's own stacks, as every template sees them. A platform is configured once — where
//! telemetry goes, where DOC is — and every service scaffolded from then on is wired to it before
//! anybody edits a line. Nothing secret is rendered into a repository: a token is named in the
//! environment and comes from a secret where the service runs.

use doc_plugin_sdk::Backend;
use serde_json::{Value, json};

use crate::model::Lifecycle;

pub const OTLP_ENDPOINT: &str = "otlp-endpoint";
pub const OTLP_PROTOCOL: &str = "otlp-protocol";
pub const OTLP_INSECURE: &str = "otlp-insecure";
pub const OTLP_HEADERS: &str = "otlp-headers";
pub const ENVIRONMENT: &str = "environment";
pub const NAMESPACE: &str = "service-namespace";
pub const DOC_URL: &str = "doc-url";
pub const FLAGS: &str = "flags";

/// Where DOC is, as a service running outside it reaches it.
pub fn doc_url(backend: &Backend) -> String {
    backend
        .settings()
        .some_text(DOC_URL)
        .or_else(|| std::env::var("DOC_PUBLIC_URL").ok())
        .unwrap_or_else(|| "http://127.0.0.1:8080".to_string())
        .trim_end_matches('/')
        .to_string()
}

/// `telemetry`, `flags`, `platform`, `lifecycle` and `env`, as a template renders them. `service`
/// is what the thing being created is called, which the variables are written for, and `lifecycle`
/// is how far along it is.
pub fn context(backend: &Backend, service: &str, lifecycle: Lifecycle) -> Value {
    let settings = backend.settings();
    let endpoint = settings
        .some_text(OTLP_ENDPOINT)
        .unwrap_or_else(|| "http://otel-collector:4317".to_string())
        .trim_end_matches('/')
        .to_string();
    let protocol = match settings.some_text(OTLP_PROTOCOL).as_deref() {
        Some("http") => "http/protobuf",
        _ => "grpc",
    };
    let environment = settings.some_text(ENVIRONMENT).unwrap_or_else(|| "production".to_string());
    let doc = doc_url(backend);
    let mut context = json!({
        "telemetry": {
            "endpoint": endpoint,
            "protocol": protocol,
            "insecure": settings.boolean(OTLP_INSECURE),
            "headers": settings.some_text(OTLP_HEADERS).unwrap_or_default(),
            "environment": environment,
            "namespace": settings.some_text(NAMESPACE).unwrap_or_default(),
        },
        "flags": {
            "enabled": backend.feature(FLAGS),
            "url": format!("{doc}/api/v1/plugins/flags/api/evaluate"),
            "ofrep_url": format!("{doc}/api/v1/plugins/flags/api/ofrep/v1"),
            "environment": environment,
        },
        "platform": { "url": doc },
        "lifecycle": lifecycle.context(),
    });
    let variables: Vec<Value> = variables(&context, service)
        .into_iter()
        .map(|(name, value)| json!({ "name": name, "value": value }))
        .collect();
    context["env"] = Value::Array(variables);
    context
}

/// Everything a scaffolded service reads from its environment, which is one list so that the
/// `.env.example`, the container and the Kubernetes manifests cannot drift apart.
pub fn variables(context: &Value, service: &str) -> Vec<(String, String)> {
    let text =
        |path: &str| crate::render::lookup(context, path).as_str().unwrap_or_default().to_string();
    let environment = text("telemetry.environment");
    // The lifecycle rides along on the resource attributes, so every trace, metric and log says
    // whether what wrote it may be depended on, without a service having to read it itself.
    let lifecycle = text("lifecycle.name");
    let mut variables = vec![
        ("OTEL_SERVICE_NAME".to_string(), service.to_string()),
        ("OTEL_EXPORTER_OTLP_ENDPOINT".to_string(), text("telemetry.endpoint")),
        ("OTEL_EXPORTER_OTLP_PROTOCOL".to_string(), text("telemetry.protocol")),
        (
            "OTEL_RESOURCE_ATTRIBUTES".to_string(),
            format!(
                "service.name={service},deployment.environment={environment},doc.lifecycle={lifecycle}"
            ),
        ),
        ("DOC_ENVIRONMENT".to_string(), environment),
        ("DOC_FLAGS_URL".to_string(), text("flags.url")),
        ("DOC_FLAGS_SERVICE".to_string(), service.to_string()),
    ];
    let headers = text("telemetry.headers");
    if !headers.is_empty() {
        variables.push(("OTEL_EXPORTER_OTLP_HEADERS".to_string(), headers));
    }
    variables
}
