//! The pages at `/p/dns/...`: whether the server is answering and what it has done, the domains
//! and their records, and for writers, forms to add, change and remove records.

use askama::Template;
use doc_plugin_sdk::{Backend, Request, Response};

use crate::api::{self, query};
use crate::server::{Server, Status};
use crate::settings::Serving;
use crate::store::{Kind, MAX_TTL, Record, Store, Wanted};
use crate::{Refusal, names};

#[derive(Default)]
pub struct Flash {
    pub notice: Option<String>,
    pub error: Option<String>,
}

impl Flash {
    pub fn done(notice: impl Into<String>) -> Self {
        Self { notice: Some(notice.into()), error: None }
    }

    pub fn refused(detail: impl Into<String>) -> Self {
        Self { notice: None, error: Some(detail.into()) }
    }
}

type Form = Vec<(String, String)>;

fn form(request: &Request) -> Form {
    url::form_urlencoded::parse(&request.body).into_owned().collect()
}

fn field(form: &Form, name: &str) -> String {
    form.iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.trim().to_string())
        .unwrap_or_default()
}

fn render<T: Template>(page: &T) -> Result<String, Refusal> {
    page.render().map_err(|err| Refusal::unavailable(format!("the page could not be drawn: {err}")))
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

pub struct Row {
    pub id: String,
    pub host: String,
    pub kind: &'static str,
    pub value: String,
    /// The value, cut short in the table when it is long, such as a DKIM key.
    pub shown: String,
    pub ttl: String,
    pub note: String,
}

pub struct Domain {
    pub name: String,
    pub rows: Vec<Row>,
}

pub struct Choice {
    pub value: String,
    pub label: String,
    pub selected: bool,
}

#[derive(Template)]
#[template(path = "home.html")]
struct HomePage {
    flash: Flash,
    writes: bool,
    settings: bool,
    state: &'static str,
    headline: String,
    detail: String,
    counts: String,
    upstreams: String,
    forward_for: String,
    /// Where a client that signs in sends its questions, when DNS over HTTPS is on.
    doh: Option<String>,
    /// Where a client that cannot sign in sends them, where this deployment offers that.
    doh_public: Option<String>,
    /// The domain each plugin is named under, where they are named at all.
    plugin_domain: Option<String>,
    /// Where those names point.
    plugin_addresses: String,
    /// The names answered for, one per plugin.
    plugin_names: Vec<String>,
    domains: Vec<Domain>,
    /// Records whose names are in no domain the settings name, as full names.
    unserved: Vec<Row>,
}

#[derive(Template)]
#[template(path = "record_form.html")]
struct RecordForm {
    flash: Flash,
    writes: bool,
    settings: bool,
    /// The record's ID when changing one.
    editing: Option<String>,
    host: String,
    zones: Vec<Choice>,
    kinds: Vec<Choice>,
    shapes: Vec<(&'static str, &'static str)>,
    value: String,
    ttl: String,
    note: String,
    max_ttl: u32,
}

#[derive(Template)]
#[template(path = "blank.html")]
struct Blank {
    flash: Flash,
}

/// How much of a value the table shows before cutting it short.
const SHOWN: usize = 64;

fn row(record: &Record, zone: Option<&str>, default_ttl: u32) -> Row {
    let shown = match record.value.chars().count() > SHOWN {
        true => format!("{}…", record.value.chars().take(SHOWN).collect::<String>()),
        false => record.value.clone(),
    };
    Row {
        id: record.id.to_string(),
        host: zone.map_or_else(|| record.name.clone(), |zone| names::host(&record.name, zone)),
        kind: record.kind.id(),
        value: record.value.clone(),
        shown,
        ttl: match record.ttl {
            Some(ttl) => format!("{ttl}s"),
            None => format!("{default_ttl}s (the default)"),
        },
        note: record.note.clone(),
    }
}

/// Whether the viewer administers DOC, and so gets links to the plugin's settings.
fn sets(backend: &Backend) -> bool {
    backend.caller().is_some_and(|caller| caller.admin)
}

async fn home(backend: &Backend, server: &Server, flash: Flash) -> Result<String, Refusal> {
    let snapshot = server.snapshot();
    let serving = Serving::read(&backend.settings());
    let (state, headline, detail) = match &snapshot.status {
        Status::Off => (
            "unknown",
            "Off".to_string(),
            "Turn on DNS server on this plugin's Features tab to answer DNS questions.".to_string(),
        ),
        Status::OverHttpsOnly => (
            "up",
            "Answering over HTTPS".to_string(),
            "No DNS port is open: questions come over the platform's own HTTPS. Turn on DNS \
             server as well to answer on a port."
                .to_string(),
        ),
        Status::Waiting { listen, problem } => (
            "down",
            format!("Waiting for {listen}"),
            format!("{problem}. It is tried again every two seconds."),
        ),
        Status::Answering { listen, since } => (
            "up",
            format!("Answering on {listen}"),
            format!("Over UDP and TCP since {}.", since.format("%-d %b %Y, %H:%M UTC")),
        ),
    };
    let detail = match (&snapshot.problem, snapshot.ready) {
        (Some(problem), false) => format!(
            "{detail} The records have not been read, so DOC's own domains fail for now: {problem}"
        ),
        (Some(problem), true) => format!(
            "{detail} The records could not be read again, so it answers from the last it read: \
             {problem}"
        ),
        (None, _) => detail,
    };
    let over_https = match snapshot.over_https {
        0 => String::new(),
        1 => ", 1 of them over HTTPS".to_string(),
        many => format!(", {many} of them over HTTPS"),
    };
    let counts = format!(
        "{} answered, {} forwarded, {} refused, {} failed and {} dropped since the plugin \
         started{over_https}",
        snapshot.answered, snapshot.forwarded, snapshot.refused, snapshot.failed, snapshot.dropped
    );
    let upstreams = match serving.upstreams.is_empty() {
        true => "Nowhere: questions about other names are refused".to_string(),
        false => serving.upstreams.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "),
    };
    let forward_for = match serving.forward_for.is_empty() {
        true => "Nobody".to_string(),
        false => serving.forward_for.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "),
    };
    let records = api::records(backend, None).await?;
    let domains = serving
        .zones
        .iter()
        .map(|zone| {
            let mut rows: Vec<Row> = records
                .iter()
                .filter(|record| serving.zone_of(&record.name) == Some(zone.as_str()))
                .map(|record| row(record, Some(zone), serving.ttl))
                .collect();
            // The domain itself first, then its names in order.
            rows.sort_by(|a, b| (a.host != "@", &a.host).cmp(&(b.host != "@", &b.host)));
            Domain { name: zone.clone(), rows }
        })
        .collect();
    let unserved = records
        .iter()
        .filter(|record| serving.zone_of(&record.name).is_none())
        .map(|record| row(record, None, serving.ttl))
        .collect();
    render(&HomePage {
        flash,
        writes: backend.writes(),
        settings: sets(backend),
        state,
        headline,
        detail,
        counts,
        upstreams,
        forward_for,
        plugin_domain: serving.plugin_domain.clone(),
        plugin_addresses: serving
            .plugin_addresses
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", "),
        plugin_names: snapshot.named.clone(),
        doh: serving.over_https.then(|| crate::doh::url(&serving, false)),
        doh_public: serving.over_https.then(|| crate::doh::public_url(&serving)).flatten(),
        domains,
        unserved,
    })
}

fn zone_choices(serving: &Serving, chosen: &str) -> Vec<Choice> {
    serving
        .zones
        .iter()
        .map(|zone| Choice { value: zone.clone(), label: zone.clone(), selected: zone == chosen })
        .collect()
}

fn kind_choices(chosen: &str) -> Vec<Choice> {
    Kind::ALL
        .iter()
        .map(|kind| Choice {
            value: kind.id().to_string(),
            label: kind.id().to_string(),
            selected: kind.id() == chosen,
        })
        .collect()
}

/// What the form holds: `asked` when it is being shown again after a refusal, else `record`.
fn record_form(
    backend: &Backend,
    record: Option<&Record>,
    asked: Option<&Form>,
    flash: Flash,
) -> Result<String, Refusal> {
    let serving = Serving::read(&backend.settings());
    let stored_zone = record.and_then(|record| serving.zone_of(&record.name)).unwrap_or_default();
    let text = |name: &str, stored: String| match asked {
        Some(asked) => field(asked, name),
        None => stored,
    };
    let zone = text("zone", stored_zone.to_string());
    let host = text(
        "host",
        record.map(|record| names::host(&record.name, stored_zone)).unwrap_or_default(),
    );
    let kind =
        text("type", record.map_or_else(|| Kind::A.id().to_string(), |r| r.kind.id().to_string()));
    render(&RecordForm {
        flash,
        writes: backend.writes(),
        settings: sets(backend),
        editing: record.map(|record| record.id.to_string()),
        host: if host == "@" && record.is_none() && asked.is_none() { String::new() } else { host },
        zones: zone_choices(&serving, &zone),
        kinds: kind_choices(&kind),
        shapes: Kind::ALL.iter().map(|kind| (kind.id(), kind.shape())).collect(),
        value: text("value", record.map(|record| record.value.clone()).unwrap_or_default()),
        ttl: text(
            "ttl",
            record.and_then(|record| record.ttl).map(|ttl| ttl.to_string()).unwrap_or_default(),
        ),
        note: text("note", record.map(|record| record.note.clone()).unwrap_or_default()),
        max_ttl: MAX_TTL,
    })
}

/// A record as the form asks for it: the part before the domain and the domain, joined.
fn wanted(asked: &Form) -> Result<Wanted, Refusal> {
    let zone = field(asked, "zone");
    let host = field(asked, "host");
    let host = host.trim_end_matches('.');
    let name = match host {
        "" | "@" => zone.clone(),
        host if zone.is_empty() => host.to_string(),
        host => format!("{host}.{zone}"),
    };
    let ttl = match field(asked, "ttl") {
        ttl if ttl.is_empty() => None,
        ttl => Some(ttl.parse::<u32>().map_err(|_| {
            Refusal::bad(format!("a TTL is a whole number of seconds, up to {MAX_TTL}"))
        })?),
    };
    Ok(Wanted {
        name,
        kind: field(asked, "type"),
        value: field(asked, "value"),
        ttl,
        note: field(asked, "note"),
    })
}

pub async fn handle(
    backend: &Backend,
    server: &Server,
    request: &Request,
    path: &[&str],
) -> Response {
    let mut moved = None;
    match route(backend, server, request, path, &mut moved).await {
        Ok(html) => match moved {
            Some(url) => Response::html(html).with_header("hx-push-url", &url),
            None => Response::html(html),
        },
        Err(refusal) => {
            let page = Blank { flash: Flash::refused(refusal.detail.clone()) };
            let html = page.render().unwrap_or_else(|_| escape(&refusal.detail));
            Response::new(refusal.status, "text/html; charset=utf-8", html)
        }
    }
}

/// `moved` is set to the address a page should be shown at when it is not the one asked for.
async fn route(
    backend: &Backend,
    server: &Server,
    request: &Request,
    path: &[&str],
    moved: &mut Option<String>,
) -> Result<String, Refusal> {
    match (request.method.as_str(), path) {
        ("GET", [] | [""]) => home(backend, server, Flash::default()).await,
        ("GET", ["records", "new"]) => {
            let asked: Form =
                query(request, "zone").map(|zone| ("zone".into(), zone)).into_iter().collect();
            let asked = (!asked.is_empty()).then_some(&asked);
            record_form(backend, None, asked, Flash::default())
        }
        ("POST", ["records"]) => {
            let asked = form(request);
            let added = match wanted(&asked) {
                Ok(wanted) => api::add(backend, server, wanted).await,
                Err(refusal) => Err(refusal),
            };
            match added {
                Ok(record) => {
                    *moved = Some("/p/dns/".into());
                    let notice = format!("{} {} added.", record.name, record.kind.id());
                    home(backend, server, Flash::done(notice)).await
                }
                Err(refusal) if refusal.status >= 500 => Err(refusal),
                Err(refusal) => {
                    record_form(backend, None, Some(&asked), Flash::refused(refusal.detail))
                }
            }
        }
        ("GET", ["records", id]) => {
            let record = Store(backend).found(id).await?;
            record_form(backend, Some(&record), None, Flash::default())
        }
        ("POST", ["records", id]) => {
            let asked = form(request);
            let changed = match wanted(&asked) {
                Ok(wanted) => api::replace(backend, server, id, wanted).await,
                Err(refusal) => Err(refusal),
            };
            match changed {
                Ok(record) => {
                    *moved = Some("/p/dns/".into());
                    let notice = format!("{} {} saved.", record.name, record.kind.id());
                    home(backend, server, Flash::done(notice)).await
                }
                Err(refusal) if refusal.status >= 500 || refusal.status == 404 => Err(refusal),
                Err(refusal) => {
                    let record = Store(backend).found(id).await?;
                    record_form(
                        backend,
                        Some(&record),
                        Some(&asked),
                        Flash::refused(refusal.detail),
                    )
                }
            }
        }
        ("POST", ["records", id, "delete"]) => {
            let record = api::remove(backend, server, id).await?;
            *moved = Some("/p/dns/".into());
            let notice = format!("{} {} {} removed.", record.name, record.kind.id(), record.value);
            home(backend, server, Flash::done(notice)).await
        }
        _ => Err(Refusal::missing("no such page")),
    }
}
