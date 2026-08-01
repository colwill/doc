//! The pages at `/p/flags/…`: every flag and setting, the providers read beside them, and the
//! panel a service's page in the Catalogue shows.

use askama::Template as Page;
use chrono::DateTime;
use doc_plugin_sdk::{Backend, Request, Response};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::model::{
    ANY, KINDS, Kind, MAX_DESCRIPTION, Role, Slot, UPSTREAMS, Upstream, key, scope,
};
use crate::store::{Entry, Provider, Store, Writing};
use crate::{Refusal, api, providers};

#[derive(Default)]
pub struct Flash {
    pub notice: Option<String>,
    pub error: Option<String>,
}

impl Flash {
    fn done(notice: impl Into<String>) -> Self {
        Self { notice: Some(notice.into()), error: None }
    }

    fn refused(detail: impl Into<String>) -> Self {
        Self { notice: None, error: Some(detail.into()) }
    }
}

pub struct Row {
    pub id: Uuid,
    pub key: String,
    pub description: String,
    pub scope: String,
    pub value: String,
    pub kind: &'static str,
    pub boolean: bool,
    pub on: bool,
    pub enabled: bool,
    pub changed: String,
    /// The provider it is read from, for a value DOC does not hold itself: shown here, and changed
    /// where the provider keeps it.
    pub from: Option<String>,
}

pub struct ProviderRow {
    pub id: Uuid,
    pub name: String,
    pub kind: &'static str,
    pub url: String,
    pub precedence: &'static str,
    pub refresh: i64,
    pub applies: String,
    pub enabled: bool,
    pub checked: Option<String>,
    pub problem: Option<String>,
}

#[derive(Page)]
#[template(path = "home.html")]
struct Home {
    flash: Flash,
    writes: bool,
    role: &'static str,
    heading: &'static str,
    lede: &'static str,
    add: &'static str,
    empty: String,
    base_url: &'static str,
    refresh_url: String,
    platform_url: String,
    service: String,
    services: Vec<String>,
    rows: Vec<Row>,
}

#[derive(Page)]
#[template(path = "form.html")]
struct Form {
    flash: Flash,
    role: &'static str,
    heading: String,
    action: String,
    back: &'static str,
    editing: bool,
    key: String,
    description: String,
    service: String,
    environment: String,
    kind: String,
    kinds: Vec<(&'static str, &'static str)>,
    value: String,
    value_hint: &'static str,
    enabled: bool,
    problems: Vec<(String, String)>,
}

impl Form {
    fn problem(&self, field: &str) -> Option<&str> {
        self.problems.iter().find(|(name, _)| name == field).map(|(_, detail)| detail.as_str())
    }
}

/// A field a provider sent that nobody has said is a flag or a setting, and what it held.
pub struct Suggestion {
    pub id: Uuid,
    pub provider: String,
    pub field: String,
    pub held: String,
}

/// A field somebody said what it is, and what they said.
pub struct Adoption {
    pub id: Uuid,
    pub provider: String,
    pub field: String,
    pub taken: &'static str,
}

#[derive(Page)]
#[template(path = "providers.html")]
struct Providers {
    flash: Flash,
    writes: bool,
    rows: Vec<ProviderRow>,
    suggestions: Vec<Suggestion>,
    adoptions: Vec<Adoption>,
}

/// Reading another provider, on a page of its own, with what was typed when it was refused.
#[derive(Page)]
#[template(path = "provider_new.html")]
struct NewProvider {
    flash: Flash,
    kinds: Vec<(&'static str, &'static str)>,
    fields: Fields,
    form: Vec<(String, String)>,
}

impl NewProvider {
    fn value(&self, name: &str) -> String {
        field(&self.form, name)
    }
}

/// The part of the provider form that is its kind's own: where it is and what it is read with.
pub struct Fields {
    pub kind: &'static str,
    pub about: &'static str,
    pub url: String,
    pub placeholder: &'static str,
    pub slots: &'static [Slot],
    pub held: Vec<String>,
    /// The configurations it reads by name, as typed, for a kind that holds them that way.
    pub configs: Option<String>,
}

impl Fields {
    fn of(backend: &Backend, kind: Upstream) -> Self {
        Self {
            kind: kind.as_str(),
            about: kind.about(),
            url: kind.default_url().unwrap_or_default().to_string(),
            placeholder: kind.placeholder(),
            slots: kind.credentials(),
            held: backend.settings().named_names(),
            configs: kind.reads_configs().then(String::new),
        }
    }
}

#[derive(Page)]
#[template(path = "provider_fields.html")]
struct ProviderFields {
    fields: Fields,
}

#[derive(Page)]
#[template(path = "panel.html")]
struct Panel {
    writes: bool,
    service: String,
    rows: Vec<Row>,
}

fn drawn<T: Page>(page: &T) -> Result<Response, Refusal> {
    page.render()
        .map(Response::html)
        .map_err(|err| Refusal::unavailable(format!("the page could not be drawn: {err}")))
}

type Form_ = Vec<(String, String)>;

fn form(request: &Request) -> Form_ {
    url::form_urlencoded::parse(&request.body).into_owned().collect()
}

fn field(form: &Form_, name: &str) -> String {
    form.iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.trim().to_string())
        .unwrap_or_default()
}

fn when(text: &str) -> String {
    DateTime::parse_from_rfc3339(text)
        .map_or_else(|_| text.to_string(), |at| at.format("%-d %b %Y, %H:%M").to_string())
}

fn kinds() -> Vec<(&'static str, &'static str)> {
    KINDS
        .into_iter()
        .map(|kind| {
            (
                kind.as_str(),
                match kind {
                    Kind::Boolean => "On or off",
                    Kind::String => "Some text",
                    Kind::Number => "A number",
                    Kind::Json => "JSON",
                },
            )
        })
        .collect()
}

fn rows(entries: &[Entry]) -> Vec<Row> {
    entries
        .iter()
        .map(|entry| {
            let kind = entry.kind();
            Row {
                id: entry.id,
                key: entry.key.clone(),
                description: entry.description.clone(),
                scope: entry.scope(),
                value: match kind {
                    Kind::Json => entry.value.to_string().chars().take(80).collect(),
                    _ => kind.written(&entry.value),
                },
                kind: kind.as_str(),
                boolean: kind == Kind::Boolean,
                on: entry.value.as_bool().unwrap_or(false),
                enabled: entry.enabled,
                changed: match entry.updated_by.is_empty() {
                    true => when(&entry.updated_at),
                    false => format!("{} · {}", entry.updated_by, when(&entry.updated_at)),
                },
                from: None,
            }
        })
        .collect()
}

/// What the providers hold for a tab, as rows beside DOC's own, read the way a service reads them
/// and from the same cache. A provider that cannot be read adds nothing here; the Providers page
/// says why.
async fn provided(backend: &Backend, role: Role, service: &str) -> Vec<Row> {
    let Ok(all) = Store(backend).providers().await else { return Vec::new() };
    let mut rows = Vec::new();
    for provider in all.iter().filter(|provider| provider.enabled) {
        let for_service = provider.services.is_empty()
            || provider.services.iter().any(|named| named == service || named == ANY);
        if !service.is_empty() && !for_service {
            continue;
        }
        let environment = match provider.environment.as_str() {
            ANY => api::default_environment(backend),
            named => named.to_string(),
        };
        let reading = if service.is_empty() { ANY } else { service };
        let Ok(fetched) = providers::fetch(backend, provider, reading, &environment).await else {
            continue;
        };
        let held = match role {
            Role::Flag => fetched.values.flags,
            Role::Config => fetched.values.config,
        };
        for (key, value) in held {
            let kind = match &value {
                Value::Bool(_) => Kind::Boolean,
                Value::Number(_) => Kind::Number,
                Value::String(_) => Kind::String,
                _ => Kind::Json,
            };
            rows.push(Row {
                id: provider.id,
                key,
                description: String::new(),
                scope: applies(provider),
                value: match kind {
                    Kind::Json => value.to_string().chars().take(80).collect(),
                    _ => kind.written(&value),
                },
                kind: kind.as_str(),
                boolean: kind == Kind::Boolean,
                on: value.as_bool().unwrap_or(false),
                enabled: true,
                changed: format!("Read from {} · {}", provider.name, provider.precedence().label()),
                from: Some(provider.name.clone()),
            });
        }
    }
    rows
}

/// What a provider is read for, as the pages write it.
fn applies(provider: &Provider) -> String {
    match (provider.services.is_empty(), provider.environment.as_str()) {
        (true, ANY) => "Every service".to_string(),
        (true, environment) => format!("Every service in {environment}"),
        (false, ANY) => provider.services.join(", "),
        (false, environment) => format!("{} in {environment}", provider.services.join(", ")),
    }
}

pub async fn handle(backend: &Backend, request: Request) -> Response {
    match route(backend, &request).await {
        Ok(response) => response,
        Err(refusal) => refusal.response(),
    }
}

async fn route(backend: &Backend, request: &Request) -> Result<Response, Refusal> {
    let writes = backend.writes();
    let path = request.path.trim_start_matches("ui").trim_matches('/');
    let parts: Vec<&str> = match path.is_empty() {
        true => Vec::new(),
        false => path.split('/').collect(),
    };
    match (request.method.as_str(), parts.as_slice()) {
        ("GET", []) => listing(backend, Role::Flag, request, Flash::default()).await,
        ("GET", ["config"]) => listing(backend, Role::Config, request, Flash::default()).await,
        ("GET", ["rows"]) => {
            // The fragment the table refreshes itself with when anything changes anywhere.
            let role = api::query(request, "role")
                .and_then(|role| Role::named(&role))
                .unwrap_or(Role::Flag);
            listing(backend, role, request, Flash::default()).await
        }
        ("GET", ["new"]) => {
            must_write(writes)?;
            let role = api::query(request, "role")
                .and_then(|role| Role::named(&role))
                .unwrap_or(Role::Flag);
            drawn(&blank(role, api::query(request, "service").unwrap_or_default()))
        }
        ("POST", ["new"]) => saved(backend, request, None).await,
        ("GET", ["e", id]) => {
            let store = Store(backend);
            let entry = api::found(&store, id).await?;
            drawn(&filled(&entry))
        }
        ("POST", ["e", id]) => {
            let store = Store(backend);
            let entry = api::found(&store, id).await?;
            saved(backend, request, Some(entry)).await
        }
        // The one-click change a table offers: a switch is flipped, and nothing else is touched.
        ("POST", ["e", id, "toggle"]) => {
            must_write(writes)?;
            let store = Store(backend);
            let entry = api::found(&store, id).await?;
            if entry.kind() != Kind::Boolean {
                return Err(Refusal::bad("only a switch is turned on and off like that"));
            }
            let on = !entry.value.as_bool().unwrap_or(false);
            store.set(entry.id, json!({ "value": on, "updated_by": crate::who(backend) })).await?;
            api::announce(backend, &entry.key, &entry.service, &entry.environment).await;
            let flash = Flash::done(format!(
                "{} is {} for {}.",
                entry.key,
                if on { "on" } else { "off" },
                entry.scope().to_lowercase()
            ));
            listing(backend, entry.role(), request, flash).await
        }
        ("DELETE", ["e", id]) => {
            must_write(writes)?;
            let store = Store(backend);
            let entry = api::found(&store, id).await?;
            store.remove(entry.id).await?;
            api::announce(backend, &entry.key, &entry.service, &entry.environment).await;
            listing(backend, entry.role(), request, Flash::done(format!("{} is gone.", entry.key)))
                .await
        }
        ("GET", ["providers"]) => providers_page(backend, Flash::default()).await,
        ("GET", ["providers", "new"]) => {
            must_write(writes)?;
            new_provider(backend, Vec::new(), Flash::default())
        }
        // What the form asks for once a kind of provider is chosen.
        ("GET", ["providers", "fields"]) => {
            let kind = api::query(request, "kind")
                .and_then(|kind| Upstream::named(&kind))
                .unwrap_or(UPSTREAMS[0]);
            drawn(&ProviderFields { fields: Fields::of(backend, kind) })
        }
        ("POST", ["providers"]) => {
            must_write(writes)?;
            let submitted = form(request);
            let body = json!({
                "name": field(&submitted, "name"),
                "kind": field(&submitted, "kind"),
                "url": field(&submitted, "url"),
                "environment": field(&submitted, "environment"),
                "services": field(&submitted, "services"),
                "precedence": field(&submitted, "precedence"),
                "credentials": submitted
                    .iter()
                    .filter(|(name, _)| name == "credential")
                    .map(|(_, named)| named.trim())
                    .collect::<Vec<_>>(),
                "configs": field(&submitted, "configs"),
                "refresh_seconds": field(&submitted, "refresh_seconds").parse::<i64>().unwrap_or(60),
            });
            match api::provider_values(&body) {
                Err(refusal) => new_provider(backend, submitted, Flash::refused(refusal.detail)),
                Ok(values) => {
                    let provider = Store(backend).write_provider(values).await?;
                    let said = format!("{} is read beside DOC's own flags.", provider.name);
                    let page = providers_page(backend, Flash::done(said)).await?;
                    Ok(page.with_header("hx-push-url", "/p/flags/providers"))
                }
            }
        }
        ("POST", ["providers", id, "check"]) => {
            must_write(writes)?;
            let store = Store(backend);
            let id =
                Uuid::parse_str(id).map_err(|_| Refusal::bad("a provider is named by its id"))?;
            let provider = store
                .provider(id)
                .await?
                .ok_or_else(|| Refusal::missing("there is no such provider"))?;
            let environment = api::default_environment(backend);
            let flash = match providers::check(backend, &provider, ANY, &environment).await {
                Ok(held) => {
                    let pending = held.pending();
                    let _ = store
                        .set_provider(
                            provider.id,
                            json!({
                                "problem": Value::Null,
                                "checked_at": chrono::Utc::now().to_rfc3339(),
                                "suggested": pending,
                            }),
                        )
                        .await;
                    let names: Vec<&str> = pending.keys().map(String::as_str).collect();
                    Flash::done(match names.is_empty() {
                        true => format!("{} holds {}.", provider.name, held.said()),
                        false => format!(
                            "{} holds {}. It also sends {}, which DOC serves once you say what it is.",
                            provider.name,
                            held.said(),
                            names.join(", ")
                        ),
                    })
                }
                Err(problem) => {
                    let _ = store
                        .set_provider(
                            provider.id,
                            json!({ "problem": problem.clone(), "checked_at": chrono::Utc::now().to_rfc3339() }),
                        )
                        .await;
                    Flash::refused(problem)
                }
            };
            providers_page(backend, flash).await
        }
        ("POST", ["providers", id, "adopt"]) => {
            must_write(writes)?;
            let submitted = form(request);
            let (adopting, taken) = (field(&submitted, "field"), field(&submitted, "as"));
            let flash = match api::adopt(backend, id, &adopting, &taken).await {
                Ok((_, said)) => Flash::done(said),
                Err(refusal) => Flash::refused(refusal.detail),
            };
            providers_page(backend, flash).await
        }
        ("DELETE", ["providers", id]) => {
            must_write(writes)?;
            let id =
                Uuid::parse_str(id).map_err(|_| Refusal::bad("a provider is named by its id"))?;
            Store(backend).remove_provider(id).await?;
            providers_page(backend, Flash::done("It is no longer read.")).await
        }
        // The panel the Catalogue puts on a service's page: every flag that applies to it, in any
        // environment and whether or not it is served, as the Flags tab lists them.
        ("GET", ["panel"]) => {
            let resource = api::query(request, "resource").unwrap_or_default();
            let service = resource
                .split_once(':')
                .map_or(resource.as_str(), |(_, name)| name)
                .to_ascii_lowercase();
            let shown =
                Store(backend).entries(Some(Role::Flag), Some(service.as_str()), None).await?;
            drawn(&Panel { writes, service, rows: rows(&shown) })
        }
        _ => Ok(Response::not_found()),
    }
}

fn must_write(writes: bool) -> Result<(), Refusal> {
    match writes {
        true => Ok(()),
        false => Err(Refusal::forbidden("changing a flag needs plugin:flags:user:rw")),
    }
}

async fn listing(
    backend: &Backend,
    role: Role,
    request: &Request,
    flash: Flash,
) -> Result<Response, Refusal> {
    let store = Store(backend);
    let service = api::query(request, "service").unwrap_or_default().to_ascii_lowercase();
    let entries = store.entries(Some(role), Some(service.as_str()), None).await?;
    let services = store.services().await?;
    let (heading, lede, add, base_url) = match role {
        Role::Flag => (
            "Flags",
            "Switches the services read while they run. A service reads every flag that applies to \
             it in one call, so turning one on here turns it on there within seconds.",
            "New flag",
            "/p/flags/",
        ),
        Role::Config => (
            "Configuration",
            "Values the services read the same way their flags are read: timeouts, limits, \
             addresses — anything that should change without a deployment.",
            "New setting",
            "/p/flags/config",
        ),
    };
    let empty = match service.is_empty() {
        true => format!("There are no {} yet.", heading.to_lowercase()),
        false => format!("Nothing applies to {service} yet."),
    };
    drawn(&Home {
        flash,
        writes: backend.writes(),
        role: role.as_str(),
        heading,
        lede,
        add,
        empty,
        base_url,
        refresh_url: format!("/p/flags/rows?role={}&service={service}", role.as_str()),
        platform_url: platform_url(),
        rows: {
            let mut rows = rows(&entries);
            rows.extend(provided(backend, role, &service).await);
            rows.sort_by(|one, two| {
                one.key.cmp(&two.key).then(one.from.is_some().cmp(&two.from.is_some()))
            });
            rows
        },
        service,
        services,
    })
}

/// Where this DOC is, for the examples on the page. The deployment says so; the examples fall back
/// to the development address rather than pretending there is none.
fn platform_url() -> String {
    std::env::var("DOC_PUBLIC_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".to_string())
}

fn blank(role: Role, service: String) -> Form {
    Form {
        flash: Flash::default(),
        role: role.as_str(),
        heading: match role {
            Role::Flag => "New flag".to_string(),
            Role::Config => "New setting".to_string(),
        },
        action: "/p/flags/new".to_string(),
        back: match role {
            Role::Flag => "/p/flags/",
            Role::Config => "/p/flags/config",
        },
        editing: false,
        key: String::new(),
        description: String::new(),
        service,
        environment: String::new(),
        kind: Kind::Boolean.as_str().to_string(),
        kinds: kinds(),
        value: "false".to_string(),
        value_hint: "What every service reading it is given, unless something more specific applies.",
        enabled: true,
        problems: Vec::new(),
    }
}

fn filled(entry: &Entry) -> Form {
    let kind = entry.kind();
    Form {
        flash: Flash::default(),
        role: entry.role().as_str(),
        heading: entry.key.clone(),
        action: format!("/p/flags/e/{}", entry.id),
        back: match entry.role() {
            Role::Flag => "/p/flags/",
            Role::Config => "/p/flags/config",
        },
        editing: true,
        key: entry.key.clone(),
        description: entry.description.clone(),
        service: match entry.service.as_str() {
            ANY => String::new(),
            service => service.to_string(),
        },
        environment: match entry.environment.as_str() {
            ANY => String::new(),
            environment => environment.to_string(),
        },
        kind: kind.as_str().to_string(),
        kinds: kinds(),
        value: kind.written(&entry.value),
        value_hint: "What every service reading it is given, unless something more specific applies.",
        enabled: entry.enabled,
        problems: Vec::new(),
    }
}

async fn saved(
    backend: &Backend,
    request: &Request,
    editing: Option<Entry>,
) -> Result<Response, Refusal> {
    must_write(backend.writes())?;
    let submitted = form(request);
    let role = Role::named(&field(&submitted, "role")).unwrap_or(Role::Flag);
    let kind = Kind::named(&field(&submitted, "kind")).unwrap_or(Kind::Boolean);
    let mut problems: Vec<(String, String)> = Vec::new();
    let written_key = match editing.as_ref() {
        Some(entry) => entry.key.clone(),
        None => field(&submitted, "key"),
    };
    let checked_key = key(&written_key).unwrap_or_else(|problem| {
        problems.push(("key".into(), problem));
        written_key.clone()
    });
    let service = scope(&field(&submitted, "service"), "a service").unwrap_or_else(|problem| {
        problems.push(("service".into(), problem));
        ANY.to_string()
    });
    let environment =
        scope(&field(&submitted, "environment"), "an environment").unwrap_or_else(|problem| {
            problems.push(("environment".into(), problem));
            ANY.to_string()
        });
    let written_value = field(&submitted, "value");
    let value = kind.read(&written_value).unwrap_or_else(|problem| {
        problems.push(("value".into(), problem));
        Value::Null
    });

    if !problems.is_empty() {
        let mut page = match editing.as_ref() {
            Some(entry) => filled(entry),
            None => blank(role, service.clone()),
        };
        page.key = written_key;
        page.value = written_value;
        page.description = field(&submitted, "description");
        page.environment = field(&submitted, "environment");
        page.service = field(&submitted, "service");
        page.enabled = !field(&submitted, "enabled").is_empty();
        page.problems = problems;
        return drawn(&page);
    }

    let writing = Writing {
        role,
        key: checked_key.clone(),
        service: service.clone(),
        environment: environment.clone(),
        kind,
        value,
        description: field(&submitted, "description").chars().take(MAX_DESCRIPTION).collect(),
        enabled: !field(&submitted, "enabled").is_empty(),
    };
    let store = Store(backend);
    // Moving one to another service or environment is a different record, so the old one goes.
    if let Some(entry) = editing.as_ref()
        && (entry.service != service || entry.environment != environment)
    {
        store.remove(entry.id).await?;
    }
    store.write(&writing, &crate::who(backend)).await?;
    api::announce(backend, &checked_key, &service, &environment).await;
    listing(backend, role, request, Flash::done(format!("{checked_key} is saved."))).await
}

async fn providers_page(backend: &Backend, flash: Flash) -> Result<Response, Refusal> {
    let store = Store(backend);
    let providers = store.providers().await?;
    let rows = providers
        .iter()
        .map(|provider| ProviderRow {
            id: provider.id,
            name: provider.name.clone(),
            kind: provider.kind().map_or("unknown", Upstream::label),
            url: provider.url.clone(),
            precedence: provider.precedence().label(),
            refresh: provider.refresh_seconds,
            applies: applies(provider),
            enabled: provider.enabled,
            checked: provider.checked_at.as_deref().map(|at| format!("Last read {}", when(at))),
            problem: provider.problem.clone(),
        })
        .collect();
    let suggestions = providers
        .iter()
        .flat_map(|provider| {
            provider.suggested.iter().map(|(field, held)| Suggestion {
                id: provider.id,
                provider: provider.name.clone(),
                field: field.clone(),
                held: held.clone(),
            })
        })
        .collect();
    let adoptions = providers
        .iter()
        .flat_map(|provider| {
            provider.adopted.iter().map(|(field, taken)| Adoption {
                id: provider.id,
                provider: provider.name.clone(),
                field: field.clone(),
                taken: match Role::named(taken) {
                    Some(Role::Flag) => "A flag",
                    Some(Role::Config) => "A setting",
                    None => "Ignored",
                },
            })
        })
        .collect();
    drawn(&Providers { flash, writes: backend.writes(), rows, suggestions, adoptions })
}

fn new_provider(
    backend: &Backend,
    form: Vec<(String, String)>,
    flash: Flash,
) -> Result<Response, Refusal> {
    let kind = Upstream::named(&field(&form, "kind")).unwrap_or(UPSTREAMS[0]);
    let mut fields = Fields::of(backend, kind);
    let url = field(&form, "url");
    if !url.is_empty() {
        fields.url = url;
    }
    if let Some(configs) = fields.configs.as_mut() {
        *configs = field(&form, "configs");
    }
    drawn(&NewProvider {
        flash,
        kinds: UPSTREAMS.into_iter().map(|kind| (kind.as_str(), kind.label())).collect(),
        fields,
        form,
    })
}
