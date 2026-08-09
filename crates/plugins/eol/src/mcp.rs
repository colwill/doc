//! End of life as an MCP server at `api/mcp`, like the other delivery plugins': Streamable HTTP
//! with JSON responses and no sessions. Core checks `plugin:eol:user` or `:service` as a read, and
//! the Catalogue is asked as the same caller, so an agent sees what the person behind it would.

use doc_plugin_sdk::{Backend, Request, Response};
use serde_json::{Value, json};

use crate::faux;
use crate::scope::{self, Scope};
use crate::view::{self, Judged};

const VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "DOC's end-of-life data: the languages, frameworks, databases and \
operating systems each service in the Catalogue runs, and where each release stands — supported, \
security fixes only, ending soon or past its end of life — from endoflife.date. What a service \
runs is what the repositories connected to it were found to be built on — the packages ccc found \
in their lockfiles when Repository Insights scanned them, and their own files where those are read \
— and what its own metadata in the Catalogue names; each answer says which it came from in `from`. Ask end_of_life for a \
service, team or organisation (or for everything), and product_lifecycle for every release of one \
product.";

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
        "serverInfo": { "name": "doc-eol", "title": "DOC end of life", "version": env!("CARGO_PKG_VERSION") },
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
        },
    })
}

fn tools() -> Value {
    let read_only = json!({ "readOnlyHint": true, "idempotentHint": true, "openWorldHint": false });
    json!([
        {
            "name": "end_of_life",
            "title": "End of life",
            "description": "For a service, team or organisation, or for everything when none is named: each product its services run and the release they run, with where that release stands — past its end of life, ending soon, security fixes only, supported or not known — and its dates. Worst first. `from` is empty where the service's own metadata names it, and otherwise names the repository and the lockfile or file it was read from.",
            "inputSchema": scoped_input(),
            "annotations": read_only,
        },
        {
            "name": "product_lifecycle",
            "title": "A product's lifecycle",
            "description": "Every release of one product, such as nodejs, python or postgresql (endoflife.date's names): when each came out, when active support and security fixes end, its latest version, and which services run it.",
            "inputSchema": {
                "type": "object",
                "properties": { "product": { "type": "string", "description": "endoflife.date's name for it, such as nodejs." } },
                "required": ["product"],
            },
            "annotations": read_only,
        },
    ])
}

fn line(judged: &Judged) -> String {
    format!("{}: {} — {}. {}", judged.title, judged.named(), judged.status.word(), judged.why)
}

async fn call(backend: &Backend, params: &Value) -> Result<Value, (i64, String)> {
    let arguments = &params["arguments"];
    match params["name"].as_str().unwrap_or_default() {
        "end_of_life" => {
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
            let view = match view::read(backend, &scope).await {
                Ok(view) => view,
                Err(refusal) => return Ok(failed(&refusal.detail)),
            };
            let text = match view.judged.is_empty() {
                true => format!("Nothing is known of what {} runs.", scope.label()),
                false => view.judged.iter().map(line).collect::<Vec<_>>().join("\n"),
            };
            let structured = json!({
                "scope": { "kind": scope.kind(), "name": scope.name() },
                "runs": view.judged.iter().map(Judged::json).collect::<Vec<_>>(),
            });
            Ok(found(&text, &structured))
        }
        "product_lifecycle" => {
            let Some(product) = arguments["product"].as_str().filter(|p| !p.trim().is_empty())
            else {
                return Ok(failed("name a product, such as nodejs"));
            };
            let (product, used_by) = match crate::api::lifecycle_of(backend, product).await {
                Ok(found) => found,
                Err(refusal) => return Ok(failed(&refusal.detail)),
            };
            let today = crate::lifecycle::today();
            let warn = crate::settings::Definitions::read(&backend.settings()).warn_days;
            let mut lines = vec![format!("{}:", product.label)];
            for release in &product.releases {
                let status = release.status(today, warn);
                let users: Vec<&str> = used_by
                    .iter()
                    .filter(|judged| {
                        judged.release.as_ref().is_some_and(|r| r.name == release.name)
                    })
                    .map(|judged| judged.title.as_str())
                    .collect();
                lines.push(format!(
                    "{} — {}. {}{}",
                    release.title(),
                    status.word(),
                    view::said(release, status, today),
                    match users.is_empty() {
                        true => String::new(),
                        false => format!(". Run by {}", users.join(", ")),
                    }
                ));
            }
            let structured = json!({ "product": product, "used_by": used_by.iter().map(Judged::json).collect::<Vec<_>>() });
            Ok(found(&lines.join("\n"), &structured))
        }
        other => Err((-32602, format!("there is no tool {other}"))),
    }
}
