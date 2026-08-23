//! The pages at `/p/vacuum/...`, for administrators: every run, a form to start one, each run with
//! the instructions its LLM follows and everything it staged to approve or reject, and each item as
//! it would be written.

use askama::Template;
use chrono::Utc;
use doc_plugin_sdk::protocol::calls::ScopedTokenRequest;
use doc_plugin_sdk::{Backend, Request, Response};
use uuid::Uuid;

use crate::Refusal;
use crate::api::{self, caller, id_of};
use crate::apply;
use crate::claude::Worker;
use crate::instructions::Instructions;
use crate::settings::Config;
use crate::store::{
    AGENT, APPLIED, CANCELLED, CATALOGUE, CLAUDE, COLLECTING, Item, KB, NOT_APPLIED, REJECTED, Run,
    SOURCES, STAGED, Store, WATER,
};

/// What an administrator's own agent may do with the token a run gives it: hand items in to this
/// plugin, and read what DOC already has so it adds to it rather than duplicating it.
const SCOPES: [&str; 4] = [
    "plugin:vacuum:user:rw",
    "plugin:kb:user:ro",
    "plugin:resources:user:ro",
    "plugin:water:user:ro",
];

#[derive(Default)]
pub struct Flash {
    pub notice: Option<String>,
    pub error: Option<String>,
}

impl Flash {
    fn notice(text: impl Into<String>) -> Self {
        Self { notice: Some(text.into()), error: None }
    }

    fn error(text: impl Into<String>) -> Self {
        Self { notice: None, error: Some(text.into()) }
    }
}

fn render<T: Template>(page: &T) -> Result<String, Refusal> {
    page.render().map_err(|err| Refusal::unavailable(format!("the page could not be drawn: {err}")))
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

type Form = Vec<(String, String)>;

fn form(request: &Request) -> Form {
    url::form_urlencoded::parse(&request.body).into_owned().collect()
}

fn field(form: &Form, name: &str) -> String {
    form.iter().find(|(key, _)| key == name).map(|(_, value)| value.clone()).unwrap_or_default()
}

fn fields(form: &Form, name: &str) -> Vec<String> {
    form.iter().filter(|(key, _)| key == name).map(|(_, value)| value.clone()).collect()
}

/// Everything here is an administrator's.
fn admin(backend: &Backend) -> Result<(Uuid, String), Refusal> {
    let (user, admin) = caller(backend)?;
    let login = backend.caller().and_then(|caller| caller.label.clone()).unwrap_or_default();
    match (user, admin) {
        (Some(user), true) => Ok((user, login)),
        _ => Err(Refusal::forbidden("the Data Vacuum is for administrators")),
    }
}

fn source_label(name: &str) -> &'static str {
    SOURCES.iter().find(|(known, _)| *known == name).map_or("", |(_, label)| label)
}

fn state_badge(state: &str) -> &'static str {
    match state {
        COLLECTING => "loading",
        "review" => "degraded",
        "done" | APPLIED => "up",
        CANCELLED | "failed" | REJECTED => "down",
        _ => "unknown",
    }
}

fn state_words(state: &str) -> &'static str {
    match state {
        COLLECTING => "Taking items in",
        "review" => "Waiting for review",
        "done" => "Done",
        CANCELLED => "Cancelled",
        "failed" => "Failed",
        STAGED => "Waiting",
        "approved" => "Approved",
        APPLIED => "Written",
        REJECTED => "Rejected",
        _ => "Not written",
    }
}

fn destination_words(destination: &str) -> &'static str {
    match destination {
        KB => "Knowledge Base",
        CATALOGUE => "Catalogue",
        WATER => "Watercooler",
        _ => "",
    }
}

pub struct RunRow {
    pub id: String,
    pub title: String,
    pub way: &'static str,
    pub sources: String,
    pub state: &'static str,
    pub badge: &'static str,
    pub by: String,
    pub when: String,
}

#[derive(Template)]
#[template(path = "home.html")]
struct Home {
    flash: Flash,
    runs: Vec<RunRow>,
}

/// Starting a run, on a page of its own, with what was typed when it was refused.
#[derive(Template)]
#[template(path = "run_new.html")]
struct NewRun {
    flash: Flash,
    sources: Vec<(&'static str, &'static str)>,
    claude: bool,
    form: Form,
}

impl NewRun {
    fn value(&self, name: &str) -> String {
        field(&self.form, name)
    }

    fn ticked(&self, source: &str) -> bool {
        fields(&self.form, "source").iter().any(|chosen| chosen == source)
    }

    /// Whether Claude drives it: only when it was chosen and can be.
    fn by_claude(&self) -> bool {
        self.claude && field(&self.form, "mode") == CLAUDE
    }
}

async fn new_run(backend: &Backend, form: &Form, flash: Flash) -> Result<String, Refusal> {
    admin(backend)?;
    let config = Config::read(&backend.settings());
    render(&NewRun {
        flash,
        sources: SOURCES.to_vec(),
        claude: config.api_key.is_some(),
        form: form.clone(),
    })
}

async fn home(backend: &Backend, flash: Flash) -> Result<String, Refusal> {
    admin(backend)?;
    let runs = Store(backend)
        .runs()
        .await?
        .into_iter()
        .map(|run| RunRow {
            id: run.id.to_string(),
            title: run.title.clone(),
            way: if run.mode == CLAUDE { "Claude, in DOC" } else { "Your own agent" },
            sources: run
                .sources
                .iter()
                .map(|name| source_label(name))
                .collect::<Vec<_>>()
                .join(", "),
            state: state_words(&run.state),
            badge: state_badge(&run.state),
            by: run.created_by.clone(),
            when: run
                .created_at
                .map(|at| at.format("%-d %b %Y %H:%M").to_string())
                .unwrap_or_default(),
        })
        .collect();
    render(&Home { flash, runs })
}

pub struct ItemRow {
    pub id: String,
    pub destination: &'static str,
    pub title: String,
    pub key: String,
    pub source: &'static str,
    pub state: &'static str,
    pub badge: &'static str,
    pub error: String,
    pub open: bool,
}

#[derive(Template)]
#[template(path = "run.html")]
struct RunPage {
    flash: Flash,
    run: Run,
    way: &'static str,
    sources: String,
    state: &'static str,
    badge: &'static str,
    collecting: bool,
    reviewing: bool,
    agent: bool,
    token_until: String,
    fresh: bool,
    instructions: String,
    items: Vec<ItemRow>,
    waiting: usize,
}

async fn run_page(
    backend: &Backend,
    id: &str,
    flash: Flash,
    token: Option<(&str, chrono::DateTime<Utc>)>,
) -> Result<String, Refusal> {
    admin(backend)?;
    let store = Store(backend);
    let run = store
        .run(id_of(id, "run")?)
        .await?
        .ok_or_else(|| Refusal::missing("there is no such run"))?;
    let config = Config::read(&backend.settings());
    let instructions = match token {
        Some((token, until)) => Instructions::new(&run, &config, Some(token), Some(until)),
        None => Instructions::new(&run, &config, None, run.token_expires_at),
    }
    .text();
    let items: Vec<ItemRow> = store
        .items(run.id)
        .await?
        .into_iter()
        .map(|item: Item| ItemRow {
            id: item.id.to_string(),
            destination: destination_words(&item.destination),
            open: [STAGED, NOT_APPLIED].contains(&item.state.as_str()),
            title: item.title,
            key: item.key,
            source: source_label(&item.source),
            state: state_words(&item.state),
            badge: state_badge(&item.state),
            error: item.error.unwrap_or_default(),
        })
        .collect();
    let page = RunPage {
        flash,
        way: if run.mode == CLAUDE { "Claude, driven by DOC" } else { "Your own agent" },
        sources: run.sources.iter().map(|name| source_label(name)).collect::<Vec<_>>().join(", "),
        state: state_words(&run.state),
        badge: state_badge(&run.state),
        collecting: run.state == COLLECTING,
        reviewing: run.state == "review",
        agent: run.mode == AGENT,
        token_until: run
            .token_expires_at
            .filter(|until| *until > Utc::now())
            .map(|until| until.format("%-d %b %Y %H:%M UTC").to_string())
            .unwrap_or_default(),
        fresh: token.is_some(),
        instructions,
        waiting: items.iter().filter(|item| item.open).count(),
        items,
        run,
    };
    render(&page)
}

/// Mints a new token for the run's agent, revoking the last one, and answers it with its end.
async fn token_for(
    backend: &Backend,
    run: &mut Run,
    minutes: i64,
) -> Result<(String, chrono::DateTime<Utc>), Refusal> {
    api::revoke(backend, run).await;
    let asked = ScopedTokenRequest {
        name: format!("Data Vacuum: {}", run.title).chars().take(100).collect(),
        scopes: SCOPES.iter().map(|scope| (*scope).to_string()).collect(),
        expires_in_minutes: Some(minutes),
    };
    let minted = backend.scoped_token(asked).await.map_err(|err| {
        Refusal::unavailable(format!("no token could be made for the agent: {err}"))
    })?;
    run.token_id = Some(minted.id);
    run.token_expires_at = Some(minted.expires_at);
    Ok((minted.token.expose().clone(), minted.expires_at))
}

async fn create(
    backend: &Backend,
    worker: &Worker,
    request: &Request,
    moved: &mut Option<String>,
) -> Result<String, Refusal> {
    let (user, login) = admin(backend)?;
    let form = form(request);
    let config = Config::read(&backend.settings());
    let title = field(&form, "title").trim().to_string();
    if title.is_empty() || title.chars().count() > 100 {
        let flash = Flash::error("Give the run a title of up to 100 characters.");
        return new_run(backend, &form, flash).await;
    }
    let sources: Vec<String> = fields(&form, "source")
        .into_iter()
        .filter(|name| SOURCES.iter().any(|(known, _)| known == name))
        .collect();
    if sources.is_empty() {
        let flash = Flash::error("Choose at least one source to take from.");
        return new_run(backend, &form, flash).await;
    }
    let mode = match field(&form, "mode").as_str() {
        CLAUDE if config.api_key.is_none() => {
            let flash =
                Flash::error("Claude needs an API key in the Data Vacuum's settings first.");
            return new_run(backend, &form, flash).await;
        }
        CLAUDE => CLAUDE,
        _ => AGENT,
    };
    let mut run = Run {
        id: Uuid::now_v7(),
        title,
        mode: mode.to_string(),
        sources,
        brief: field(&form, "brief").trim().chars().take(10_000).collect(),
        state: COLLECTING.to_string(),
        created_by: login,
        created_by_id: user,
        token_id: None,
        token_expires_at: None,
        summary: String::new(),
        error: None,
        turns: 0,
        created_at: None,
        finished_at: None,
    };
    let token = match mode {
        AGENT => Some(token_for(backend, &mut run, config.token_minutes).await?),
        _ => None,
    };
    Store(backend).save_run(&run).await?;
    let detail = serde_json::json!({ "mode": run.mode, "sources": run.sources });
    if let Err(err) = backend.audit("run.started", Some(&run.id.to_string()), detail).await {
        tracing::warn!(%err, "a new run was not audited");
    }
    *moved = Some(format!("/p/vacuum/runs/{}", run.id));
    match &token {
        Some((token, until)) => {
            let said =
                "Give your agent the instructions below. The token in them is shown this once.";
            run_page(backend, &run.id.to_string(), Flash::notice(said), Some((token, *until))).await
        }
        None => {
            worker.wake();
            let said = "Claude is on it. This page shows what it has staged as it goes.";
            run_page(backend, &run.id.to_string(), Flash::notice(said), None).await
        }
    }
}

async fn found(backend: &Backend, id: &str) -> Result<Run, Refusal> {
    admin(backend)?;
    Store(backend)
        .run(id_of(id, "run")?)
        .await?
        .ok_or_else(|| Refusal::missing("there is no such run"))
}

#[derive(Template)]
#[template(path = "item.html")]
struct ItemPage {
    item: Item,
    run_title: String,
    destination: &'static str,
    state: &'static str,
    badge: &'static str,
}

async fn item_page(backend: &Backend, id: &str) -> Result<String, Refusal> {
    admin(backend)?;
    let store = Store(backend);
    let item = store
        .item(id_of(id, "item")?)
        .await?
        .ok_or_else(|| Refusal::missing("there is no such item"))?;
    let run_title = store.run(item.run).await?.map(|run| run.title).unwrap_or_default();
    render(&ItemPage {
        destination: destination_words(&item.destination),
        state: state_words(&item.state),
        badge: state_badge(&item.state),
        run_title,
        item,
    })
}

fn applied_words(done: &apply::Applied) -> String {
    let mut said = match done.written {
        0 => "Nothing was written".to_string(),
        1 => "1 item was written to DOC".to_string(),
        many => format!("{many} items were written to DOC"),
    };
    if done.failed > 0 {
        said.push_str(&format!("; {} could not be, and say why below", done.failed));
    }
    said.push('.');
    said
}

async fn route(
    backend: &Backend,
    worker: &Worker,
    request: &Request,
    path: &[&str],
    moved: &mut Option<String>,
) -> Result<String, Refusal> {
    match (request.method.as_str(), path) {
        ("GET", [] | [""]) => home(backend, Flash::default()).await,
        ("GET", ["runs", "new"]) => new_run(backend, &Form::new(), Flash::default()).await,
        ("POST", ["runs"]) => create(backend, worker, request, moved).await,
        ("GET", ["runs", id]) => run_page(backend, id, Flash::default(), None).await,
        ("POST", ["runs", id, "stop"]) => {
            let run = found(backend, id).await?;
            let login =
                backend.caller().and_then(|caller| caller.label.clone()).unwrap_or_default();
            let said = format!("Stopped by {login} before the LLM finished.");
            let flash = match api::finish(backend, &run, &said).await {
                Ok(_) => Flash::notice("It takes nothing more in. Review what it staged below."),
                Err(refusal) => Flash::error(refusal.detail),
            };
            run_page(backend, id, flash, None).await
        }
        ("POST", ["runs", id, "cancel"]) => {
            let mut run = found(backend, id).await?;
            if [COLLECTING, "review"].contains(&run.state.as_str()) {
                api::revoke(backend, &mut run).await;
                run.state = CANCELLED.to_string();
                run.finished_at = Some(Utc::now());
                Store(backend).save_run(&run).await?;
            }
            run_page(backend, id, Flash::notice("Cancelled: nothing more of it is written."), None)
                .await
        }
        ("POST", ["runs", id, "token"]) => {
            let mut run = found(backend, id).await?;
            if run.mode != AGENT || run.state != COLLECTING {
                return run_page(
                    backend,
                    id,
                    Flash::error(
                        "Only a run for your own agent that is still taking items in has a token.",
                    ),
                    None,
                )
                .await;
            }
            let minutes = Config::read(&backend.settings()).token_minutes;
            let (token, until) = token_for(backend, &mut run, minutes).await?;
            Store(backend).save_run(&run).await?;
            let said = "A new token is in the instructions below, shown this once; the last one no longer works.";
            run_page(backend, id, Flash::notice(said), Some((&token, until))).await
        }
        ("POST", ["runs", id, "approve"]) => {
            let run = found(backend, id).await?;
            let form = form(request);
            let chosen = fields(&form, "item");
            let everything = !field(&form, "all").is_empty();
            let asked = (!everything).then_some(chosen.as_slice());
            let flash = match apply::approve(backend, &run, asked).await {
                Ok(done) => Flash::notice(applied_words(&done)),
                Err(refusal) => Flash::error(refusal.detail),
            };
            run_page(backend, id, flash, None).await
        }
        ("POST", ["runs", id, "reject"]) => {
            let run = found(backend, id).await?;
            let chosen = fields(&form(request), "item");
            let flash = match apply::reject(backend, &run, &chosen).await {
                Ok(done) => {
                    Flash::notice(format!("{} rejected: none of it is written.", done.rejected))
                }
                Err(refusal) => Flash::error(refusal.detail),
            };
            run_page(backend, id, flash, None).await
        }
        ("GET", ["items", id]) => item_page(backend, id).await,
        _ => Err(Refusal::missing("there is no such page")),
    }
}

#[derive(Template)]
#[template(path = "blank.html")]
struct Blank {
    flash: Flash,
}

pub async fn handle(
    backend: &Backend,
    worker: &Worker,
    request: &Request,
    path: &[&str],
) -> Response {
    let mut moved = None;
    match route(backend, worker, request, path, &mut moved).await {
        Ok(html) => match moved {
            Some(url) => Response::html(html).with_header("hx-push-url", &url),
            None => Response::html(html),
        },
        Err(refusal) => {
            let page = Blank { flash: Flash::error(refusal.detail.clone()) };
            let html = page.render().unwrap_or_else(|_| escape(&refusal.detail));
            Response::new(refusal.status, "text/html; charset=utf-8", html)
        }
    }
}
