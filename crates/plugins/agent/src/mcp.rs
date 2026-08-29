//! One MCP server for the whole of DOC at `api/mcp`: Streamable HTTP with JSON answers and no
//! sessions, as the plugins' own servers speak it. Its tools are DOC's and every plugin's, made as
//! the person connecting, and its prompts are the playbooks.

use doc_plugin_sdk::{Backend, Request, Response};
use serde_json::{Value, json};

use crate::{playbooks, tools};

const VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "DOC, the internal developer platform, as one server. Every tool acts \
as the person whose token connected, with their access and nothing more. doc_* tools are DOC's \
own: the Catalogue, readiness, DOC's health, discussions, notifications, reminders, adding people \
and any plugin's API. The rest are each plugin's own, named after it: dora_* for DORA metrics, \
cicd_* for pipelines, kb_* for documentation and so on. The prompts are playbooks for onboarding \
an existing platform, finding a root cause, reporting on health and setting reminders.";

fn rpc_error(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn json_response(status: u16, value: &Value) -> Response {
    Response::new(status, "application/json", serde_json::to_vec(value).unwrap_or_default())
}

pub async fn handle(backend: &Backend, request: &Request) -> Response {
    if request.method != "POST" {
        let said =
            b"{\"detail\":\"MCP is spoken over POST here; this server opens no event streams\"}";
        return Response::new(405, "application/json", said.to_vec()).with_header("allow", "POST");
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
    let params = &message["params"];
    let result = match method {
        "initialize" => Ok(initialize(params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools::available(backend, false).await })),
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or_default();
            let arguments = match &params["arguments"] {
                Value::Null => json!({}),
                given => given.clone(),
            };
            let outcome = tools::call(backend, name, &arguments).await;
            Ok(
                json!({ "content": [{ "type": "text", "text": outcome.text }], "isError": outcome.failed }),
            )
        }
        "prompts/list" => Ok(
            json!({ "prompts": playbooks::ALL.iter().map(playbooks::Playbook::described).collect::<Vec<_>>() }),
        ),
        "prompts/get" => match playbooks::named(params["name"].as_str().unwrap_or_default()) {
            Some(playbook) => Ok(json!({
                "description": playbook.about,
                "messages": [{ "role": "user", "content": { "type": "text", "text": playbook.instructions(&params["arguments"]) } }],
            })),
            None => Err((-32602, "there is no such prompt".to_string())),
        },
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
        "capabilities": { "tools": { "listChanged": false }, "prompts": { "listChanged": false } },
        "serverInfo": { "name": "doc", "title": "DOC, through Agent Smith", "version": env!("CARGO_PKG_VERSION") },
        "instructions": INSTRUCTIONS,
    })
}
