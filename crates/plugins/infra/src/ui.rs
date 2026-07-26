//! The plugin's pages at `/p/infra/...`: your teams' resources, templates and their management, the
//! request form, each request with how far it has got, and a panel for team and service pages.

use std::collections::BTreeMap;

use askama::Template;
use chrono::DateTime;
use doc_plugin_sdk::{Backend, Request as Call, Response};

use crate::Refusal;
use crate::api::{self, NewRequest, Sight, TemplateDetails, query};
use crate::catalog;
use crate::store::{self, Request, Store, Template as Kept};

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

type Form = Vec<(String, String)>;

fn form(request: &Call) -> Form {
    url::form_urlencoded::parse(&request.body).into_owned().collect()
}

fn field(form: &Form, name: &str) -> Option<String> {
    form.iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Every value a form sent for a field, as with checkboxes.
fn fields(form: &Form, name: &str) -> Vec<String> {
    form.iter().filter(|(key, _)| key == name).map(|(_, value)| value.trim().to_string()).collect()
}

fn render<T: Template>(page: &T) -> Result<String, Refusal> {
    page.render().map_err(|err| Refusal::unavailable(format!("the page could not be drawn: {err}")))
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn when(text: &str) -> String {
    DateTime::parse_from_rfc3339(text)
        .map_or_else(|_| text.to_string(), |at| at.format("%a %-d %b %Y, %H:%M UTC").to_string())
}

pub struct Badge {
    pub modifier: &'static str,
    pub text: &'static str,
}

fn badge(status: &str) -> Badge {
    let (modifier, text) = match status {
        "pending" => ("loading", "Pending"),
        "provisioning" => ("loading", "Provisioning"),
        "active" => ("ready", "Active"),
        "expiring" => ("degraded", "Expiring"),
        "deleting" => ("unloading", "Deleting"),
        "deleted" => ("unknown", "Deleted"),
        _ => ("error", "Failed"),
    };
    Badge { modifier, text }
}

pub struct Line {
    pub href: String,
    pub name: String,
    pub what: String,
    pub region: String,
    pub team: String,
    pub environment: String,
    pub badge: Badge,
    pub expires: String,
}

fn line(request: &Request) -> Line {
    let kind = catalog::resource_type(&request.vendor, &request.resource_type);
    Line {
        href: format!("/p/infra/requests/{}", request.id),
        name: request.name.clone(),
        what: format!(
            "{} {}",
            catalog::vendor_name(&request.vendor),
            kind.map_or(request.resource_type.as_str(), |kind| kind.title)
        ),
        region: request.region.clone(),
        team: request.team.clone(),
        environment: request.environment.clone(),
        badge: badge(&request.status),
        expires: when(&request.expires_at),
    }
}

pub struct Offer {
    pub name: String,
    pub title: String,
    pub what: String,
    pub regions: String,
    pub sizes: String,
    pub lifetime: String,
    pub quota: i64,
    pub allowed: bool,
    pub description: String,
}

fn offer(backend: &Backend, template: &Kept) -> Offer {
    let kind = catalog::resource_type(&template.vendor, &template.resource_type);
    Offer {
        name: template.name.clone(),
        title: template.title.clone(),
        what: format!(
            "{} {}",
            catalog::vendor_name(&template.vendor),
            kind.map_or(template.resource_type.as_str(), |kind| kind.title)
        ),
        regions: template.regions.join(", "),
        sizes: template.sizes.join(", "),
        lifetime: format!(
            "{}, at most {}",
            api::lifetime(template.default_lifetime_minutes),
            api::lifetime(template.max_lifetime_minutes)
        ),
        quota: template.team_quota,
        allowed: api::may_request(backend, &template.vendor),
        description: template.description.clone(),
    }
}

pub struct Choice {
    pub value: String,
    pub label: String,
    pub selected: bool,
}

pub struct Step {
    pub title: &'static str,
    pub state: &'static str,
    pub meta: String,
}

#[derive(Template)]
#[template(path = "blank.html")]
struct Blank {
    flash: Flash,
}

#[derive(Template)]
#[template(path = "home.html")]
struct HomePage {
    flash: Flash,
    /// Which infrastructure this page shows: `development` or `production`.
    environment: String,
    lines: Vec<Line>,
    offers: Vec<Offer>,
}

#[derive(Template)]
#[template(path = "templates.html")]
struct TemplatesPage {
    flash: Flash,
    manages: bool,
    offers: Vec<Offer>,
}

#[derive(Template)]
#[template(path = "template_form.html")]
struct TemplateForm {
    flash: Flash,
    action: String,
    heading: String,
    new: bool,
    what: String,
    size_means: String,
    kind: String,
    /// What a new template could offer; choosing one draws the form again for it.
    kinds: Vec<Choice>,
    regions: Vec<Choice>,
    sizes: Vec<Choice>,
    fields: BTreeMap<String, String>,
}

#[derive(Template)]
#[template(path = "request_form.html")]
struct RequestForm {
    flash: Flash,
    offer: Offer,
    regions: Vec<Choice>,
    sizes: Vec<Choice>,
    teams: Vec<Choice>,
    fields: BTreeMap<String, String>,
}

#[derive(Template)]
#[template(path = "request.html")]
struct RequestPage {
    flash: Flash,
    request: Request,
    line: Line,
    size: String,
    steps: Vec<Step>,
    detail: String,
    owns: bool,
    resource_name: String,
}

#[derive(Template)]
#[template(path = "panel.html")]
struct PanelFragment {
    lines: Vec<Line>,
}

pub async fn handle(backend: &Backend, request: &Call, path: &[&str]) -> Response {
    let fragment = matches!(path, ["panel"] | ["dashboard"]);
    let mut moved = None;
    match route(backend, request, path, &mut moved).await {
        Ok(html) => match moved {
            Some(url) => Response::html(html).with_header("hx-push-url", &url),
            None => Response::html(html),
        },
        Err(refusal) if fragment => {
            let text = format!(
                "<p class=\"doc-error-message\" role=\"alert\">{}</p>",
                escape(&refusal.detail)
            );
            Response::new(refusal.status, "text/html; charset=utf-8", text)
        }
        Err(refusal) => {
            let html = Blank { flash: Flash::refused(refusal.detail.clone()) }
                .render()
                .unwrap_or_else(|_| escape(&refusal.detail));
            Response::new(refusal.status, "text/html; charset=utf-8", html)
        }
    }
}

/// `moved` is set to the address a page should be shown at when it is not the one asked for.
async fn route(
    backend: &Backend,
    request: &Call,
    path: &[&str],
    moved: &mut Option<String>,
) -> Result<String, Refusal> {
    let store = Store(backend);
    let mut sight = Sight::new(backend);
    match (request.method.as_str(), path) {
        ("GET", [] | [""]) => home(backend, store::DEVELOPMENT, Flash::default()).await,
        ("GET", [name]) if store::ENVIRONMENTS.contains(name) => {
            home(backend, name, Flash::default()).await
        }
        ("GET", ["templates"]) => templates(backend, Flash::default()).await,
        ("GET", ["templates", "new"]) => {
            // Without a kind, the first there is.
            let first = catalog::TYPES.first().map(|kind| format!("{}/{}", kind.vendor, kind.name));
            let kind = query(request, "kind").or(first).unwrap_or_default();
            template_form(None, &kind, BTreeMap::new(), Flash::default())
        }
        ("POST", ["templates"]) => {
            let asked = form(request);
            let kind = field(&asked, "kind").unwrap_or_default();
            let (vendor, resource_type) = kind.split_once('/').unwrap_or_default();
            let details = template_details(&asked, Some((vendor, resource_type)));
            match api::save_template(backend, None, details).await {
                Ok(template) => {
                    *moved = Some("/p/infra/templates".into());
                    templates(backend, Flash::done(format!("Saved {}.", template.title))).await
                }
                Err(refusal) if refusal.status >= 500 => Err(refusal),
                Err(refusal) => {
                    template_form(None, &kind, kept(&asked), Flash::refused(refusal.detail))
                }
            }
        }
        ("GET", ["templates", name, "edit"]) => {
            let template = store
                .template(name)
                .await?
                .ok_or_else(|| Refusal::missing("there is no such template"))?;
            let kind = format!("{}/{}", template.vendor, template.resource_type);
            template_form(Some(&template), &kind, filled(&template), Flash::default())
        }
        ("POST", ["templates", name]) => {
            let template = store
                .template(name)
                .await?
                .ok_or_else(|| Refusal::missing("there is no such template"))?;
            let asked = form(request);
            let kind = format!("{}/{}", template.vendor, template.resource_type);
            match api::save_template(
                backend,
                Some(template.clone()),
                template_details(&asked, None),
            )
            .await
            {
                Ok(saved) => {
                    templates(backend, Flash::done(format!("Saved {}.", saved.title))).await
                }
                Err(refusal) if refusal.status >= 500 => Err(refusal),
                Err(refusal) => template_form(
                    Some(&template),
                    &kind,
                    kept(&asked),
                    Flash::refused(refusal.detail),
                ),
            }
        }
        ("POST", ["templates", name, "delete"]) => {
            let flash = match api::delete_template(backend, name).await {
                Ok(template) => Flash::done(format!("Deleted {}.", template.title)),
                Err(refusal) => Flash::refused(refusal.detail),
            };
            templates(backend, flash).await
        }
        ("GET", ["request"]) => {
            let name =
                query(request, "template").ok_or_else(|| Refusal::bad("choose a template"))?;
            let mut asked = BTreeMap::new();
            asked.insert(
                "environment".to_string(),
                store::environment_of(query(request, "environment").as_deref()),
            );
            request_form(backend, &name, asked, Flash::default()).await
        }
        ("POST", ["requests"]) => {
            let asked = form(request);
            let template = field(&asked, "template").unwrap_or_default();
            let wanted = NewRequest {
                template: template.clone(),
                name: field(&asked, "name").unwrap_or_default(),
                region: field(&asked, "region"),
                size: field(&asked, "size"),
                lifetime: field(&asked, "lifetime"),
                team: field(&asked, "team").unwrap_or_default(),
                service: field(&asked, "service"),
                environment: field(&asked, "environment"),
            };
            match api::request(backend, &mut sight, wanted).await {
                Ok((made, _)) => {
                    *moved = Some(format!("/p/infra/requests/{}", made.id));
                    let notice = "Requested. It is being made in the background; this page shows how far it has got.";
                    request_page(backend, &mut sight, &made.id.to_string(), Flash::done(notice))
                        .await
                }
                Err(refusal) if refusal.status >= 500 => Err(refusal),
                Err(refusal) => {
                    request_form(backend, &template, kept(&asked), Flash::refused(refusal.detail))
                        .await
                }
            }
        }
        ("GET", ["requests", id]) => request_page(backend, &mut sight, id, Flash::default()).await,
        ("POST", ["requests", id, "extend"]) => {
            let by = field(&form(request), "by").unwrap_or_default();
            let flash = match api::extend(backend, &mut sight, id, &by).await {
                Ok(extended) => {
                    Flash::done(format!("It now expires {}.", when(&extended.expires_at)))
                }
                Err(refusal) => Flash::refused(refusal.detail),
            };
            request_page(backend, &mut sight, id, flash).await
        }
        ("POST", ["requests", id, "delete"]) => {
            let flash = match api::delete_early(backend, &mut sight, id).await {
                Ok(_) => Flash::done("It is being deleted."),
                Err(refusal) => Flash::refused(refusal.detail),
            };
            request_page(backend, &mut sight, id, flash).await
        }
        // A person's own resources and their teams', soonest to expire first, on their dashboard.
        ("GET", ["dashboard"]) => {
            let me = api::login(backend)?;
            let mut found = Vec::new();
            for held in store.in_status(&["active", "expiring", "failed"]).await? {
                if held.requester == me || sight.member(&held.team, &me).await {
                    found.push(held);
                }
            }
            found.sort_by(|a, b| a.expires_at.cmp(&b.expires_at));
            let lines = found.iter().take(6).map(line).collect();
            render(&PanelFragment { lines })
        }
        ("GET", ["panel"]) => {
            let asked =
                query(request, "resource").ok_or_else(|| Refusal::bad("name the resource"))?;
            let (kind, name) = asked
                .split_once(':')
                .ok_or_else(|| Refusal::bad("name the resource as Kind:name"))?;
            let kind = kind.to_ascii_lowercase();
            let mut lines = Vec::new();
            for found in store
                .in_status(&["pending", "provisioning", "active", "expiring", "deleting", "failed"])
                .await?
            {
                let belongs = match kind.as_str() {
                    "team" => found.team == name,
                    _ => found.service.as_deref() == Some(name),
                };
                if belongs && api::sees(backend, &mut sight, &found).await {
                    lines.push(line(&found));
                }
            }
            render(&PanelFragment { lines })
        }
        _ => Err(Refusal::missing("no such page")),
    }
}

/// One environment's resources: what is running in it, and what can be asked for.
async fn home(backend: &Backend, environment: &str, flash: Flash) -> Result<String, Refusal> {
    let store = Store(backend);
    let mut sight = Sight::new(backend);
    let mut lines = Vec::new();
    for request in store.requests().await? {
        let theirs = request.environment == environment;
        if theirs && request.status != "deleted" && api::sees(backend, &mut sight, &request).await {
            lines.push(line(&request));
        }
    }
    let offers = store
        .templates()
        .await?
        .iter()
        .map(|template| offer(backend, template))
        .filter(|offer| offer.allowed)
        .collect();
    render(&HomePage { flash, environment: environment.to_string(), lines, offers })
}

async fn templates(backend: &Backend, flash: Flash) -> Result<String, Refusal> {
    let offers =
        Store(backend).templates().await?.iter().map(|template| offer(backend, template)).collect();
    render(&TemplatesPage { flash, manages: api::manages(backend), offers })
}

/// Everything a template can offer, with `chosen` selected.
fn kinds(chosen: &str) -> Vec<Choice> {
    catalog::TYPES
        .iter()
        .map(|kind| {
            let value = format!("{}/{}", kind.vendor, kind.name);
            Choice {
                label: format!("{} {}", catalog::vendor_name(kind.vendor), kind.title),
                selected: value == chosen,
                value,
            }
        })
        .collect()
}

fn kept(form: &Form) -> BTreeMap<String, String> {
    let mut kept: BTreeMap<String, String> = BTreeMap::new();
    for (key, value) in form {
        kept.entry(key.clone())
            .and_modify(|joined| {
                joined.push(',');
                joined.push_str(value);
            })
            .or_insert_with(|| value.clone());
    }
    kept
}

fn filled(template: &Kept) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("title".into(), template.title.clone()),
        ("description".into(), template.description.clone()),
        ("regions".into(), template.regions.join(",")),
        ("sizes".into(), template.sizes.join(",")),
        ("default_region".into(), template.default_region.clone()),
        ("default_size".into(), template.default_size.clone().unwrap_or_default()),
        ("default_lifetime".into(), api::lifetime(template.default_lifetime_minutes)),
        ("max_lifetime".into(), api::lifetime(template.max_lifetime_minutes)),
        ("team_quota".into(), template.team_quota.to_string()),
    ])
}

fn template_details(form: &Form, new: Option<(&str, &str)>) -> TemplateDetails {
    TemplateDetails {
        name: new.and(field(form, "name")),
        title: Some(field(form, "title").unwrap_or_default()),
        description: Some(field(form, "description").unwrap_or_default()),
        vendor: new.map(|(vendor, _)| vendor.to_string()),
        resource_type: new.map(|(_, kind)| kind.to_string()),
        regions: Some(fields(form, "regions")),
        sizes: Some(fields(form, "sizes")),
        default_region: field(form, "default_region"),
        default_size: field(form, "default_size"),
        default_lifetime: field(form, "default_lifetime"),
        max_lifetime: field(form, "max_lifetime"),
        team_quota: field(form, "team_quota").and_then(|quota| quota.parse().ok()),
    }
}

fn template_form(
    template: Option<&Kept>,
    kind: &str,
    fields: BTreeMap<String, String>,
    flash: Flash,
) -> Result<String, Refusal> {
    let (vendor, name) = kind.split_once('/').unwrap_or_default();
    let found = catalog::resource_type(vendor, name)
        .ok_or_else(|| Refusal::bad("choose what the template offers"))?;
    let chosen = |key: &str| -> Vec<String> {
        fields
            .get(key)
            .map(|text| text.split(',').map(str::to_string).collect())
            .unwrap_or_default()
    };
    let (regions, sizes) = (chosen("regions"), chosen("sizes"));
    let choices = |offered: &[&str], picked: &[String]| -> Vec<Choice> {
        let mut all: Vec<String> = offered.iter().map(|value| (*value).to_string()).collect();
        all.extend(
            picked
                .iter()
                .filter(|value| !value.is_empty() && !offered.contains(&value.as_str()))
                .cloned(),
        );
        all.into_iter()
            .map(|value| Choice { selected: picked.contains(&value), label: value.clone(), value })
            .collect()
    };
    let (action, heading) = match template {
        Some(template) => {
            (format!("/p/infra/templates/{}", template.name), format!("Change {}", template.title))
        }
        None => ("/p/infra/templates".to_string(), "New template".to_string()),
    };
    render(&TemplateForm {
        flash,
        action,
        heading,
        new: template.is_none(),
        what: format!("{} {}", catalog::vendor_name(found.vendor), found.title),
        size_means: found.size.unwrap_or_default().to_string(),
        kind: kind.to_string(),
        kinds: match template {
            Some(_) => Vec::new(),
            None => kinds(kind),
        },
        regions: choices(found.regions, &regions),
        sizes: choices(found.sizes, &sizes),
        fields,
    })
}

/// The teams Resource Definitions lists for the viewer, to choose from.
async fn teams(backend: &Backend, chosen: &str) -> Vec<Choice> {
    match backend.ask("resources", "GET", "resources", Some("kind=team&limit=200"), None).await {
        Ok((200, found)) => found
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|team| {
                let name = team["name"].as_str()?.to_string();
                Some(Choice { selected: name == chosen, label: name.clone(), value: name })
            })
            .collect(),
        _ => Vec::new(),
    }
}

async fn request_form(
    backend: &Backend,
    name: &str,
    fields: BTreeMap<String, String>,
    flash: Flash,
) -> Result<String, Refusal> {
    let template = Store(backend)
        .template(name)
        .await?
        .ok_or_else(|| Refusal::missing(format!("there is no template {name}")))?;
    let pick = |key: &str, fallback: &str| {
        fields.get(key).cloned().unwrap_or_else(|| fallback.to_string())
    };
    let region = pick("region", &template.default_region);
    let size = pick("size", template.default_size.as_deref().unwrap_or_default());
    let options = |values: &[String], chosen: &str| -> Vec<Choice> {
        values
            .iter()
            .map(|value| Choice {
                value: value.clone(),
                label: value.clone(),
                selected: value == chosen,
            })
            .collect()
    };
    let mut fields = fields;
    fields
        .entry("lifetime".into())
        .or_insert_with(|| api::lifetime(template.default_lifetime_minutes));
    let team = fields.get("team").cloned().unwrap_or_default();
    render(&RequestForm {
        flash,
        offer: offer(backend, &template),
        regions: options(&template.regions, &region),
        sizes: options(&template.sizes, &size),
        teams: teams(backend, &team).await,
        fields,
    })
}

fn steps(request: &Request) -> Vec<Step> {
    let order = ["pending", "provisioning", "active", "expiring", "deleting", "deleted"];
    let reached = order.iter().position(|status| *status == request.status);
    let titles = ["Requested", "Provisioning", "Active", "Expiry warned", "Deleting", "Deleted"];
    titles
        .iter()
        .enumerate()
        .map(|(index, title)| {
            let state = match (reached, request.status.as_str()) {
                (None, _) if index == 1 => "failed",
                (None, _) if index == 0 => "done",
                (None, _) => "skipped",
                (Some(at), _) if index < at => "done",
                (Some(at), "deleted") if index == at => "done",
                (Some(at), _) if index == at => "running",
                _ => "pending",
            };
            let meta = match (index, state) {
                (0, _) => format!("{} · {}", request.requester, when(&request.created_at)),
                (1, "failed") => request.error.clone().unwrap_or_default(),
                (3, "done" | "running") => {
                    request.warned_at.as_deref().map(when).unwrap_or_default()
                }
                (5, _) => when(&request.expires_at),
                _ => String::new(),
            };
            Step { title, state, meta }
        })
        .collect()
}

async fn request_page(
    backend: &Backend,
    sight: &mut Sight<'_>,
    id: &str,
    flash: Flash,
) -> Result<String, Refusal> {
    let request = api::found(backend, sight, id).await?;
    let me = api::login(backend)?;
    let owns = backend.writes()
        && (api::admin(backend)
            || request.requester == me
            || sight.member(&request.team, &me).await);
    render(&RequestPage {
        flash,
        line: line(&request),
        size: request.size.clone().unwrap_or_else(|| "—".into()),
        steps: steps(&request),
        detail: serde_json::to_string_pretty(&request.detail).unwrap_or_default(),
        resource_name: api::resource_name(&request),
        owns,
        request,
    })
}
