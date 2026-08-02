//! DORA metrics as an MCP server at `api/mcp`, like the Knowledge Base's: Streamable HTTP with
//! JSON responses and no sessions. Core checks `plugin:dora:user` or `:service` as a read, since
//! the manifest declares the route one, and the Catalogue is asked as the same caller, so an agent
//! sees exactly what the person or account behind it would.

use doc_plugin_sdk::{Backend, Request, Response};
use serde_json::{Value, json};

use crate::faux;
use crate::metrics::{self, Band, Period};
use crate::scope::{self, Scope};
use crate::settings::Definitions;
use crate::ui::{self, MAX_DAYS};

const VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const MAX_FAILURES: usize = 50;

const INSTRUCTIONS: &str = "DOC's DORA metrics: how often each service deploys to production, how \
long a change takes to get there, how often a deployment causes a failure, and how long recovering \
takes. Ask delivery_metrics for a service, team or organisation (or for everything), and \
failed_deployments for what caused each failure. Metrics are for services and teams, never people.";

fn rpc_error(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn json_response(status: u16, value: &Value) -> Response {
    Response::new(status, "application/json", serde_json::to_vec(value).unwrap_or_default())
}

pub async fn handle(backend: &Backend, request: &Request) -> Response {
    if request.method != "POST" {
        return Response::new(
            405,
            "application/json",
            b"{\"detail\":\"MCP is spoken over POST here; this server opens no event streams\"}"
                .to_vec(),
        )
        .with_header("allow", "POST");
    }
    if let Some(version) = request.headers.get("mcp-protocol-version")
        && !VERSIONS.contains(&version.as_str())
    {
        let message = format!("MCP protocol version {version} is not supported");
        return json_response(400, &rpc_error(&Value::Null, -32600, &message));
    }
    let Ok(message) = serde_json::from_slice::<Value>(&request.body) else {
        return json_response(400, &rpc_error(&Value::Null, -32700, "the body is not JSON"));
    };
    match message {
        Value::Array(messages) => {
            let mut answers = Vec::new();
            for message in &messages {
                if let Some(answer) = answer(backend, message).await {
                    answers.push(answer);
                }
            }
            match answers.is_empty() {
                true => Response::new(202, "application/json", Vec::new()),
                false => json_response(200, &Value::Array(answers)),
            }
        }
        message => match answer(backend, &message).await {
            Some(answer) => json_response(200, &answer),
            None if message.get("method").is_some() => {
                Response::new(202, "application/json", Vec::new())
            }
            None => json_response(400, &rpc_error(&Value::Null, -32600, "not a JSON-RPC message")),
        },
    }
}

async fn answer(backend: &Backend, message: &Value) -> Option<Value> {
    let id = message.get("id").filter(|id| !id.is_null())?;
    let Some(method) = message["method"].as_str() else {
        return Some(rpc_error(id, -32600, "a request names a method"));
    };
    let result = match method {
        "initialize" => Ok(initialize(&message["params"])),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => call(backend, &message["params"]).await,
        _ => Err((-32601, format!("there is no method {method}"))),
    };
    Some(match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => rpc_error(id, code, &message),
    })
}

fn initialize(params: &Value) -> Value {
    let asked = params["protocolVersion"].as_str().unwrap_or_default();
    let version = VERSIONS.iter().find(|version| **version == asked).unwrap_or(&VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "doc-dora", "title": "DOC DORA metrics", "version": env!("CARGO_PKG_VERSION") },
        "instructions": INSTRUCTIONS,
    })
}

fn scoped_input() -> Value {
    json!({
        "type": "object",
        "properties": {
            "service": { "type": "string", "description": "A service's name in the Catalogue, such as card-gateway." },
            "team": { "type": "string", "description": "A team's name, for all of its services and repositories." },
            "organisation": { "type": "string", "description": "An organisation's name, for all of its services." },
            "days": { "type": "integer", "minimum": 1, "maximum": MAX_DAYS, "description": "How many days back from now; 30 unless given." },
        },
    })
}

fn tools() -> Value {
    let read_only = json!({ "readOnlyHint": true, "idempotentHint": true, "openWorldHint": false });
    json!([
        {
            "name": "delivery_metrics",
            "title": "DORA metrics",
            "description": "The four DORA metrics for a service, team or organisation, or for everything when none is named: deployment frequency, change lead time (median and 90th percentile), change fail rate and failed deployment recovery time, each with its performance band, and the same for the period before.",
            "inputSchema": scoped_input(),
            "annotations": read_only,
        },
        {
            "name": "failed_deployments",
            "title": "Failed deployments",
            "description": "The deployments that caused a failure, newest first: when, which repository and commit, what said it failed (a revert, rollback, hotfix or a counted incident) and how long recovery took.",
            "inputSchema": scoped_input(),
            "annotations": read_only,
        },
    ])
}

fn failed(reason: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": reason }], "isError": true })
}

/// What a tool found. Faux data says so first, so an agent never passes it on as measured.
fn found(text: &str, structured: &Value) -> Value {
    let text = format!("{}{text}", faux::prefix());
    let mut structured = structured.clone();
    structured["faux"] = faux::said();
    json!({ "content": [{ "type": "text", "text": text }], "structuredContent": structured, "isError": false })
}

fn banded(value: String, band: Option<Band>) -> String {
    match band {
        Some(band) => format!("{value} ({})", band.word().to_ascii_lowercase()),
        None => value,
    }
}

async fn call(backend: &Backend, params: &Value) -> Result<Value, (i64, String)> {
    let arguments = &params["arguments"];
    let mut query = Vec::new();
    for key in ["service", "team", "organisation"] {
        if let Some(value) =
            arguments[key].as_str().map(str::trim).filter(|value| !value.is_empty())
        {
            query.push((key, value.to_string()));
        }
    }
    let days = arguments["days"].as_i64().unwrap_or(30).to_string();
    query.push(("days", days));
    let pairs: Vec<(&str, &str)> =
        query.iter().map(|(key, value)| (*key, value.as_str())).collect();
    let query = scope::encoded(&pairs);
    let (scope, days) = match (Scope::from_query(&query), ui::days(&query)) {
        (Ok(scope), Ok(days)) => (scope, days),
        (Err(refusal), _) | (_, Err(refusal)) => return Ok(failed(&refusal.detail)),
    };
    let period = Period::last(days);
    let repositories = match scope::repositories(backend, &scope).await {
        Ok(repositories) => repositories,
        Err(refusal) => return Ok(failed(&refusal.detail)),
    };
    match params["name"].as_str().unwrap_or_default() {
        "delivery_metrics" => {
            let definitions = Definitions::read(&backend.settings());
            let (now, before) = match (
                metrics::deployments(backend, repositories.as_ref(), &period).await,
                metrics::deployments(backend, repositories.as_ref(), &period.before()).await,
            ) {
                (Ok(now), Ok(before)) => (now, before),
                (Err(refusal), _) | (_, Err(refusal)) => return Ok(failed(&refusal.detail)),
            };
            let figures = metrics::figures(&now, &period);
            let bands = figures.bands(&definitions.bands);
            let lead = figures.lead_time.map_or_else(
                || "no changes with known commits".to_string(),
                |lead| {
                    format!(
                        "median {}, 90th percentile {}",
                        metrics::duration(lead.median),
                        metrics::duration(lead.p90)
                    )
                },
            );
            let recovery = match (figures.recovery, figures.failed) {
                (Some(recovery), _) => format!("median {}", metrics::duration(recovery.median)),
                (None, 0) => "no failures".to_string(),
                (None, _) => "none recovered yet".to_string(),
            };
            let text = format!(
                "{} over the last {}: {} deployments, {}; change lead time {}; change fail rate {}; \
                 failed deployment recovery time {}, {} not yet recovered.",
                scope.label(),
                period.said(),
                figures.deployments,
                banded(metrics::frequency(&figures), bands.deployment_frequency),
                banded(lead, bands.lead_time),
                banded(
                    figures.change_fail_rate.map_or_else(|| "none".into(), metrics::percent),
                    bands.change_fail_rate
                ),
                banded(recovery, bands.recovery),
                figures.unrecovered,
            );
            let structured = json!({
                "scope": { "kind": scope.kind(), "name": scope.name() },
                "from": period.from,
                "to": period.to,
                "metrics": figures.json(&definitions.bands),
                "previous": metrics::figures(&before, &period.before()).json(&definitions.bands),
            });
            Ok(found(&text, &structured))
        }
        "failed_deployments" => {
            let listed =
                match ui::newest(backend, repositories.as_ref(), &period, true, MAX_FAILURES).await
                {
                    Ok(listed) => listed,
                    Err(refusal) => return Ok(failed(&refusal.detail)),
                };
            let lines: Vec<String> = listed
                .iter()
                .map(|deployment| {
                    let recovered = deployment.recovery_seconds.map_or_else(
                        || "not recovered yet".to_string(),
                        |seconds| format!("recovered in {}", metrics::duration(seconds)),
                    );
                    format!(
                        "{} {} {}: {}, {recovered}",
                        deployment.deployed_at.format("%Y-%m-%d %H:%M"),
                        deployment.repository,
                        deployment.sha.chars().take(7).collect::<String>(),
                        deployment.failure.as_deref().unwrap_or("failed"),
                    )
                })
                .collect();
            let text = match lines.is_empty() {
                true => format!(
                    "No deployment of {} caused a failure in the last {}.",
                    scope.label(),
                    period.said()
                ),
                false => lines.join("\n"),
            };
            Ok(found(&text, &json!({ "deployments": listed })))
        }
        other => Err((-32602, format!("there is no tool {other}"))),
    }
}
