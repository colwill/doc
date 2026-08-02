//! CI/CD/CT metrics as an MCP server at `api/mcp`, like DORA's: Streamable HTTP with JSON
//! responses and no sessions. Core checks `plugin:cicd:user` or `:service` as a read, since the
//! manifest declares the route one, and the Catalogue is asked as the same caller, so an agent sees
//! exactly what the person or account behind it would.

use doc_plugin_sdk::{Backend, Request, Response};
use serde_json::{Value, json};

use crate::faux;
use crate::metrics::{self, Band, Counting, Period};
use crate::scope::{self, Scope};
use crate::settings::{Definitions, Stage};
use crate::ui::{self, MAX_DAYS};

const VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const MAX_SPELLS: usize = 50;

const INSTRUCTIONS: &str = "DOC's CI/CD/CT metrics, from GitHub Actions: how often each service's \
pipelines pass, how long they take, how quickly a broken default branch is fixed and how often a \
run passes only when re-run, for continuous integration, delivery and testing. Ask \
pipeline_metrics for a service, team or organisation (or for everything), and broken_pipelines for \
each time a workflow broke on a default branch.";

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
        "serverInfo": { "name": "doc-cicd", "title": "DOC CI/CD/CT metrics", "version": env!("CARGO_PKG_VERSION") },
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
            "name": "pipeline_metrics",
            "title": "CI/CD/CT metrics",
            "description": "For a service, team or organisation, or for everything when none is named: the success rate of its GitHub Actions runs, their duration (median and 90th percentile), the time from a default branch breaking to passing again, and the share of runs that passed only when re-run, each with its band, for every stage together and for integration, delivery and testing each, and the same for the period before.",
            "inputSchema": scoped_input(),
            "annotations": read_only,
        },
        {
            "name": "broken_pipelines",
            "title": "Broken default branches",
            "description": "Each time a workflow broke on a repository's default branch, still broken first and then the newest: which workflow and stage, when it broke, how many runs failed and how long it took to pass again.",
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

/// One line of figures: success rate, duration, time to recover and re-runs, each banded.
fn said(figures: &metrics::Figures, definitions: &Definitions) -> String {
    let bands = figures.bands(&definitions.bands);
    let duration = figures.duration.map_or_else(
        || "none passed".to_string(),
        |spread| {
            format!(
                "median {}, 90th percentile {}",
                metrics::duration(spread.median),
                metrics::duration(spread.p90)
            )
        },
    );
    let recovery = match (figures.recovery, figures.broke) {
        (Some(recovery), _) => format!("median {}", metrics::duration(recovery.median)),
        (None, 0) => "no failure".to_string(),
        (None, _) => "none recovered yet".to_string(),
    };
    format!(
        "{} runs; success rate {}; duration {}; time to recover {}, {} still broken; passed on a re-run {}",
        figures.runs,
        banded(
            figures.success_rate.map_or_else(|| "none".into(), metrics::percent),
            bands.success_rate
        ),
        banded(duration, bands.duration),
        banded(recovery, bands.recovery),
        figures.still_broken,
        banded(figures.rerun_rate.map_or_else(|| "none".into(), metrics::percent), bands.reruns),
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
    let definitions = Definitions::read(&backend.settings());
    let held = match metrics::held(backend, repositories.as_ref(), &period).await {
        Ok(held) => held,
        Err(refusal) => return Ok(failed(&refusal.detail)),
    };
    match params["name"].as_str().unwrap_or_default() {
        "pipeline_metrics" => {
            let before = match metrics::held(backend, repositories.as_ref(), &period.before()).await
            {
                Ok(before) => before,
                Err(refusal) => return Ok(failed(&refusal.detail)),
            };
            let counting = Counting::of(&definitions);
            let figures = metrics::figures(&held, &period, counting);
            let mut lines = vec![format!(
                "{} over the last {}, {}: {}.",
                scope.label(),
                period.said(),
                match definitions.default_only {
                    true => "default branches only",
                    false => "every branch",
                },
                said(&figures, &definitions)
            )];
            let mut stages = serde_json::Map::new();
            for stage in Stage::ALL {
                let theirs = metrics::figures(&held, &period, counting.stage(stage));
                if theirs.runs > 0 {
                    lines.push(format!("{}: {}.", stage.word(), said(&theirs, &definitions)));
                }
                stages.insert(stage.key().to_string(), theirs.json(&definitions.bands));
            }
            let structured = json!({
                "scope": { "kind": scope.kind(), "name": scope.name() },
                "from": period.from,
                "to": period.to,
                "metrics": figures.json(&definitions.bands),
                "stages": stages,
                "previous": metrics::figures(&before, &period.before(), counting).json(&definitions.bands),
            });
            Ok(found(&lines.join("\n"), &structured))
        }
        "broken_pipelines" => {
            let mut spells = ui::ordered(held.recoveries);
            spells.truncate(MAX_SPELLS);
            let lines: Vec<String> = spells
                .iter()
                .map(|spell| {
                    let fixed = spell.seconds.map_or_else(
                        || "still broken".to_string(),
                        |seconds| format!("passed again after {}", metrics::duration(seconds)),
                    );
                    format!(
                        "{} {} {} ({}): {} failed, {fixed}",
                        spell.broke_at.format("%Y-%m-%d %H:%M"),
                        spell.repository,
                        spell.workflow,
                        spell.stage,
                        metrics::plural(spell.failed_runs.max(0) as usize, "run"),
                    )
                })
                .collect();
            let text = match lines.is_empty() {
                true => format!(
                    "No workflow of {} broke on a default branch in the last {}.",
                    scope.label(),
                    period.said()
                ),
                false => lines.join("\n"),
            };
            Ok(found(&text, &json!({ "recoveries": spells })))
        }
        other => Err((-32602, format!("there is no tool {other}"))),
    }
}
