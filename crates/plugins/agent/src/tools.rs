//! The tools an agent has, each made as whoever it works for: DOC's own, written here, and every
//! plugin's MCP tools, asked of that plugin as the same person, so an agent sees what they see.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use doc_plugin_sdk::{Backend, PersonRequest, Query};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::store::{REMINDERS, Reminder, Store, or_default};

/// However long an answer, an agent is given no more of it than this.
const MOST: usize = 30_000;
/// The plugins answering `api/readiness` for a service.
const MEASURING: [&str; 5] = ["maturity", "dora", "cicd", "reliability", "eol"];

pub struct Tool {
    pub name: &'static str,
    pub title: &'static str,
    pub about: &'static str,
    pub schema: Value,
    pub reads: bool,
}

fn object(properties: Value, required: &[&str]) -> Value {
    let mut schema = serde_json::Map::new();
    schema.insert("type".into(), json!("object"));
    schema.insert("properties".into(), properties);
    schema.insert("required".into(), json!(required));
    Value::Object(schema)
}

/// DOC's own tools; `jobs` adds the ones only a job DOC runs has.
pub fn own(jobs: bool) -> Vec<Tool> {
    let text = |about: &str| json!({ "type": "string", "description": about });
    let mut tools = vec![
        Tool {
            name: "doc_search",
            title: "Search the Catalogue",
            about: "Services, teams, repositories, documentation and cloud resources whose names or descriptions match.",
            schema: object(
                json!({ "query": text("What to look for"), "kinds": text("Kinds, comma separated, such as service,team; all when empty") }),
                &["query"],
            ),
            reads: true,
        },
        Tool {
            name: "doc_resource",
            title: "A resource and its neighbours",
            about: "One resource in the Catalogue, with its metadata, owner and everything connected to it.",
            schema: object(
                json!({ "kind": text("Service, Team, Repository, Documentation, CloudResource or Organisation"), "name": text("Its name, such as payments-api") }),
                &["kind", "name"],
            ),
            reads: true,
        },
        Tool {
            name: "doc_readiness",
            title: "Is a service ready",
            about: "Asks every plugin that measures services (maturity, DORA, CI/CD/CT, reliability, end of life) whether a service is ready, and why not.",
            schema: object(json!({ "service": text("The service's name") }), &["service"]),
            reads: true,
        },
        Tool {
            name: "doc_status",
            title: "DOC's own health",
            about: "How DOC's own parts and each of its plugins are, and what is wrong with any.",
            schema: object(json!({}), &[]),
            reads: true,
        },
        Tool {
            name: "doc_api",
            title: "Call a plugin's API",
            about: "Any plugin's api/ route, as you. The route is what comes after api/, such as `search` or `threads/<id>`.",
            schema: object(
                json!({
                    "plugin": text("The plugin's ID, such as resources, water, kb, rbac, dora"),
                    "method": { "type": "string", "enum": ["GET", "POST", "PUT", "PATCH", "DELETE"] },
                    "route": text("The route after api/"),
                    "query": text("A query string, without the ?"),
                    "body": { "description": "A JSON body, for POST, PUT and PATCH" },
                }),
                &["plugin", "method", "route"],
            ),
            reads: false,
        },
        Tool {
            name: "doc_discuss",
            title: "Start a discussion",
            about: "Starts a Watercooler discussion, as you. Tags are kind:name, such as service:payments-api or team:platform.",
            schema: object(
                json!({ "title": text("Its title"), "body": text("Markdown"), "tags": { "type": "array", "items": { "type": "string" } } }),
                &["title", "body"],
            ),
            reads: false,
        },
        Tool {
            name: "doc_notify",
            title: "Notify someone",
            about: "Puts a notification in the inbox of you, of somebody in one of your teams, or of everyone in a team you are in.",
            schema: object(
                json!({ "to": text("`me`, a person's login, or a team's name"), "title": text("One line"), "body": text("A sentence or two"), "url": text("A DOC address it links to") }),
                &["to", "title"],
            ),
            reads: false,
        },
        Tool {
            name: "doc_remind",
            title: "Set a reminder",
            about: "A notification delivered when it is due: to you, somebody in one of your teams, or a team you are in.",
            schema: object(
                json!({ "at": text("When: an RFC 3339 time, YYYY-MM-DD HH:MM in UTC, or `in 3 days`, `in 2 hours`"), "message": text("What to be reminded of"), "to": text("`me` unless said: a login or a team's name"), "url": text("A DOC address it links to") }),
                &["at", "message"],
            ),
            reads: false,
        },
        Tool {
            name: "doc_reminders",
            title: "Your reminders",
            about: "The reminders you set that are still to come.",
            schema: object(json!({}), &[]),
            reads: true,
        },
        Tool {
            name: "doc_cancel_reminder",
            title: "Cancel a reminder",
            about: "Cancels a reminder you set, by its ID.",
            schema: object(json!({ "id": text("The reminder's ID") }), &["id"]),
            reads: false,
        },
        Tool {
            name: "doc_add_person",
            title: "Add somebody",
            about: "Adds somebody to DOC by email address, into a team, if their organisation approved the address's domain. You must lead the team or manage identity.",
            schema: object(
                json!({ "email": text("Their email address"), "name": text("Their name"), "team": text("The team's name, or organisation/team where two share it") }),
                &["email"],
            ),
            reads: false,
        },
        Tool {
            name: "doc_jobs",
            title: "Your jobs",
            about: "The jobs you set Agent Smith, and how each last ran.",
            schema: object(json!({}), &[]),
            reads: true,
        },
        Tool {
            name: "doc_run_job",
            title: "Run a job",
            about: "Runs one of your jobs now, by its ID. Its report arrives in your inbox.",
            schema: object(json!({ "job": text("The job's ID") }), &["job"]),
            reads: false,
        },
    ];
    if jobs {
        tools.push(Tool {
            name: "doc_fetch",
            title: "Read a source",
            about: "GET an address from one of the sources in Agent Smith's settings. Long answers come a page at a time: pass the offset the answer gives to read on.",
            schema: object(json!({ "url": text("The address"), "offset": { "type": "integer", "minimum": 0 } }), &["url"]),
            reads: true,
        });
    }
    tools
}

impl Tool {
    /// As MCP's `tools/list` describes a tool.
    pub fn described(&self) -> Value {
        json!({
            "name": self.name,
            "title": self.title,
            "description": self.about,
            "inputSchema": self.schema,
            "annotations": { "readOnlyHint": self.reads, "openWorldHint": false },
        })
    }
}

/// What a tool answered: text for the agent, and whether it failed.
pub struct Outcome {
    pub text: String,
    pub failed: bool,
}

impl Outcome {
    fn ok(text: impl Into<String>) -> Self {
        Self { text: cut(text.into()), failed: false }
    }

    fn failed(text: impl Into<String>) -> Self {
        Self { text: text.into(), failed: true }
    }

    fn json(value: &Value) -> Self {
        Self::ok(serde_json::to_string_pretty(value).unwrap_or_default())
    }
}

fn cut(text: String) -> String {
    match text.len() > MOST {
        true => {
            let mut end = MOST;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}\n… (cut at {MOST} characters; ask for less)", &text[..end])
        }
        false => text,
    }
}

fn encoded(pairs: &[(&str, &str)]) -> String {
    let mut writing = url::form_urlencoded::Serializer::new(String::new());
    for (name, value) in pairs {
        writing.append_pair(name, value);
    }
    writing.finish()
}

/// A plugin's answer as the agent reads it: the body on success, and why not otherwise.
fn answered(plugin: &str, asked: Result<(u16, Value), doc_plugin_sdk::PluginError>) -> Outcome {
    match asked {
        Ok((status, body)) if (200..300).contains(&status) => Outcome::json(&body),
        Ok((status, body)) => {
            let why =
                body["detail"].as_str().map(str::to_string).unwrap_or_else(|| body.to_string());
            Outcome::failed(format!("{plugin} answered {status}: {why}"))
        }
        Err(err) => Outcome::failed(format!("{plugin} could not be asked: {}", err.detail())),
    }
}

/// A plugin's MCP tool's name as offered here: the plugin's ID first.
pub fn offered_name(plugin: &str, tool: &str) -> String {
    format!("{}_{tool}", plugin.replace('-', "_"))
}

#[derive(Debug, Clone, Deserialize)]
struct Served {
    id: String,
    #[serde(default, deserialize_with = "or_default")]
    display_name: String,
    #[serde(default, deserialize_with = "or_default")]
    state: String,
    #[serde(default, deserialize_with = "or_default")]
    mcp: bool,
}

/// The running plugins that serve MCP, besides this one.
async fn servers(backend: &Backend) -> Vec<Served> {
    let query = Query::new("core.plugins").fields(&["id", "display_name", "state", "mcp"]);
    let found: Vec<Served> = backend.query_all(query).await.unwrap_or_default();
    found
        .into_iter()
        .filter(|served| served.mcp && served.state == "running" && served.id != crate::ID)
        .collect()
}

/// Every tool the person may use: DOC's own and each plugin's, asked of it as them.
pub async fn available(backend: &Backend, jobs: bool) -> Vec<Value> {
    let mut listed: Vec<Value> = own(jobs).iter().map(Tool::described).collect();
    let asking = servers(backend).await.into_iter().map(|served| async move {
        let asked = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });
        let answer = backend.ask(&served.id, "POST", "mcp", None, Some(asked)).await;
        (served, answer)
    });
    for (served, answer) in futures::future::join_all(asking).await {
        let Ok((200, body)) = answer else { continue };
        for tool in body["result"]["tools"].as_array().into_iter().flatten() {
            let Some(inner) = tool["name"].as_str() else { continue };
            let name = offered_name(&served.id, inner);
            if name.len() > 64 {
                continue;
            }
            let from = match served.display_name.is_empty() {
                true => served.id.clone(),
                false => served.display_name.clone(),
            };
            let mut described = tool.clone();
            described["name"] = json!(name);
            described["description"] =
                json!(format!("{from}: {}", tool["description"].as_str().unwrap_or_default()));
            listed.push(described);
        }
    }
    listed
}

/// Where a plugin's tool offered here goes: the plugin whose ID begins its name, the longest
/// such ID winning, and the tool's own name there.
async fn route_of(backend: &Backend, name: &str) -> Option<(String, String)> {
    let mut found: Vec<(String, String)> = servers(backend)
        .await
        .into_iter()
        .filter_map(|served| {
            let prefix = format!("{}_", served.id.replace('-', "_"));
            name.strip_prefix(&prefix).map(|inner| (served.id.clone(), inner.to_string()))
        })
        .collect();
    found.sort_by_key(|(plugin, _)| std::cmp::Reverse(plugin.len()));
    found.into_iter().next()
}

/// Whether a tool only reads, as far as can be told before calling it: DOC's own tools say so,
/// `doc_api` does when it GETs, and a plugin's tool does when that plugin lists it as read-only.
/// Anything else counts as a change, so what cannot be told is refused where only reading is.
pub async fn only_reads(backend: &Backend, name: &str, input: &Value) -> bool {
    if name == "doc_api" {
        return input["method"].as_str().is_some_and(|method| method.eq_ignore_ascii_case("GET"));
    }
    if let Some(tool) = own(true).into_iter().find(|tool| tool.name == name) {
        return tool.reads;
    }
    let Some((plugin, inner)) = route_of(backend, name).await else { return false };
    let asked = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });
    let Ok((200, body)) = backend.ask(&plugin, "POST", "mcp", None, Some(asked)).await else {
        return false;
    };
    body["result"]["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|tool| tool["name"] == json!(inner) && tool["annotations"]["readOnlyHint"] == true)
}

/// Agent Smith's own tools that change something here, which the MCP route, being a read, does
/// not check for itself.
const WRITING: [&str; 4] = ["doc_notify", "doc_remind", "doc_cancel_reminder", "doc_run_job"];

/// One tool, called as whoever this backend acts for; boxed, since the MCP server and the runs
/// made as a job's owner both await it.
pub fn call<'a>(
    backend: &'a Backend,
    name: &'a str,
    input: &'a Value,
) -> std::pin::Pin<Box<dyn Future<Output = Outcome> + Send + 'a>> {
    Box::pin(called(backend, name, input))
}

async fn called(backend: &Backend, name: &str, input: &Value) -> Outcome {
    let text = |key: &str| input[key].as_str().map(str::trim).unwrap_or_default().to_string();
    if WRITING.contains(&name) && !backend.writes() {
        return Outcome::failed(format!("{name} needs plugin:agent:user:rw"));
    }
    match name {
        "doc_search" => {
            let query =
                encoded(&[("q", &text("query")), ("kinds", &text("kinds")), ("limit", "25")]);
            answered(
                "resources",
                backend.ask("resources", "GET", "search", Some(&query), None).await,
            )
        }
        "doc_resource" => {
            let (kind, name) = (text("kind"), text("name"));
            let route = format!("resources/{}/{}", kind.to_lowercase(), name);
            let found =
                answered("resources", backend.ask("resources", "GET", &route, None, None).await);
            if found.failed {
                return found;
            }
            let of = encoded(&[("of", &format!("{kind}:{name}"))]);
            let around = answered(
                "resources",
                backend.ask("resources", "GET", "neighbours", Some(&of), None).await,
            );
            Outcome::ok(format!("{}\n\nConnected to:\n{}", found.text, around.text))
        }
        "doc_readiness" => {
            let service = text("service");
            let query = encoded(&[("service", &service)]);
            let mut said = serde_json::Map::new();
            for plugin in MEASURING {
                let answer = backend.ask(plugin, "GET", "readiness", Some(&query), None).await;
                let value = match answer {
                    Ok((200, body)) => body["services"][&service].clone(),
                    Ok((status, _)) => {
                        json!({ "state": "unknown", "summary": format!("{plugin} answered {status}") })
                    }
                    Err(err) => json!({ "state": "unknown", "summary": err.detail() }),
                };
                said.insert(plugin.to_string(), value);
            }
            Outcome::json(&Value::Object(said))
        }
        "doc_status" => status(backend).await,
        "doc_api" => {
            let plugin = text("plugin");
            let method = text("method").to_uppercase();
            if !matches!(method.as_str(), "GET" | "POST" | "PUT" | "PATCH" | "DELETE") {
                return Outcome::failed("the method is GET, POST, PUT, PATCH or DELETE");
            }
            let route = text("route");
            let route = route.trim_start_matches('/').trim_start_matches("api/");
            let query = Some(text("query")).filter(|query| !query.is_empty());
            let body = Some(input["body"].clone()).filter(|body| !body.is_null());
            answered(&plugin, backend.ask(&plugin, &method, route, query.as_deref(), body).await)
        }
        "doc_discuss" => {
            let tags: Vec<String> = input["tags"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
            let asked = json!({ "title": text("title"), "body": text("body"), "tags": tags });
            answered("water", backend.ask("water", "POST", "threads", None, Some(asked)).await)
        }
        "doc_notify" => notify(backend, input).await,
        "doc_remind" => remind(backend, input).await,
        "doc_reminders" => {
            let Some(me) = person(backend) else {
                return Outcome::failed("reminders are for people");
            };
            let store = Store(backend);
            let waiting =
                store.reminders(json!({ "by": me, "delivered_at": { "is_null": true } })).await;
            match waiting {
                Ok(waiting) => Outcome::json(&json!(waiting.iter().map(|reminder| json!({
                    "id": reminder.id, "at": reminder.at, "message": reminder.message, "to": reminder.to_label,
                })).collect::<Vec<_>>())),
                Err(refusal) => Outcome::failed(refusal.detail),
            }
        }
        "doc_cancel_reminder" => {
            let Some(me) = person(backend) else {
                return Outcome::failed("reminders are for people");
            };
            let store = Store(backend);
            let Ok(id) = text("id").parse::<Uuid>() else {
                return Outcome::failed("that is not a reminder's ID");
            };
            match store.reminder(id).await {
                Ok(reminder) if reminder.by == me => match store.delete(REMINDERS, id).await {
                    Ok(()) => Outcome::ok("Cancelled."),
                    Err(refusal) => Outcome::failed(refusal.detail),
                },
                _ => Outcome::failed("you set no reminder with that ID"),
            }
        }
        "doc_add_person" => add_person(backend, input).await,
        "doc_jobs" => {
            let Some(me) = person(backend) else { return Outcome::failed("jobs are for people") };
            match Store(backend).jobs(Some(me)).await {
                Ok(jobs) => Outcome::json(&json!(jobs.iter().map(|job| json!({
                    "id": job.id, "title": job.title, "playbook": job.playbook, "scope": job.scope,
                    "trigger": job.trigger, "enabled": job.enabled,
                })).collect::<Vec<_>>())),
                Err(refusal) => Outcome::failed(refusal.detail),
            }
        }
        "doc_run_job" => {
            let Some(me) = person(backend) else { return Outcome::failed("jobs are for people") };
            let Ok(id) = text("job").parse::<Uuid>() else {
                return Outcome::failed("that is not a job's ID");
            };
            let login =
                backend.caller().and_then(|caller| caller.label.clone()).unwrap_or_default();
            match Store(backend).job(id).await {
                // Somebody asking for a runbook's job lends it their own access, as Run does.
                Ok(job) if job.owner == me && job.playbook == crate::playbooks::RUNBOOK => {
                    let asked = match crate::jobs::Asker::of(backend) {
                        Ok(asker) => {
                            crate::runbooks::requested(
                                backend,
                                &asker,
                                &job.runbook,
                                "",
                                Some(&job),
                            )
                            .await
                        }
                        Err(refusal) => Err(refusal),
                    };
                    match asked {
                        Ok(run) => Outcome::ok(format!(
                            "Queued as run {}, with your access. Its report will be in your inbox.",
                            run.id
                        )),
                        Err(refusal) => Outcome::failed(refusal.detail),
                    }
                }
                Ok(job) if job.owner == me => match crate::jobs::queue(
                    backend,
                    &job,
                    &format!("asked by {login} through MCP"),
                    None,
                )
                .await
                {
                    Ok(run) => Outcome::ok(format!(
                        "Queued as run {}. Its report will be in your inbox.",
                        run.id
                    )),
                    Err(refusal) => Outcome::failed(refusal.detail),
                },
                _ => Outcome::failed("you have no job with that ID"),
            }
        }
        other => match route_of(backend, other).await {
            Some((plugin, inner)) => {
                let asked = json!({
                    "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                    "params": { "name": inner, "arguments": input },
                });
                match backend.ask(&plugin, "POST", "mcp", None, Some(asked)).await {
                    Ok((200, body)) if body.get("error").is_some() => Outcome::failed(
                        body["error"]["message"].as_str().unwrap_or("the tool failed").to_string(),
                    ),
                    Ok((200, body)) => {
                        let result = &body["result"];
                        let texts: Vec<&str> = result["content"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|part| part["text"].as_str())
                            .collect();
                        let text = match texts.is_empty() {
                            true => result["structuredContent"].to_string(),
                            false => texts.join("\n"),
                        };
                        match result["isError"].as_bool().unwrap_or(false) {
                            true => Outcome::failed(text),
                            false => Outcome::ok(text),
                        }
                    }
                    other => answered(&plugin, other),
                }
            }
            None => Outcome::failed(format!("there is no tool called {other}")),
        },
    }
}

/// The person this call is for, when it is for one.
fn person(backend: &Backend) -> Option<Uuid> {
    let caller = backend.caller()?;
    (caller.kind == "user").then(|| caller.id.as_deref()?.parse().ok())?
}

async fn status(backend: &Backend) -> Outcome {
    #[derive(Deserialize)]
    struct Registered {
        id: String,
        #[serde(default, deserialize_with = "or_default")]
        state: String,
    }
    #[derive(Deserialize)]
    struct Probed {
        plugin: String,
        #[serde(default, deserialize_with = "or_default")]
        state: String,
        #[serde(default)]
        error: Option<String>,
    }
    #[derive(Deserialize)]
    struct Checked {
        name: String,
        #[serde(default, deserialize_with = "or_default")]
        kind: String,
        #[serde(default, deserialize_with = "or_default")]
        state: String,
        #[serde(default)]
        detail: Option<String>,
    }
    // What each plugin is as core registered it, and what the probe last found where it ran.
    let registered: Vec<Registered> = backend
        .query_all(Query::new("core.plugins").fields(&["id", "state"]))
        .await
        .unwrap_or_default();
    let probed: Vec<Probed> =
        backend.query_all(Query::new("core.plugin-status")).await.unwrap_or_default();
    let since = (Utc::now() - Duration::minutes(15)).to_rfc3339();
    let checks: Vec<Checked> = backend
        .query(
            Query::new("core.status-history").filter(json!({ "at": { "gte": since } })).limit(500),
        )
        .await
        .map(|page| page.records)
        .unwrap_or_default();
    let mut parts: BTreeMap<String, Value> = BTreeMap::new();
    for check in checks {
        let said = json!({ "kind": check.kind, "state": check.state, "detail": check.detail });
        parts.insert(check.name.clone(), said);
    }
    // A plugin that has never registered is only configured here, so there is nothing to say.
    let plugins: Vec<Value> = registered
        .iter()
        .filter(|plugin| !plugin.state.is_empty())
        .map(|plugin| {
            let found = probed.iter().find(|probe| probe.plugin == plugin.id);
            json!({
                "plugin": plugin.id,
                "state": found.map_or(plugin.state.as_str(), |probe| probe.state.as_str()),
                "error": found.and_then(|probe| probe.error.clone()),
            })
        })
        .collect();
    Outcome::json(&json!({ "parts": parts, "plugins": plugins }))
}

/// Teams and people, as core has them, for working out whom a notification may go to.
struct People {
    teams: Vec<(Uuid, String, Option<Uuid>, Option<Uuid>)>,
    logins: BTreeMap<Uuid, String>,
    members: Vec<(Uuid, Uuid)>,
}

impl People {
    async fn read(backend: &Backend) -> Self {
        #[derive(Deserialize)]
        struct Team {
            id: Uuid,
            name: String,
            #[serde(default)]
            parent_id: Option<Uuid>,
            #[serde(default)]
            lead_id: Option<Uuid>,
        }
        #[derive(Deserialize)]
        struct Person {
            id: Uuid,
            login: String,
        }
        #[derive(Deserialize)]
        struct Member {
            team_id: Uuid,
            user_id: Uuid,
        }
        let teams: Vec<Team> = backend
            .query_all(Query::new("core.teams").fields(&["id", "name", "parent_id", "lead_id"]))
            .await
            .unwrap_or_default();
        let people: Vec<Person> = backend
            .query_all(Query::new("core.users").fields(&["id", "login"]))
            .await
            .unwrap_or_default();
        let members: Vec<Member> = backend
            .query_all(Query::new("core.team-members").fields(&["team_id", "user_id"]))
            .await
            .unwrap_or_default();
        Self {
            teams: teams
                .into_iter()
                .map(|team| (team.id, team.name, team.parent_id, team.lead_id))
                .collect(),
            logins: people.into_iter().map(|person| (person.id, person.login)).collect(),
            members: members.into_iter().map(|member| (member.team_id, member.user_id)).collect(),
        }
    }

    fn teams_of(&self, user: Uuid) -> Vec<Uuid> {
        self.members.iter().filter(|(_, member)| *member == user).map(|(team, _)| *team).collect()
    }

    fn members_of(&self, team: Uuid) -> Vec<Uuid> {
        self.members.iter().filter(|(held, _)| *held == team).map(|(_, user)| *user).collect()
    }

    /// Whether `user` leads the team or one above it.
    fn leads(&self, user: Uuid, team: Uuid) -> bool {
        let mut next = Some(team);
        let mut seen = 0;
        while let (Some(at), true) = (next, seen < 64) {
            let Some((_, _, parent, lead)) = self.teams.iter().find(|(id, ..)| *id == at) else {
                break;
            };
            if *lead == Some(user) {
                return true;
            }
            next = *parent;
            seen += 1;
        }
        false
    }

    /// Whom `to` names, if `me` may reach them: themselves, somebody sharing a team with them or
    /// in a team they lead, or everyone in a team they are in or lead.
    fn reach(&self, me: Uuid, to: &str) -> Result<(Vec<Uuid>, String), String> {
        let to = to.trim();
        let mine = self.teams_of(me);
        if to.is_empty() || to == "me" || self.logins.get(&me).is_some_and(|login| login == to) {
            return Ok((vec![me], "you".into()));
        }
        if let Some((id, _)) = self.logins.iter().find(|(_, login)| login.eq_ignore_ascii_case(to))
        {
            let theirs = self.teams_of(*id);
            let near = theirs.iter().any(|team| mine.contains(team) || self.leads(me, *team));
            return match near {
                true => Ok((vec![*id], to.to_string())),
                false => {
                    Err(format!("{to} is in none of your teams, so they are not yours to notify"))
                }
            };
        }
        let named: Vec<&(Uuid, String, Option<Uuid>, Option<Uuid>)> =
            self.teams.iter().filter(|(_, name, ..)| name.eq_ignore_ascii_case(to)).collect();
        match named.as_slice() {
            [] => Err(format!("there is nobody and no team called {to}")),
            [(team, name, ..)] if mine.contains(team) || self.leads(me, *team) => {
                Ok((self.members_of(*team), format!("everyone in {name}")))
            }
            [_] => Err(format!("you are not in {to}, so it is not yours to notify")),
            _ => Err(format!("more than one team is called {to}")),
        }
    }
}

async fn notify(backend: &Backend, input: &Value) -> Outcome {
    let Some(me) = person(backend) else {
        return Outcome::failed("notifications are sent for people");
    };
    let title = input["title"].as_str().unwrap_or_default().trim();
    if title.is_empty() {
        return Outcome::failed("give it a title");
    }
    let people = People::read(backend).await;
    let (to, label) = match people.reach(me, input["to"].as_str().unwrap_or("me")) {
        Ok(found) => found,
        Err(why) => return Outcome::failed(why),
    };
    let body = input["body"].as_str().unwrap_or_default();
    let url = input["url"].as_str().filter(|url| url.starts_with('/'));
    for user in &to {
        if let Err(err) = backend.notify(&user.to_string(), title, body, url).await {
            return Outcome::failed(format!("the notification was not sent: {}", err.detail()));
        }
    }
    Outcome::ok(format!("Sent to {label}."))
}

/// When a reminder is for: a time, a time written plainly, or a while from now.
pub fn due(text: &str) -> Result<DateTime<Utc>, String> {
    let text = text.trim();
    if let Ok(at) = DateTime::parse_from_rfc3339(text) {
        return Ok(at.with_timezone(&Utc));
    }
    for format in ["%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M"] {
        if let Ok(at) = NaiveDateTime::parse_from_str(text, format) {
            return Ok(at.and_utc());
        }
    }
    let words: Vec<&str> = text.split_whitespace().collect();
    if let ["in", count, unit] = words.as_slice()
        && let Ok(count) = count.parse::<i64>()
    {
        let span = match unit.trim_end_matches('s') {
            "minute" => Duration::minutes(count),
            "hour" => Duration::hours(count),
            "day" => Duration::days(count),
            "week" => Duration::weeks(count),
            _ => return Err(format!("{unit} is not minutes, hours, days or weeks")),
        };
        return Ok(Utc::now() + span);
    }
    Err(format!("`{text}` is not a time: write 2026-10-01 09:00, or in 3 days"))
}

async fn remind(backend: &Backend, input: &Value) -> Outcome {
    let Some(me) = person(backend) else { return Outcome::failed("reminders are for people") };
    let message = input["message"].as_str().unwrap_or_default().trim();
    if message.is_empty() || message.chars().count() > 1_000 {
        return Outcome::failed("say what to be reminded of, in at most 1,000 characters");
    }
    let at = match due(input["at"].as_str().unwrap_or_default()) {
        Ok(at) if at > Utc::now() - Duration::minutes(1) => at,
        Ok(_) => return Outcome::failed("that time has passed"),
        Err(why) => return Outcome::failed(why),
    };
    let people = People::read(backend).await;
    let (to, label) = match people.reach(me, input["to"].as_str().unwrap_or("me")) {
        Ok(found) => found,
        Err(why) => return Outcome::failed(why),
    };
    let by_label = people.logins.get(&me).cloned().unwrap_or_default();
    let values = json!({
        "id": Uuid::now_v7(), "at": at, "message": message,
        "url": input["url"].as_str().filter(|url| url.starts_with('/')).unwrap_or_default(),
        "to": to.iter().map(Uuid::to_string).collect::<Vec<_>>(), "to_label": label,
        "by": me, "by_label": by_label,
    });
    match Store(backend).insert::<Reminder>(REMINDERS, values).await {
        Ok(reminder) => Outcome::ok(format!(
            "Set for {} for {label} (reminder {}).",
            reminder.at.format("%-d %b %Y, %H:%M UTC"),
            reminder.id
        )),
        Err(refusal) => Outcome::failed(refusal.detail),
    }
}

async fn add_person(backend: &Backend, input: &Value) -> Outcome {
    #[derive(Deserialize)]
    struct Team {
        id: Uuid,
        name: String,
        organisation_id: Uuid,
    }
    #[derive(Deserialize)]
    struct Organisation {
        id: Uuid,
        name: String,
    }
    let email = input["email"].as_str().unwrap_or_default().trim().to_string();
    let wanted = input["team"].as_str().unwrap_or_default().trim();
    let team = match wanted {
        "" => None,
        wanted => {
            let teams: Vec<Team> = backend
                .query_all(Query::new("core.teams").fields(&["id", "name", "organisation_id"]))
                .await
                .unwrap_or_default();
            let organisations: Vec<Organisation> = backend
                .query_all(Query::new("core.organisations").fields(&["id", "name"]))
                .await
                .unwrap_or_default();
            let (organisation, name) = match wanted.split_once('/') {
                Some((organisation, name)) => (Some(organisation), name),
                None => (None, wanted),
            };
            let matching: Vec<&Team> = teams
                .iter()
                .filter(|team| team.name.eq_ignore_ascii_case(name))
                .filter(|team| {
                    organisation.is_none_or(|named| {
                        organisations.iter().any(|found| {
                            found.id == team.organisation_id
                                && found.name.eq_ignore_ascii_case(named)
                        })
                    })
                })
                .collect();
            match matching.as_slice() {
                [one] => Some(one.id),
                [] => return Outcome::failed(format!("there is no team called {wanted}")),
                _ => {
                    return Outcome::failed(format!(
                        "more than one team is called {wanted}: write organisation/team"
                    ));
                }
            }
        }
    };
    let asked = PersonRequest {
        email,
        name: input["name"].as_str().map(str::to_string).filter(|name| !name.trim().is_empty()),
        team,
        organisation: None,
        login: None,
    };
    match backend.add_person(asked).await {
        Ok(answer) => {
            let login = answer["user"]["login"].as_str().unwrap_or("them");
            let said = match (answer["created"].as_bool(), answer["login"]["emailed"].as_bool()) {
                (Some(false), _) => format!("{login} was here already, and is in the team now."),
                (_, Some(true)) => format!("{login} is added, and was emailed a link to sign in."),
                _ => format!(
                    "{login} is added, but no email could be sent, so they cannot sign in yet: an administrator gives them a one-time password from their Accounts on People."
                ),
            };
            Outcome::ok(said)
        }
        Err(err) => Outcome::failed(err.detail()),
    }
}
