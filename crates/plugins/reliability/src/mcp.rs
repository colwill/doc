//! Reliability as an MCP server at `api/mcp`, like DORA's: Streamable HTTP with JSON responses and
//! no sessions. Core checks `plugin:reliability:user` or `:service` as a read, since the manifest
//! declares the route one, and the Catalogue is asked as the same caller, so an agent sees exactly
//! what the person or account behind it would.

use doc_plugin_sdk::{Backend, Request, Response};
use serde_json::{Value, json};

use crate::metrics::{self, Judged, Period};
use crate::scope::{self, Scope};
use crate::ui::{self, MAX_DAYS};
use crate::{api, faux, view};

const VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const MAX_OUTAGES: usize = 50;

const INSTRUCTIONS: &str = "DOC's reliability: each service's availability against its objective \
(SLA), its mean time to recover (MTTR), and its recovery time and recovery point objectives (RTO, \
RPO), each met, at risk, missed or not measured; and the same for DOC itself. Ask reliability for a \
service, team or organisation, for every service, or with doc true for DOC; and outages for what \
went down, when and for how long.";

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
        "serverInfo": { "name": "doc-reliability", "title": "DOC reliability", "version": env!("CARGO_PKG_VERSION") },
        "instructions": INSTRUCTIONS,
    })
}

fn scoped_input() -> Value {
    json!({
        "type": "object",
        "properties": {
            "service": { "type": "string", "description": "A service's name in the Catalogue, such as card-gateway." },
            "team": { "type": "string", "description": "A team's name, for all of its services." },
            "organisation": { "type": "string", "description": "An organisation's name, for all of its services." },
            "doc": { "type": "boolean", "description": "DOC itself, the platform, rather than a service." },
            "days": { "type": "integer", "minimum": 1, "maximum": MAX_DAYS, "description": "How many days back from now; 30 unless given." },
        },
    })
}

fn tools() -> Value {
    let read_only = json!({ "readOnlyHint": true, "idempotentHint": true, "openWorldHint": false });
    json!([
        {
            "name": "reliability",
            "title": "Reliability",
            "description": "For a service, or each service of a team, an organisation or everything, or for DOC itself: availability against its objective, mean time to recover, the slowest recovery against its recovery time objective, and the most data at risk between backups against its recovery point objective, each met, at risk, missed or not measured.",
            "inputSchema": scoped_input(),
            "annotations": read_only,
        },
        {
            "name": "outages",
            "title": "Outages",
            "description": "The outages of a service, of each service of a team, an organisation or everything, or of DOC: still going first, then the newest, with what noticed each and how long it lasted.",
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

/// One subject in a line, each figure with its verdict.
fn said(judged: &Judged) -> String {
    let verdict = |verdict: metrics::Verdict| verdict.word().to_ascii_lowercase();
    let targets = judged.targets;
    // A recovery point is only for what keeps data: a service, DOC's database, or DOC as a whole.
    let point = match metrics::keeps_data(&judged.subject) || judged.subject == "doc" {
        true => format!(
            " most data at risk {} against an RPO of {} ({});",
            judged.exposure.map_or_else(|| "unknown, no backups".into(), metrics::duration),
            metrics::duration(targets.rpo),
            verdict(judged.rpo),
        ),
        false => String::new(),
    };
    let now = match judged.open {
        0 => "none down now".to_string(),
        open => format!("{} still going", metrics::plural(open, "outage")),
    };
    format!(
        "{}: availability {} against {} ({}); mean time to recover {} against {} ({}); slowest recovery {} against an RTO of {} ({});{point} {now}.",
        judged.title,
        judged.availability.map_or_else(|| "not watched".into(), metrics::percent),
        metrics::percent(targets.sla),
        verdict(judged.sla),
        judged.mttr.map_or_else(|| "no outages".into(), metrics::duration),
        metrics::duration(targets.mttr),
        verdict(judged.recovery),
        judged.worst_recovery().map_or_else(|| "not tested".into(), metrics::duration),
        metrics::duration(targets.rto),
        verdict(judged.rto),
    )
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
    let doc = arguments["doc"].as_bool().unwrap_or(false);
    if doc {
        query.push(("doc", "1".to_string()));
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
    match params["name"].as_str().unwrap_or_default() {
        "reliability" if doc => {
            let doc = match view::doc(backend, &period).await {
                Ok(doc) => doc,
                Err(refusal) => return Ok(failed(&refusal.detail)),
            };
            let mut lines =
                vec![format!("DOC over the last {}: {}", period.said(), said(&doc.whole))];
            lines.extend(
                doc.parts
                    .iter()
                    .chain(&doc.plugins)
                    .filter(|j| j.outages > 0 || j.open > 0)
                    .map(said),
            );
            let structured = json!({
                "from": period.from,
                "to": period.to,
                "doc": doc.whole.json(),
                "parts": doc.parts.iter().map(Judged::json).collect::<Vec<_>>(),
                "plugins": doc.plugins.iter().map(Judged::json).collect::<Vec<_>>(),
            });
            Ok(found(&lines.join("\n"), &structured))
        }
        "reliability" => {
            let services = match view::services(backend, &scope, &period).await {
                Ok(services) => services,
                Err(refusal) => return Ok(failed(&refusal.detail)),
            };
            let watched: Vec<&Judged> = services
                .judged
                .iter()
                .filter(|j| services.held.subjects.contains_key(&j.subject))
                .collect();
            let mut lines = vec![format!(
                "{} over the last {}: {} watched of {}.",
                scope.label(),
                period.said(),
                watched.len(),
                metrics::plural(services.judged.len(), "service")
            )];
            lines.extend(watched.iter().map(|j| said(j)));
            let structured = json!({
                "scope": { "kind": scope.kind(), "name": scope.name() },
                "from": period.from,
                "to": period.to,
                "services": watched.iter().map(|j| j.json()).collect::<Vec<_>>(),
            });
            Ok(found(&lines.join("\n"), &structured))
        }
        "outages" => {
            let mut outages = match api::listed_outages(backend, &query, &period).await {
                Ok(outages) => outages,
                Err(refusal) => return Ok(failed(&refusal.detail)),
            };
            outages.truncate(MAX_OUTAGES);
            let now = chrono::Utc::now();
            let lines: Vec<String> = outages
                .iter()
                .map(|outage| {
                    let lasted = match outage.ended_at {
                        Some(_) => format!("lasted {}", metrics::duration(outage.lasted(now))),
                        None => {
                            format!("still down after {}", metrics::duration(outage.lasted(now)))
                        }
                    };
                    format!(
                        "{} {}: {lasted}, from {}{}",
                        outage.started_at.format("%Y-%m-%d %H:%M"),
                        view::titled(&outage.subject),
                        outage.source,
                        outage
                            .detail
                            .as_deref()
                            .map(|detail| format!(" ({detail})"))
                            .unwrap_or_default(),
                    )
                })
                .collect();
            let text = match lines.is_empty() {
                true => format!("No outages in the last {}.", period.said()),
                false => lines.join("\n"),
            };
            Ok(found(&text, &json!({ "outages": outages })))
        }
        other => Err((-32602, format!("there is no tool {other}"))),
    }
}
