//! Jobs and runbooks DOC runs itself: Claude, driven turn by turn by this long-running worker, which
//! holds no one's access. Every tool call is a background run of its own, made with the run's
//! access — a job's owner, whoever pressed Run on a runbook, or Agent Smith's own service account —
//! checked as it is at that moment, limited by the run's environment, and handed back here when it
//! answers.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use chrono::Utc;
use doc_plugin_sdk::{Backend, PluginError};
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::jobs::WAKE;
use crate::settings::{Auth, Config};
use crate::store::{
    AGENT, CALLS, Call, DONE, FAILED, Job, QUEUED, REQUESTER, RUNNING, RUNS, Run, STOPPED, Store,
};
use crate::{playbooks, runbooks, tools};

/// Where the Messages API is; `ANTHROPIC_BASE_URL` points it at a proxy or gateway instead.
const API: &str = "https://api.anthropic.com";
const VERSION: &str = "2023-06-01";
const MAX_TOKENS: u32 = 8_000;
/// How much of a fetched answer Claude reads at a time, and the most of one kept.
const PAGE: usize = 40_000;
const MAX_BODY: usize = 4 * 1024 * 1024;
/// Tool results older than this many turns are cut down, so a long run stays readable.
const KEEP_TURNS: usize = 6;
const IDLE: Duration = Duration::from_secs(10);
/// How long a call made as the owner has to come back: the queue, then its 30 seconds.
const PATIENCE: Duration = Duration::from_secs(90);
/// The most a tool's input may be, since a record holds at most a mebibyte.
const MAX_INPUT: usize = 900 * 1024;

#[derive(Default)]
pub struct Worker {
    stop: Mutex<CancellationToken>,
    waiting: Mutex<HashMap<Uuid, oneshot::Sender<Value>>>,
}

impl Worker {
    pub fn stop(&self) {
        if let Ok(stop) = self.stop.lock() {
            stop.cancel();
        }
        WAKE.notify_one();
    }

    pub async fn run(&self, backend: &Backend) -> Result<(), PluginError> {
        let stop = CancellationToken::new();
        if let Ok(mut held) = self.stop.lock() {
            *held = stop.clone();
        }
        interrupted(backend).await;
        while !stop.is_cancelled() {
            let queued =
                Store(backend).runs(json!({ "state": QUEUED }), 50).await.unwrap_or_default();
            for run in queued.into_iter().rev() {
                tokio::select! {
                    () = stop.cancelled() => break,
                    () = Box::pin(drive(self, backend, run)) => {}
                }
            }
            tokio::select! {
                () = stop.cancelled() => {}
                () = WAKE.notified() => {}
                () = tokio::time::sleep(IDLE) => {}
            }
        }
        Ok(())
    }

    fn hand(&self, key: Uuid, value: Value) {
        let waiting = self.waiting.lock().ok().and_then(|mut waiting| waiting.remove(&key));
        if let Some(sender) = waiting {
            let _ = sender.send(value);
        }
    }

    /// A background run made as a job's owner: the tools they have, or one tool call, answered
    /// to the drive waiting for it and kept on the run.
    pub async fn answer(&self, backend: &Backend, payload: &Value) -> Result<Value, PluginError> {
        let store = Store(backend);
        let uuid = |field: &str| payload[field].as_str().and_then(|id| id.parse::<Uuid>().ok());
        if let Some(run) = uuid("tools") {
            let mut listed = tools::available(backend, true).await;
            // Against production a run only reads, so it is offered only what reads.
            if store.run(run).await.is_ok_and(|run| run.environment == "production") {
                listed.retain(|tool| {
                    tool["annotations"]["readOnlyHint"] == true || tool["name"] == "doc_api"
                });
            }
            let count = listed.len();
            self.hand(run, json!({ "tools": listed }));
            return Ok(json!({ "tools": count }));
        }
        if let (Some(run), Some(reply)) = (uuid("runbook"), uuid("reply")) {
            let read = match store.run(run).await {
                Ok(run) => runbooks::read(backend, &run.runbook).await,
                Err(refusal) => Err(refusal),
            };
            let answer = read.unwrap_or_else(|refusal| json!({ "error": refusal.detail }));
            self.hand(reply, answer);
            return Ok(json!({ "runbook": run }));
        }
        let id =
            uuid("call").ok_or_else(|| PluginError::from("a run names a call or a tool list"))?;
        let call = store.call(id).await?;
        let environment = store.run(call.run).await.map(|run| run.environment).unwrap_or_default();
        // Turn 0 is delivering the report, which is DOC's own and changes nothing the run is about.
        let outcome = match (call.turn, runbooks::guard(&environment)) {
            (0, _) | (_, None) => tools::call(backend, &call.tool, &call.input).await,
            (_, Some(_))
                if environment == "production"
                    && !tools::only_reads(backend, &call.tool, &call.input).await =>
            {
                tools::Outcome {
                    text: "refused: this run is against production, so it only reads. Put this \
                           change in the report as a step for the requester."
                        .into(),
                    failed: true,
                }
            }
            (_, Some(guard)) => tools::call(&backend.guarded(guard), &call.tool, &call.input).await,
        };
        let set = json!({ "result": outcome.text, "failed": outcome.failed, "done": true });
        let _ = store.update::<Value>(CALLS, id, set).await;
        self.hand(id, json!({ "text": outcome.text, "failed": outcome.failed }));
        Ok(json!({ "call": id, "failed": outcome.failed }))
    }
}

/// Runs DOC was driving when it stopped cannot pick their conversation up again.
async fn interrupted(backend: &Backend) {
    let store = Store(backend);
    let Ok(runs) = store.runs(json!({ "state": RUNNING }), 100).await else { return };
    for run in runs {
        let set = json!({
            "state": FAILED, "finished_at": Utc::now(),
            "error": "DOC restarted while it was running. Run it again.",
        });
        let _ = store.update::<Value>(RUNS, run.id, set).await;
        give_back(backend, &run).await;
    }
}

/// Whose access a run's calls are made with.
#[derive(Clone, Copy)]
enum Acting {
    /// Leave somebody gave: a job's owner, or whoever pressed Run on a runbook.
    Leave(Uuid),
    /// Agent Smith's own service account, for a runbook run by itself.
    Itself,
}

fn acting_of(run: &Run, job: Option<&Job>) -> Option<Acting> {
    match run.access.as_str() {
        AGENT => Some(Acting::Itself),
        REQUESTER => run.delegation.map(Acting::Leave),
        _ => job.and_then(|job| job.delegation).map(Acting::Leave),
    }
}

/// Whom a run's calls are made as, in words.
fn who_of(run: &Run, job: Option<&Job>) -> String {
    match run.access.as_str() {
        AGENT => format!("Agent Smith ({})", crate::ACCOUNT),
        REQUESTER => run.owner_label.clone(),
        _ => job.map(|job| job.owner_label.clone()).unwrap_or_default(),
    }
}

/// Gives back the leave whoever pressed Run lent a run, once it has ended.
async fn give_back(backend: &Backend, run: &Run) {
    if let Some(delegation) = run.delegation {
        let _ = backend.revoke(delegation).await;
    }
}

/// Asks for something to be done with the run's access, in a run of its own, and waits for it.
/// It is tried once: a call that changes something is never made twice.
async fn delegated(
    worker: &Worker,
    backend: &Backend,
    (acting, who): (Acting, &str),
    key: Uuid,
    payload: Value,
) -> Result<Value, String> {
    let (sender, receiver) = oneshot::channel();
    if let Ok(mut waiting) = worker.waiting.lock() {
        waiting.insert(key, sender);
    }
    let started = match acting {
        Acting::Leave(delegation) => backend.task_as(delegation, payload, Some(1)).await,
        Acting::Itself => backend.task_tried(payload, 1).await,
    };
    if let Err(err) = started {
        if let Ok(mut waiting) = worker.waiting.lock() {
            waiting.remove(&key);
        }
        return Err(format!("it could not be done as {who}: {}", err.detail()));
    }
    match tokio::time::timeout(PATIENCE, receiver).await {
        Ok(Ok(value)) => Ok(value),
        _ => {
            if let Ok(mut waiting) = worker.waiting.lock() {
                waiting.remove(&key);
            }
            Err("it did not answer in time".into())
        }
    }
}

/// One tool call made with the run's access, kept on the run whatever it answers.
async fn act(
    worker: &Worker,
    backend: &Backend,
    acting: (Acting, &str),
    run: &Run,
    turn: i64,
    tool: &str,
    input: &Value,
) -> (String, bool) {
    let store = Store(backend);
    // The run made as the owner reads the call from its record, so the record holds all of it.
    if input.to_string().len() > MAX_INPUT {
        return (
            "that input is too large to hand over in one call: send less at a time".into(),
            true,
        );
    }
    let values = json!({ "id": Uuid::now_v7(), "run": run.id, "turn": turn, "tool": tool, "input": input, "done": false });
    let call: Call = match store.insert(CALLS, values).await {
        Ok(call) => call,
        Err(refusal) => {
            return (format!("the call could not be recorded: {}", refusal.detail), true);
        }
    };
    match delegated(worker, backend, acting, call.id, json!({ "call": call.id })).await {
        Ok(answer) => (
            answer["text"].as_str().unwrap_or_default().to_string(),
            answer["failed"].as_bool().unwrap_or(false),
        ),
        Err(why) => {
            let _ = store
                .update::<Value>(
                    CALLS,
                    call.id,
                    json!({ "result": why, "failed": true, "done": true }),
                )
                .await;
            (why, true)
        }
    }
}

async fn fail(backend: &Backend, job: Option<&Job>, run: &Run, why: String) {
    tracing::warn!(run = %run.id, %why, "a run failed");
    let set = json!({ "state": FAILED, "error": why, "finished_at": Utc::now() });
    let _ = Store(backend).update::<Value>(RUNS, run.id, set).await;
    give_back(backend, run).await;
    let _ = backend
        .notify(
            &run.owner.to_string(),
            &format!("Agent Smith could not finish {}", run.title(job)),
            &why,
            Some(&run.href()),
        )
        .await;
}

/// What Claude is told a runbook's run is, and the runbook itself as its first message.
fn runbook_brief(run: &Run, runbook: &Value) -> (String, String) {
    let access = match run.access.as_str() {
        AGENT => format!(
            "Agent Smith's own access, as the service account {}, which holds only what \
             administrators granted it: a refusal means it was not given that",
            crate::ACCOUNT
        ),
        _ => format!("the access of {}, who asked for it, as it is at each call", run.owner_label),
    };
    let who = format!(
        "It is {now}. The requester is {owner}. Every call you make is made with {access}. When \
         you are done, call finish with the report in Markdown, written for {owner}.",
        now = Utc::now().format("%A %-d %B %Y, %H:%M UTC"),
        owner = run.owner_label,
    );
    let system = format!("{}\n\n{}", playbooks::DOC, runbooks::instructions(run, &who));
    let first = format!(
        "Run this runbook.\n\n<runbook>\n{}\n</runbook>",
        runbook["markdown"].as_str().unwrap_or_default()
    );
    (system, first)
}

/// What the owner and Claude are told the run is about.
fn instructions(job: &Job, run: &Run) -> String {
    let playbook = playbooks::named(&job.playbook).unwrap_or(&playbooks::ALL[4]);
    let about = job.scope.split_once(':').map(|(_, name)| name.trim()).unwrap_or_default();
    let events: Vec<String> = run
        .events
        .as_array()
        .into_iter()
        .flatten()
        .map(|event| {
            let payload = event["payload"].to_string();
            format!(
                "- {} at {}: {}",
                event["topic"].as_str().unwrap_or_default(),
                event["at"].as_str().unwrap_or_default(),
                payload.chars().take(2_000).collect::<String>()
            )
        })
        .collect();
    let happened = match events.is_empty() {
        true => String::new(),
        false => format!("What happened:\n{}", events.join("\n")),
    };
    let arguments = json!({
        "from": job.brief, "what": "", "brief": job.brief, "scope": about,
        "subject": if about.is_empty() { job.brief.as_str() } else { about },
        "happened": if events.is_empty() { "see the timeline" } else { happened.as_str() },
    });
    let scope = match job.scope.is_empty() {
        true => String::new(),
        false => format!(", about {}", job.scope),
    };
    let brief = match job.brief.is_empty() {
        true => "none beyond the playbook".to_string(),
        false => job.brief.clone(),
    };
    format!(
        "{}\n\nYou work for {owner}. It is {now}. The job is \"{title}\"{scope}. Their brief: {brief}\n\n\
         This run was started by {why}.\n{happened}\n\nYou may also read the sources in Agent Smith's \
         settings with doc_fetch. When you are done, call finish with the report in Markdown, written \
         for {owner}. Say what you changed as well as what you found: whatever you changed, you \
         changed as them.",
        playbook.instructions(&arguments),
        owner = job.owner_label,
        now = Utc::now().format("%A %-d %B %Y, %H:%M UTC"),
        title = job.title,
        why = run.why,
    )
}

fn client(timeout: Duration) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|err| format!("no HTTP client: {err}"))
}

async fn claude(
    key: &str,
    model: &str,
    system: &str,
    messages: &[Value],
    tools: &[Value],
) -> Result<Value, String> {
    let body = json!({ "model": model, "max_tokens": MAX_TOKENS, "system": system, "tools": tools, "messages": messages });
    let mut wait = Duration::from_secs(5);
    for attempt in 0..4 {
        let base = std::env::var("ANTHROPIC_BASE_URL").unwrap_or_else(|_| API.to_string());
        let answer = client(Duration::from_secs(300))?
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
                    block["content"] = json!("(read in an earlier turn and no longer kept)");
                }
            }
        }
    }
}

/// What has been fetched this run, so reading on through a long answer does not fetch it again.
#[derive(Default)]
struct Fetched(HashMap<String, (u16, String, String)>);

/// The source an address is under, if any.
fn source<'a>(config: &'a Config, url: &url::Url) -> Option<&'a crate::settings::Source> {
    config.sources.iter().find(|source| {
        source.base.scheme() == url.scheme()
            && source.base.host_str() == url.host_str()
            && source.base.port_or_known_default() == url.port_or_known_default()
            && url.path().starts_with(source.base.path().trim_end_matches('/'))
    })
}

async fn fetch(config: &Config, fetched: &mut Fetched, input: &Value) -> (String, bool) {
    let url = input["url"].as_str().unwrap_or_default().trim().to_string();
    let offset = input["offset"].as_u64().unwrap_or(0) as usize;
    if !fetched.0.contains_key(&url) {
        let Ok(parsed) = url::Url::parse(&url) else {
            return (format!("`{url}` is not an address"), true);
        };
        let Some(found) = source(config, &parsed) else {
            let known: Vec<String> =
                config.sources.iter().map(|source| source.base.to_string()).collect();
            let known = if known.is_empty() { "none yet".to_string() } else { known.join(", ") };
            return (
                format!(
                    "{url} is under none of the sources in Agent Smith's settings, which are {known}"
                ),
                true,
            );
        };
        let hosts: Vec<(String, Option<u16>)> = config
            .sources
            .iter()
            .filter_map(|source| {
                Some((source.base.host_str()?.to_string(), source.base.port_or_known_default()))
            })
            .collect();
        let policy = reqwest::redirect::Policy::custom(move |attempt| {
            let url = attempt.url();
            let fine = hosts.iter().any(|(host, port)| {
                url.host_str() == Some(host.as_str()) && url.port_or_known_default() == *port
            });
            match fine && attempt.previous().len() < 5 {
                true => attempt.follow(),
                false => attempt.stop(),
            }
        });
        let Ok(fetcher) =
            reqwest::Client::builder().timeout(Duration::from_secs(60)).redirect(policy).build()
        else {
            return ("no HTTP client".into(), true);
        };
        let mut asked = fetcher
            .get(parsed)
            .header("accept", "application/json, text/markdown, text/plain, text/html;q=0.8");
        asked = match &found.auth {
            Auth::Bearer(secret) => asked.bearer_auth(secret.expose()),
            Auth::Basic(user, secret) => asked.basic_auth(user, Some(secret.expose())),
            Auth::None => asked,
        };
        let answer = match asked.send().await {
            Ok(answer) => answer,
            Err(err) => return (format!("the fetch failed: {err}"), true),
        };
        let status = answer.status().as_u16();
        let kind = answer
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string();
        let bytes = match answer.bytes().await {
            Ok(bytes) => bytes,
            Err(err) => return (format!("the answer was cut off: {err}"), true),
        };
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
    (
        format!(
            "status {status}, {kind}, {} characters (showing {from} to {to}{more})\n\n{shown}",
            chars.len()
        ),
        false,
    )
}

fn finish_tool() -> Value {
    json!({
        "name": "finish",
        "description": "End the run with the report, in Markdown, for the person the job is for.",
        "input_schema": { "type": "object", "properties": { "report": { "type": "string" } }, "required": ["report"] },
    })
}

/// Works through one run until Claude finishes it, it runs out of turns, or somebody stops it.
async fn drive(worker: &Worker, backend: &Backend, run: Run) {
    let store = Store(backend);
    let held = match run.job {
        Some(id) => match store.job(id).await {
            Ok(job) => Some(job),
            Err(_) => return fail(backend, None, &run, "its job was deleted".into()).await,
        },
        None => None,
    };
    let job = held.as_ref();
    let Some(acting) = acting_of(&run, job) else {
        return fail(backend, job, &run, "it holds no leave to act as anybody".into()).await;
    };
    let who = who_of(&run, job);
    let acting = (acting, who.as_str());
    let config = Config::read(&backend.settings());
    let Some(key) = config.api_key.clone() else {
        return fail(
            backend,
            job,
            &run,
            "there is no Claude API key in Agent Smith's settings".into(),
        )
        .await;
    };
    let started = json!({ "state": RUNNING, "started_at": Utc::now() });
    let Ok(mut run) = store.update::<Run>(RUNS, run.id, started).await else { return };
    let (system, first) = match (job, run.runbook.is_empty()) {
        (Some(job), true) => (instructions(job, &run), "Start the job.".to_string()),
        (None, true) => {
            return fail(backend, None, &run, "it has no job and no runbook".into()).await;
        }
        (_, false) => {
            // A runbook is read with the run's own access, so it runs only what that can read.
            let asked = json!({ "runbook": run.id, "reply": Uuid::now_v7() });
            let reply = asked["reply"].as_str().and_then(|id| id.parse().ok()).unwrap_or(run.id);
            let runbook = match delegated(worker, backend, acting, reply, asked).await {
                Ok(read) if read["error"].is_null() => read,
                Ok(read) => {
                    let why = format!("the runbook could not be read: {}", read["error"]);
                    return fail(backend, job, &run, why).await;
                }
                Err(why) => {
                    return fail(
                        backend,
                        job,
                        &run,
                        format!("the runbook could not be read: {why}"),
                    )
                    .await;
                }
            };
            let hash = runbooks::text(&runbook, "hash");
            if run.access == AGENT {
                match store.approval(&run.runbook).await {
                    Ok(Some(approval)) if approval.hash == hash => {}
                    Ok(Some(approval)) => {
                        let why = format!(
                            "the runbook changed since {} approved it on {}. Somebody holding \
                             Agent Smith's runbooks permission approves the new version on its \
                             page before it runs by itself again.",
                            approval.approved_by_label,
                            approval.approved_at.format("%-d %b %Y")
                        );
                        return fail(backend, job, &run, why).await;
                    }
                    Ok(None) => {
                        let why = "the runbook is no longer approved for Agent Smith to run by \
                                   itself"
                            .to_string();
                        return fail(backend, job, &run, why).await;
                    }
                    Err(refusal) => return fail(backend, job, &run, refusal.detail).await,
                }
            }
            if let Ok(now) = store.update::<Run>(RUNS, run.id, json!({ "hash": hash })).await {
                run = now;
            }
            runbook_brief(&run, &runbook)
        }
    };
    let listed = match delegated(worker, backend, acting, run.id, json!({ "tools": run.id })).await
    {
        Ok(answer) => answer["tools"].as_array().cloned().unwrap_or_default(),
        Err(why) => {
            return fail(backend, job, &run, format!("the tools could not be listed: {why}")).await;
        }
    };
    let mut offered: Vec<Value> = listed
        .iter()
        .map(|tool| json!({ "name": tool["name"], "description": tool["description"], "input_schema": tool["inputSchema"] }))
        .collect();
    offered.push(finish_tool());
    let mut messages = vec![json!({ "role": "user", "content": first })];
    let mut fetched = Fetched::default();
    let mut nudged = 0;
    for turn in 1..=config.max_turns as i64 {
        match store.run(run.id).await {
            Ok(now) if now.state == RUNNING => run = now,
            _ => return,
        }
        let reply = match claude(key.expose(), &config.model, &system, &messages, &offered).await {
            Ok(reply) => reply,
            Err(why) => return fail(backend, job, &run, why).await,
        };
        let used = json!({
            "turns": turn,
            "input_tokens": run.input_tokens + reply["usage"]["input_tokens"].as_i64().unwrap_or(0),
            "output_tokens": run.output_tokens + reply["usage"]["output_tokens"].as_i64().unwrap_or(0),
        });
        if let Ok(now) = store.update::<Run>(RUNS, run.id, used).await {
            run = now;
        }
        let content = reply["content"].clone();
        messages.push(json!({ "role": "assistant", "content": content }));
        let uses: Vec<Value> = content
            .as_array()
            .map(|blocks| {
                blocks.iter().filter(|block| block["type"] == "tool_use").cloned().collect()
            })
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
                    "Call finish with your report."
                }
                _ => return complete(worker, backend, (job, acting), &run, &said).await,
            };
            messages.push(json!({ "role": "user", "content": more }));
            continue;
        }
        if let Some(finished) = uses.iter().find(|used| used["name"] == "finish") {
            let report = finished["input"]["report"].as_str().unwrap_or_default().to_string();
            return complete(worker, backend, (job, acting), &run, &report).await;
        }
        let mut local = Vec::new();
        let mut remote = Vec::new();
        for used in &uses {
            match used["name"].as_str() {
                Some("doc_fetch") => local.push(used),
                _ => remote.push(used),
            }
        }
        let mut answers: HashMap<String, (String, bool)> = HashMap::new();
        for used in local {
            answers.insert(
                used["id"].as_str().unwrap_or_default().to_string(),
                fetch(&config, &mut fetched, &used["input"]).await,
            );
        }
        let calls = remote.iter().map(|used| {
            let run = &run;
            async move {
                let name = used["name"].as_str().unwrap_or_default();
                (
                    used["id"].as_str().unwrap_or_default().to_string(),
                    act(worker, backend, acting, run, turn, name, &used["input"]).await,
                )
            }
        });
        answers.extend(futures::future::join_all(calls).await);
        let results: Vec<Value> = uses
            .iter()
            .map(|used| {
                let id = used["id"].as_str().unwrap_or_default();
                let (text, failed) = answers.remove(id).unwrap_or_else(|| ("no answer".into(), true));
                json!({ "type": "tool_result", "tool_use_id": id, "content": text, "is_error": failed })
            })
            .collect();
        messages.push(json!({ "role": "user", "content": results }));
        forget_old(&mut messages);
    }
    let report = format!(
        "Stopped after {} turns, the most Agent Smith's settings allow. Its tool calls are listed below; run it again, or raise the limit.",
        config.max_turns
    );
    complete(worker, backend, (job, acting), &run, &report).await;
}

/// A finished run: its report kept, and sent where the job says; a runbook's always goes to its
/// requester. Leave lent for it is given back once the report is delivered.
async fn complete(
    worker: &Worker,
    backend: &Backend,
    (job, acting): (Option<&Job>, (Acting, &str)),
    run: &Run,
    report: &str,
) {
    deliver(worker, backend, (job, acting), run, report).await;
    give_back(backend, run).await;
}

async fn deliver(
    worker: &Worker,
    backend: &Backend,
    (job, acting): (Option<&Job>, (Acting, &str)),
    run: &Run,
    report: &str,
) {
    let store = Store(backend);
    let report = match report.trim() {
        "" => "It finished without saying anything.".to_string(),
        said => said.to_string(),
    };
    let set = json!({ "state": DONE, "report": report, "finished_at": Utc::now() });
    let _ = store.update::<Value>(RUNS, run.id, set).await;
    let headline = report
        .lines()
        .map(|line| line.trim_start_matches(['#', ' ', '*']))
        .find(|line| !line.is_empty())
        .unwrap_or("It is done.");
    let headline: String = headline.chars().take(200).collect();
    let title = run.title(job);
    let told = |how: &str| job.is_some_and(|job| job.deliver.iter().any(|held| held == how));
    if let (Some(job), true) = (job, told("discuss")) {
        let body =
            format!("{report}\n\n[The run, and every call it made](/p/agent/runs/{})", run.id);
        let body: String = body.chars().take(60_000).collect();
        let tags: Vec<&str> =
            std::iter::once(job.scope.as_str()).filter(|scope| !scope.is_empty()).collect();
        let title = format!("{}: {}", job.title, Utc::now().format("%-d %b %Y"));
        let input = json!({ "title": title, "body": body, "tags": tags });
        let (said, failed) = act(worker, backend, acting, run, 0, "doc_discuss", &input).await;
        if failed {
            tracing::warn!(run = %run.id, %said, "the report was not posted as a discussion");
        }
    }
    if let (true, Some(("team", team))) =
        (told("team"), job.and_then(|job| job.scope.split_once(':')))
    {
        let input = json!({ "to": team, "title": format!("Agent Smith: {title}"), "body": headline, "url": run.href() });
        let _ = act(worker, backend, acting, run, 0, "doc_notify", &input).await;
    }
    if told("notify") || !run.runbook.is_empty() {
        let _ = backend
            .notify(
                &run.owner.to_string(),
                &format!("Agent Smith: {title}"),
                &headline,
                Some(&run.href()),
            )
            .await;
    }
}

/// Stops a run that is waiting or going; it ends at its next turn.
pub async fn stop(backend: &Backend, run: &Run) -> Result<(), crate::Refusal> {
    if run.state != QUEUED && run.state != RUNNING {
        return Err(crate::Refusal::conflict("it is not running"));
    }
    let set = json!({ "state": STOPPED, "finished_at": Utc::now(), "error": "Stopped by hand." });
    Store(backend).update::<Value>(RUNS, run.id, set).await?;
    give_back(backend, run).await;
    Ok(())
}
