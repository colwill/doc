//! The delivery roadmap as an MCP server at `api/mcp`, like the other delivery plugins':
//! Streamable HTTP with JSON responses and no sessions. Core checks `plugin:roadmap:user` or
//! `:service` as a read, and the Catalogue and every plugin asked about readiness are asked as the
//! same caller, so an agent sees what the person behind it would.

use doc_plugin_sdk::{Backend, Request, Response};
use serde_json::{Value, json};

use crate::faux;
use crate::plan::Release;
use crate::readiness::{self, Column};
use crate::scope::{self, Scope};
use crate::view;

const VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "DOC's delivery roadmap: every release planned in Jira for each \
service, team and organisation, when it is due, how far along it is, and whether the services in \
it are ready — their pipelines, reliability, end of life and deployments, from the plugins that \
measure them. Ask roadmap for a service, team or organisation (or everything), and release for one \
release's issues and readiness.";

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
        "serverInfo": { "name": "doc-roadmap", "title": "DOC delivery roadmap", "version": env!("CARGO_PKG_VERSION") },
        "instructions": INSTRUCTIONS,
    })
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

fn scoped_input() -> Value {
    json!({
        "type": "object",
        "properties": {
            "service": { "type": "string", "description": "A service's name in the Catalogue, such as card-gateway." },
            "team": { "type": "string", "description": "A team's name, for all of its services." },
            "organisation": { "type": "string", "description": "An organisation's name, for all of its services." },
            "released": { "type": "boolean", "description": "true for what shipped lately rather than what is coming up." },
        },
    })
}

fn tools() -> Value {
    let read_only = json!({ "readOnlyHint": true, "idempotentHint": true, "openWorldHint": false });
    json!([
        {
            "name": "roadmap",
            "title": "Delivery roadmap",
            "description": "For a service, team or organisation, or everything when none is named: each release coming up (or, with released, shipped lately), its due date and progress, whether it is on track, at risk, overdue, planned or not scheduled, and what each plugin that measures services says of the services in it: ready, warning or blocked.",
            "inputSchema": scoped_input(),
            "annotations": read_only,
        },
        {
            "name": "release",
            "title": "One release",
            "description": "One release by its id (source:id, as roadmap gives it) or its title (such as PAY 2.4): its dates, progress, issues, and each plugin's word on each of its services.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Its id, such as jira:10042." },
                    "title": { "type": "string", "description": "Its project key and name, such as PAY 2.4." },
                },
            },
            "annotations": read_only,
        },
    ])
}

fn readiness_said(columns: &[Column]) -> String {
    let said: Vec<String> = columns
        .iter()
        .map(|column| {
            // A plugin's faux data says so, so an agent never passes it on as measured.
            let title = match column.faux {
                true => format!("{} (FAUX DATA)", column.title),
                false => column.title.clone(),
            };
            match column.state {
                readiness::State::Ready | readiness::State::Unknown => {
                    format!("{title}: {}", column.state.word().to_ascii_lowercase())
                }
                _ => format!(
                    "{title}: {} — {}",
                    column.state.word().to_ascii_lowercase(),
                    column.said()
                ),
            }
        })
        .collect();
    match said.is_empty() {
        true => String::new(),
        false => format!(" Readiness: {}.", said.join("; ")),
    }
}

fn line(release: &Release, columns: &[Column]) -> String {
    let services: Vec<&str> = release.services.iter().map(|(name, _)| name.as_str()).collect();
    format!(
        "{} ({}) — {}: {}. Services: {}.{}",
        release.title(),
        release.key,
        release.status.word(),
        release.why,
        if services.is_empty() { "none named".to_string() } else { services.join(", ") },
        readiness_said(columns),
    )
}

async fn call(backend: &Backend, params: &Value) -> Result<Value, (i64, String)> {
    let arguments = &params["arguments"];
    match params["name"].as_str().unwrap_or_default() {
        "roadmap" => {
            let mut pairs = Vec::new();
            for key in ["service", "team", "organisation"] {
                if let Some(value) =
                    arguments[key].as_str().map(str::trim).filter(|v| !v.is_empty())
                {
                    pairs.push((key, value.to_string()));
                }
            }
            let pairs: Vec<(&str, &str)> = pairs.iter().map(|(k, v)| (*k, v.as_str())).collect();
            let scope = match Scope::from_query(&scope::encoded(&pairs)) {
                Ok(scope) => scope,
                Err(refusal) => return Ok(failed(&refusal.detail)),
            };
            let shipped = arguments["released"].as_bool().unwrap_or(false);
            let view = match view::read(backend, &scope, !shipped).await {
                Ok(view) => view,
                Err(refusal) => return Ok(failed(&refusal.detail)),
            };
            let chosen: Vec<&Release> = view
                .releases
                .iter()
                .filter(|release| release.version.released == shipped)
                .collect();
            let lines: Vec<String> =
                chosen.iter().map(|release| line(release, &view.columns(release))).collect();
            let text = match lines.is_empty() {
                true => format!(
                    "No release {} for {}.",
                    if shipped { "shipped lately" } else { "is coming up" },
                    scope.label()
                ),
                false => lines.join("\n"),
            };
            let structured: Vec<Value> = chosen
                .iter()
                .map(|release| {
                    let columns = view.columns(release);
                    let mut value = release.json(false);
                    value["readiness"] =
                        json!({ "worst": readiness::worst(&columns), "plugins": columns });
                    value
                })
                .collect();
            Ok(found(&text, &json!({ "releases": structured })))
        }
        "release" => {
            let id = match (arguments["id"].as_str(), arguments["title"].as_str()) {
                (Some(id), _) if !id.trim().is_empty() => id.trim().to_string(),
                (_, Some(title)) if !title.trim().is_empty() => {
                    let view = match view::read(backend, &Scope::All, false).await {
                        Ok(view) => view,
                        Err(refusal) => return Ok(failed(&refusal.detail)),
                    };
                    let wanted = title.trim().to_ascii_lowercase();
                    match view
                        .releases
                        .iter()
                        .find(|release| release.title().to_ascii_lowercase() == wanted)
                    {
                        Some(release) => release.key.clone(),
                        None => return Ok(failed(&format!("there is no release called {title}"))),
                    }
                }
                _ => return Ok(failed("name a release by its id or its title")),
            };
            let (release, columns) = match view::one(backend, &id).await {
                Ok(found) => found,
                Err(refusal) => return Ok(failed(&refusal.detail)),
            };
            let progress = release.progress;
            let text = format!(
                "{} {} to do, {} in progress, {} done.",
                line(&release, &columns),
                progress.to_do,
                progress.in_progress,
                progress.done
            );
            let mut structured = release.json(true);
            structured["readiness"] =
                json!({ "worst": readiness::worst(&columns), "plugins": columns });
            Ok(found(&text, &structured))
        }
        other => Err((-32602, format!("there is no tool {other}"))),
    }
}
