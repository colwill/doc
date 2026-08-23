//! Runs DOC drives itself: Claude, with the API key in the settings, working through a run's brief
//! with tools. It can fetch only from the sources the settings configure, signed in as they say,
//! and it can only stage, as an administrator's own agent can, so nothing reaches the Knowledge
//! Base, Watercooler or the Catalogue until an administrator approves it.
//!
//! The plugin is long-running for this: one run at a time, turn by turn, each turn saved, until
//! Claude finishes, the run reaches its turn limit, or somebody stops it. A conversation lives in
//! memory, so a run DOC restarts in the middle of is marked failed rather than half-resumed.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use doc_plugin_sdk::{Backend, PluginError};
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::api::{self, Handed};
use crate::instructions::Instructions;
use crate::settings::{Auth, Config};
use crate::store::{CLAUDE, COLLECTING, FAILED, Run, Store};

/// Where the Messages API is; `ANTHROPIC_BASE_URL` points it at a proxy or gateway instead.
const API: &str = "https://api.anthropic.com";
const VERSION: &str = "2023-06-01";
const MAX_TOKENS: u32 = 16_000;
/// How much of a fetched answer Claude reads at a time.
const PAGE: usize = 40_000;
/// However large an answer, no more of it is kept.
const MAX_BODY: usize = 4 * 1024 * 1024;
/// Tool results older than this many turns are cut down, so a long run's conversation stays
/// within what the model can read; what was staged from them is kept regardless.
const KEEP_TURNS: usize = 6;
const IDLE: Duration = Duration::from_secs(30);

#[derive(Default)]
pub struct Worker {
    nudge: Notify,
    stop: Mutex<CancellationToken>,
}

impl Worker {
    pub fn wake(&self) {
        self.nudge.notify_one();
    }

    pub fn stop(&self) {
        if let Ok(stop) = self.stop.lock() {
            stop.cancel();
        }
        self.wake();
    }

    pub async fn run(&self, backend: &Backend) -> Result<(), PluginError> {
        let stop = CancellationToken::new();
        if let Ok(mut held) = self.stop.lock() {
            *held = stop.clone();
        }
        interrupted(backend).await;
        while !stop.is_cancelled() {
            let waiting = match Store(backend).runs_in(COLLECTING).await {
                Ok(runs) => runs.into_iter().filter(|run| run.mode == CLAUDE).collect(),
                Err(refusal) => {
                    tracing::warn!(detail = %refusal.detail, "runs could not be read");
                    Vec::new()
                }
            };
            for run in waiting {
                tokio::select! {
                    () = stop.cancelled() => break,
                    () = drive(backend, run) => {}
                }
            }
            tokio::select! {
                () = stop.cancelled() => {}
                () = self.nudge.notified() => {}
                () = tokio::time::sleep(IDLE) => {}
            }
        }
        Ok(())
    }
}

/// Runs Claude had started before DOC last stopped cannot pick their conversation up again.
async fn interrupted(backend: &Backend) {
    let store = Store(backend);
    let Ok(runs) = store.runs_in(COLLECTING).await else { return };
    for mut run in runs.into_iter().filter(|run| run.mode == CLAUDE && run.turns > 0) {
        run.state = FAILED.to_string();
        run.error = Some(
            "DOC restarted while Claude was working. What it staged is kept; start another run \
             for the rest."
                .into(),
        );
        run.finished_at = Some(chrono::Utc::now());
        let _ = store.save_run(&run).await;
    }
}

async fn fail(backend: &Backend, run: &mut Run, why: String) {
    tracing::warn!(run = %run.id, %why, "a Claude run failed");
    run.state = FAILED.to_string();
    run.error = Some(why);
    run.finished_at = Some(chrono::Utc::now());
    let _ = Store(backend).save_run(run).await;
}

fn tools() -> Value {
    let item = |properties: Value, required: &[&str]| json!({ "type": "object", "properties": properties, "required": required });
    let page = item(
        json!({
            "space": { "type": "string" }, "space_title": { "type": "string" },
            "path": { "type": "string" }, "title": { "type": "string" },
            "markdown": { "type": "string" }, "source": { "type": "string" },
            "source_url": { "type": "string" },
        }),
        &["space", "path", "title", "markdown", "source"],
    );
    let resource = item(
        json!({
            "document": { "type": "object" }, "source": { "type": "string" },
            "source_url": { "type": "string" },
        }),
        &["document", "source"],
    );
    let discussion = item(
        json!({
            "title": { "type": "string" }, "body": { "type": "string" },
            "tags": { "type": "array", "items": { "type": "string" } },
            "source": { "type": "string" }, "source_url": { "type": "string" },
        }),
        &["title", "body", "source"],
    );
    let list = |key: &str, of: Value, about: &str| {
        json!({
            "name": format!("stage_{key}"),
            "description": about,
            "input_schema": {
                "type": "object",
                "properties": { key: { "type": "array", "items": of, "maxItems": api::BATCH } },
                "required": [key],
            },
        })
    };
    json!([
        {
            "name": "fetch",
            "description": "GET a URL from one of the run's sources. Long answers come a page at a \
                            time: pass the `offset` the answer gives to read on.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "url": { "type": "string" },
                    "offset": { "type": "integer", "minimum": 0 },
                },
                "required": ["url"],
            },
        },
        list("pages", page, "Stage Knowledge Base pages, up to 50 at a time."),
        list("resources", resource, "Stage Catalogue resources, up to 50 at a time."),
        list("discussions", discussion, "Stage Watercooler discussions, up to 50 at a time."),
        {
            "name": "finish",
            "description": "End the run once everything is handed in, with a summary for the \
                            administrator who reviews it.",
            "input_schema": {
                "type": "object",
                "properties": { "summary": { "type": "string" } },
                "required": ["summary"],
            },
        },
    ])
}

/// What Claude has fetched this run, so reading on through a long answer does not fetch it again.
#[derive(Default)]
struct Fetched(BTreeMap<String, (u16, String, String)>);

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|err| format!("no HTTP client: {err}"))
}

/// The source a URL is on, and how to sign in there; `None` when it is none of them.
fn allowed<'a>(config: &'a Config, url: &url::Url) -> Option<Option<&'a Auth>> {
    let host = url.host_str()?.to_lowercase();
    let with_port = match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.clone(),
    };
    if let Some(connection) = config.connections.iter().find(|connection| {
        connection.base.host_str().map(str::to_lowercase).as_deref() == Some(host.as_str())
            && connection.base.port() == url.port()
    }) {
        return Some(Some(&connection.auth));
    }
    config.hosts.iter().any(|allowed| *allowed == host || *allowed == with_port).then_some(None)
}

async fn fetch(config: &Config, fetched: &mut Fetched, input: &Value) -> Result<String, String> {
    let url = input["url"].as_str().unwrap_or_default().trim().to_string();
    let offset = input["offset"].as_u64().unwrap_or(0) as usize;
    if !fetched.0.contains_key(&url) {
        let parsed = url::Url::parse(&url).map_err(|_| format!("`{url}` is not a URL"))?;
        if !matches!(parsed.scheme(), "https" | "http") {
            return Err("only http and https URLs are fetched".into());
        }
        let Some(auth) = allowed(config, &parsed) else {
            let mut known: Vec<String> = config
                .connections
                .iter()
                .filter_map(|connection| connection.base.host_str().map(str::to_string))
                .collect();
            known.extend(config.hosts.iter().cloned());
            return Err(format!(
                "{} is not one of this run's sources, which are {}",
                parsed.host_str().unwrap_or("that"),
                if known.is_empty() { "none yet".to_string() } else { known.join(", ") }
            ));
        };
        // A redirect is followed only to another of the run's sources, never elsewhere.
        let hosts: Vec<(String, Option<u16>)> = config
            .connections
            .iter()
            .filter_map(|connection| {
                Some((connection.base.host_str()?.to_lowercase(), connection.base.port()))
            })
            .chain(config.hosts.iter().map(|host| match host.rsplit_once(':') {
                Some((name, port)) if port.parse::<u16>().is_ok() => {
                    (name.to_string(), port.parse().ok())
                }
                _ => (host.clone(), None),
            }))
            .collect();
        let policy = reqwest::redirect::Policy::custom(move |attempt| {
            let url = attempt.url();
            let host = url.host_str().map(str::to_lowercase).unwrap_or_default();
            let fine = hosts
                .iter()
                .any(|(known, port)| *known == host && (port.is_none() || *port == url.port()));
            match fine && attempt.previous().len() < 5 {
                true => attempt.follow(),
                false => attempt.stop(),
            }
        });
        let fetcher = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .redirect(policy)
            .build()
            .map_err(|err| format!("no HTTP client: {err}"))?;
        let mut asked = fetcher
            .get(parsed)
            .header("accept", "application/json, text/markdown, text/plain, text/html;q=0.8");
        asked = match auth {
            Some(Auth::Basic { user, secret }) => asked.basic_auth(user, Some(secret.expose())),
            Some(Auth::Bearer(secret)) => asked.bearer_auth(secret.expose()),
            _ => asked,
        };
        let answer = asked.send().await.map_err(|err| format!("the fetch failed: {err}"))?;
        let status = answer.status().as_u16();
        let kind = answer
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string();
        let bytes = answer.bytes().await.map_err(|err| format!("the answer was cut off: {err}"))?;
        let text: String = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_BODY)]).into();
        fetched.0.insert(url.clone(), (status, kind, text));
    }
    let (status, kind, text) = &fetched.0[&url];
    let chars: Vec<char> = text.chars().collect();
    let from = offset.min(chars.len());
    let to = (from + PAGE).min(chars.len());
    let more = match to < chars.len() {
        true => format!("; read on with offset {to}"),
        false => String::new(),
    };
    let shown: String = chars[from..to].iter().collect();
    Ok(format!(
        "status {status}, {kind}, {} characters (showing {from} to {to}{more})\n\n{shown}",
        chars.len()
    ))
}

async fn call(key: &str, model: &str, system: &str, messages: &[Value]) -> Result<Value, String> {
    let body = json!({
        "model": model, "max_tokens": MAX_TOKENS, "system": system,
        "tools": tools(), "messages": messages,
    });
    let mut wait = Duration::from_secs(5);
    for attempt in 0..4 {
        let base = std::env::var("ANTHROPIC_BASE_URL").unwrap_or_else(|_| API.to_string());
        let answer = client()?
            .post(format!("{}/v1/messages", base.trim_end_matches('/')))
            .header("x-api-key", key)
            .header("anthropic-version", VERSION)
            .json(&body)
            .send()
            .await;
        match answer {
            Ok(answer) if answer.status().is_success() => {
                return answer.json().await.map_err(|err| format!("Claude's answer: {err}"));
            }
            Ok(answer) => {
                let status = answer.status().as_u16();
                let detail: Value = answer.json().await.unwrap_or_default();
                let message = detail["error"]["message"].as_str().unwrap_or("no detail");
                // Busy, rate-limited or briefly down: try again, a little later each time.
                if (status == 429 || status >= 500) && attempt < 3 {
                    tokio::time::sleep(wait).await;
                    wait *= 3;
                    continue;
                }
                return Err(format!("Claude answered {status}: {message}"));
            }
            Err(err) if attempt < 3 => {
                tracing::warn!(%err, "Claude could not be reached; trying again");
                tokio::time::sleep(wait).await;
                wait *= 3;
            }
            Err(err) => return Err(format!("Claude could not be reached: {err}")),
        }
    }
    Err("Claude could not be reached".into())
}

/// Cuts long tool results in all but the latest turns down to a line.
fn forget_old(messages: &mut [Value]) {
    let results: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| {
            message["content"]
                .as_array()
                .is_some_and(|blocks| blocks.iter().any(|block| block["type"] == "tool_result"))
        })
        .map(|(index, _)| index)
        .collect();
    let old = results.len().saturating_sub(KEEP_TURNS);
    for index in results.into_iter().take(old) {
        if let Some(blocks) = messages[index]["content"].as_array_mut() {
            for block in blocks.iter_mut() {
                let long = block["content"].as_str().is_some_and(|text| text.len() > 1_500);
                if block["type"] == "tool_result" && long {
                    block["content"] = json!(
                        "(read in an earlier turn and no longer kept; what was staged from it is)"
                    );
                }
            }
        }
    }
}

/// One tool call, answered as its result and whether it failed.
async fn answer(
    backend: &Backend,
    config: &Config,
    fetched: &mut Fetched,
    run: &Run,
    name: &str,
    input: &Value,
) -> (String, bool) {
    let handed = match name {
        "fetch" => {
            return match fetch(config, fetched, input).await {
                Ok(text) => (text, false),
                Err(problem) => (problem, true),
            };
        }
        "stage_pages" => Handed::Pages,
        "stage_resources" => Handed::Resources,
        "stage_discussions" => Handed::Discussions,
        other => return (format!("there is no tool called {other}"), true),
    };
    match api::hand_in(backend, run, handed, input).await {
        Ok(result) => (result.to_string(), false),
        Err(refusal) => (refusal.detail, true),
    }
}

/// Works through one run until Claude finishes it, it runs out of turns, or it is stopped.
async fn drive(backend: &Backend, mut run: Run) {
    let store = Store(backend);
    let config = Config::read(&backend.settings());
    let Some(key) = config.api_key.clone() else {
        fail(backend, &mut run, "there is no Claude API key in the settings".into()).await;
        return;
    };
    let system = Instructions::new(&run, &config, None, None).text();
    let mut messages = vec![json!({
        "role": "user",
        "content": "Start the run: take what the brief asks from the sources, hand it in, then finish.",
    })];
    let mut fetched = Fetched::default();
    let mut nudged = 0;
    for _ in 0..config.max_turns {
        // Somebody may have stopped or cancelled it between turns.
        match store.run(run.id).await {
            Ok(Some(now)) if now.state == COLLECTING => run = now,
            _ => return,
        }
        let reply = match call(key.expose(), &config.model, &system, &messages).await {
            Ok(reply) => reply,
            Err(problem) => return fail(backend, &mut run, problem).await,
        };
        run.turns += 1;
        let _ = store.save_run(&run).await;
        let content = reply["content"].clone();
        messages.push(json!({ "role": "assistant", "content": content }));
        let uses: Vec<&Value> = content
            .as_array()
            .map(|blocks| blocks.iter().filter(|block| block["type"] == "tool_use").collect())
            .unwrap_or_default();
        if uses.is_empty() {
            let said: String = content
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|block| block["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n");
            let more = match reply["stop_reason"].as_str() {
                Some("max_tokens") => "Go on.",
                _ if nudged < 2 => {
                    nudged += 1;
                    "Hand in anything left with the stage tools, then call finish with a summary."
                }
                _ => {
                    let _ = api::finish(backend, &run, &said).await;
                    return;
                }
            };
            messages.push(json!({ "role": "user", "content": more }));
            continue;
        }
        let mut results = Vec::new();
        let mut finished = None;
        for used in uses {
            let name = used["name"].as_str().unwrap_or_default();
            if name == "finish" {
                finished = Some(used["input"]["summary"].as_str().unwrap_or_default().to_string());
                results.push(json!({
                    "type": "tool_result", "tool_use_id": used["id"], "content": "Finished.",
                }));
                continue;
            }
            let (text, failed) =
                answer(backend, &config, &mut fetched, &run, name, &used["input"]).await;
            results.push(json!({
                "type": "tool_result", "tool_use_id": used["id"], "content": text,
                "is_error": failed,
            }));
        }
        if let Some(summary) = finished {
            if let Err(refusal) = api::finish(backend, &run, &summary).await {
                tracing::warn!(detail = %refusal.detail, "a finished run was not saved");
            }
            return;
        }
        messages.push(json!({ "role": "user", "content": results }));
        forget_old(&mut messages);
    }
    let summary = format!(
        "Stopped after {} turns, the most the settings allow. What was handed in is here to review; \
         start another run for the rest.",
        config.max_turns
    );
    let _ = api::finish(backend, &run, &summary).await;
}
