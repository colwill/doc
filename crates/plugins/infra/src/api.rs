//! The plugin's routes and what they do. Holders of `templates` manage templates; a request needs
//! `selfservice` or `selfservice-<vendor>`, the template's limits and the team's quota, and then
//! provisioning and teardown run as background tasks, every step audited.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use doc_plugin_sdk::{Backend, Caller, Request as Call, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use url::form_urlencoded::byte_serialize;
use uuid::Uuid;

use crate::Refusal;
use crate::catalog;
use crate::store::{self, Request, Store, Template};
use crate::vendors::{self, Wanted};

type Answer = Result<(u16, Value), Refusal>;

/// Resources are looked at for drift at most this often, unless `DOC_INFRA_DRIFT_MINUTES` says.
const DRIFT_MINUTES: i64 = 15;
const WARN_HOURS: i64 = 24;

pub fn query(request: &Call, key: &str) -> Option<String> {
    url::form_urlencoded::parse(request.query.as_bytes())
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn body<T: for<'de> Deserialize<'de>>(request: &Call) -> Result<T, Refusal> {
    let bytes = if request.body.is_empty() { &b"{}"[..] } else { &request.body[..] };
    serde_json::from_slice(bytes)
        .map_err(|err| Refusal::bad(format!("the body is not what this takes: {err}")))
}

fn encoded(text: &str) -> String {
    byte_serialize(text.as_bytes()).collect::<String>().replace('+', "%20")
}

pub fn login(backend: &Backend) -> Result<String, Refusal> {
    match backend.caller() {
        Some(Caller { kind, id: Some(id), label, .. }) if kind == "user" || kind == "service" => {
            Ok(label.clone().unwrap_or_else(|| id.clone()))
        }
        _ => Err(Refusal::forbidden("infrastructure is for people and service accounts")),
    }
}

pub fn admin(backend: &Backend) -> bool {
    backend.caller().is_some_and(|caller| caller.admin)
}

pub fn manages(backend: &Backend) -> bool {
    backend.allows("templates", true)
}

/// Whether the caller holds a permission that names an ability, at any scope.
fn holds(backend: &Backend, name: &str) -> bool {
    backend.caller().is_some_and(|caller| caller.admin || caller.custom.contains_key(name))
}

/// Whether the caller may request resources from this vendor.
pub fn may_request(backend: &Backend, vendor: &str) -> bool {
    holds(backend, "selfservice") || holds(backend, &format!("selfservice-{vendor}"))
}

pub fn page(path: &str) -> String {
    let base = std::env::var("DOC_PUBLIC_URL").unwrap_or_default();
    format!("{}/p/infra/{path}", base.trim_end_matches('/'))
}

/// `30m`, `12h` or `7d`, in minutes.
pub fn minutes(text: &str) -> Result<i64, Refusal> {
    let text = text.trim();
    let refused = || Refusal::bad(format!("`{text}` is not a lifetime, such as 30m, 12h or 7d"));
    let (number, unit) = text.split_at(text.len().saturating_sub(1));
    let number: i64 = number.parse().map_err(|_| refused())?;
    let minutes = match unit {
        "m" => number,
        "h" => number * 60,
        "d" => number * 24 * 60,
        _ => return Err(refused()),
    };
    match minutes {
        1..=525_600 => Ok(minutes),
        _ => Err(Refusal::bad("a lifetime is a minute to a year")),
    }
}

/// Minutes as the shortest of `30m`, `12h` or `7d` that says them exactly.
pub fn lifetime(minutes: i64) -> String {
    match minutes {
        minutes if minutes % (24 * 60) == 0 => format!("{}d", minutes / (24 * 60)),
        minutes if minutes % 60 == 0 => format!("{}h", minutes / 60),
        minutes => format!("{minutes}m"),
    }
}

/// What Resource Definitions shows the caller, asked as them once for each resource.
pub struct Sight<'a> {
    backend: &'a Backend,
    seen: BTreeMap<String, Result<Value, u16>>,
}

impl<'a> Sight<'a> {
    pub fn new(backend: &'a Backend) -> Self {
        Self { backend, seen: BTreeMap::new() }
    }

    async fn resource(&mut self, kind: &str, name: &str) -> Result<Value, u16> {
        let key = format!("{kind}:{name}");
        if let Some(found) = self.seen.get(&key) {
            return found.clone();
        }
        let route = format!("resources/{kind}/{}", encoded(name));
        let found = match self.backend.ask("resources", "GET", &route, None, None).await {
            Ok((200, answer)) => Ok(answer),
            Ok((status, _)) => Err(status),
            Err(_) => Err(503),
        };
        self.seen.insert(key, found.clone());
        found
    }

    pub async fn member(&mut self, team: &str, login: &str) -> bool {
        let Ok(found) = self.resource("team", team).await else { return false };
        found["connections"].as_array().into_iter().flatten().any(|connection| {
            connection["kind"] == "User" && connection["name"].as_str() == Some(login)
        })
    }

    async fn exists(&mut self, kind: &str, name: &str) -> bool {
        self.resource(kind, name).await.is_ok()
    }
}

/// Whether the caller may see a request: who made it, its team's members, template managers and admins.
pub async fn sees(backend: &Backend, sight: &mut Sight<'_>, request: &Request) -> bool {
    let Ok(me) = login(backend) else { return false };
    admin(backend)
        || manages(backend)
        || request.requester == me
        || sight.member(&request.team, &me).await
}

async fn audit(backend: &Backend, action: &str, subject: &str, detail: Value) {
    if let Err(err) = backend.audit(action, Some(subject), detail).await {
        tracing::warn!(%err, %action, "an infrastructure action was not audited");
    }
}

async fn announce(backend: &Backend, topic: &str, payload: Value) {
    if let Err(err) = backend.publish(topic, payload).await {
        tracing::warn!(%err, %topic, "an infrastructure event was not published");
    }
}

pub fn template_shown(template: &Template) -> Value {
    let kind = catalog::resource_type(&template.vendor, &template.resource_type);
    json!({
        "id": template.id,
        "name": template.name,
        "title": template.title,
        "description": template.description,
        "vendor": template.vendor,
        "type": template.resource_type,
        "type_title": kind.map(|kind| kind.title),
        "size_means": kind.and_then(|kind| kind.size),
        "regions": template.regions,
        "sizes": template.sizes,
        "default_region": template.default_region,
        "default_size": template.default_size,
        "default_lifetime": lifetime(template.default_lifetime_minutes),
        "max_lifetime": lifetime(template.max_lifetime_minutes),
        "team_quota": template.team_quota,
        "created_by": template.created_by,
    })
}

/// The name Resource Definitions keeps the resource under.
pub fn resource_name(request: &Request) -> String {
    let at = request.vendor_id.clone().unwrap_or_else(|| request.name.clone());
    format!("{}/{}/{}/{at}", request.vendor, request.region, request.resource_type)
}

pub fn request_shown(request: &Request) -> Value {
    json!({
        "id": request.id,
        "template": request.template,
        "vendor": request.vendor,
        "type": request.resource_type,
        "name": request.name,
        "region": request.region,
        "size": request.size,
        "team": request.team,
        "service": request.service,
        "requester": request.requester,
        "status": request.status,
        "vendor_id": request.vendor_id,
        "resource": resource_name(request),
        "detail": request.detail,
        "error": request.error,
        "expires_at": request.expires_at,
        "warned_at": request.warned_at,
        "created_at": request.created_at,
        "url": page(&format!("requests/{}", request.id)),
    })
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct TemplateDetails {
    pub name: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub vendor: Option<String>,
    #[serde(rename = "type")]
    pub resource_type: Option<String>,
    pub regions: Option<Vec<String>>,
    pub sizes: Option<Vec<String>>,
    pub default_region: Option<String>,
    pub default_size: Option<String>,
    pub default_lifetime: Option<String>,
    pub max_lifetime: Option<String>,
    pub team_quota: Option<i64>,
}

fn fine(text: &str, extra: &str, most: usize) -> bool {
    !text.is_empty()
        && text.len() <= most
        && text.chars().all(|c| c.is_ascii_alphanumeric() || extra.contains(c))
}

/// Makes a template, or changes one; its vendor and type stay as they were made.
pub async fn save_template(
    backend: &Backend,
    existing: Option<Template>,
    asked: TemplateDetails,
) -> Result<Template, Refusal> {
    if !manages(backend) {
        return Err(Refusal::forbidden("needs plugin:infra:pluginuser:templates:rw"));
    }
    let me = login(backend)?;
    let new = existing.is_none();
    let mut template = match existing {
        Some(template) => template,
        None => {
            let name = asked.name.clone().unwrap_or_default().trim().to_string();
            if !fine(&name, "-", 40) || name.starts_with('-') {
                return Err(Refusal::bad(
                    "a template's name is up to 40 lowercase letters, digits and hyphens",
                ));
            }
            let vendor = asked.vendor.clone().unwrap_or_default();
            let kind = asked.resource_type.clone().unwrap_or_default();
            let found = catalog::resource_type(vendor.trim(), kind.trim()).ok_or_else(|| {
                Refusal::bad(format!(
                    "{} offers no `{kind}`; see the catalogue for what each vendor offers",
                    catalog::vendor_name(vendor.trim())
                ))
            })?;
            Template {
                id: Uuid::now_v7(),
                name: name.to_ascii_lowercase(),
                title: String::new(),
                description: String::new(),
                vendor: found.vendor.to_string(),
                resource_type: found.name.to_string(),
                regions: Vec::new(),
                sizes: Vec::new(),
                default_region: String::new(),
                default_size: None,
                default_lifetime_minutes: 0,
                max_lifetime_minutes: 0,
                team_quota: 1,
                created_by: me,
                created_at: String::new(),
                updated_at: String::new(),
            }
        }
    };
    if !new && (asked.vendor.is_some() || asked.resource_type.is_some() || asked.name.is_some()) {
        return Err(Refusal::bad(
            "a template keeps its name, vendor and type; make another for a different one",
        ));
    }
    let kind = catalog::resource_type(&template.vendor, &template.resource_type)
        .ok_or_else(|| Refusal::unavailable("the template's type is no longer in the catalogue"))?;
    if let Some(title) = asked.title {
        template.title = title.trim().to_string();
    }
    if template.title.is_empty() || template.title.chars().count() > 200 {
        return Err(Refusal::bad("a template's title is 1 to 200 characters"));
    }
    if let Some(description) = asked.description {
        template.description = description.trim().chars().take(2_000).collect();
    }
    if let Some(regions) = asked.regions {
        template.regions = regions
            .iter()
            .map(|region| region.trim().to_string())
            .filter(|region| !region.is_empty())
            .collect();
    }
    if template.regions.is_empty() || !template.regions.iter().all(|region| fine(region, "-", 40)) {
        return Err(Refusal::bad("a template allows at least one region, such as eu-west-2"));
    }
    if let Some(sizes) = asked.sizes {
        template.sizes = sizes
            .iter()
            .map(|size| size.trim().to_string())
            .filter(|size| !size.is_empty())
            .collect();
    }
    match kind.size {
        Some(means)
            if template.sizes.is_empty()
                || !template.sizes.iter().all(|size| kind.sizes.contains(&size.as_str())) =>
        {
            return Err(Refusal::bad(format!(
                "a {} template allows at least one {means}, from {}",
                kind.title,
                kind.sizes.join(", ")
            )));
        }
        None if !template.sizes.is_empty() => {
            return Err(Refusal::bad(format!("a {} has no size", kind.title)));
        }
        _ => {}
    }
    if let Some(region) = asked.default_region {
        template.default_region = region.trim().to_string();
    }
    if template.default_region.is_empty() {
        template.default_region = template.regions[0].clone();
    }
    if !template.regions.contains(&template.default_region) {
        return Err(Refusal::bad("the default region is one of the regions it allows"));
    }
    if let Some(size) = asked.default_size {
        template.default_size = Some(size.trim().to_string()).filter(|size| !size.is_empty());
    }
    if kind.size.is_some() && template.default_size.is_none() {
        template.default_size = template.sizes.first().cloned();
    }
    if template.default_size.as_ref().is_some_and(|size| !template.sizes.contains(size)) {
        return Err(Refusal::bad("the default size is one of the sizes it allows"));
    }
    if let Some(most) = asked.max_lifetime {
        template.max_lifetime_minutes = minutes(&most)?;
    }
    if let Some(usual) = asked.default_lifetime {
        template.default_lifetime_minutes = minutes(&usual)?;
    }
    if template.max_lifetime_minutes == 0 {
        return Err(Refusal::bad("say how long its resources may live at most, such as 30d"));
    }
    if template.default_lifetime_minutes == 0 {
        template.default_lifetime_minutes = template.max_lifetime_minutes;
    }
    if template.default_lifetime_minutes > template.max_lifetime_minutes {
        return Err(Refusal::bad("the usual lifetime is within the longest"));
    }
    if let Some(quota) = asked.team_quota {
        template.team_quota = quota;
    }
    if !(1..=100).contains(&template.team_quota) {
        return Err(Refusal::bad("each team may have 1 to 100 resources from a template"));
    }
    let store = Store(backend);
    store.save_template(&template).await?;
    let action = if new { "template.created" } else { "template.changed" };
    audit(backend, action, &template.name, template_shown(&template)).await;
    store.template(&template.name).await?.ok_or_else(|| Refusal::missing("the template has gone"))
}

pub async fn delete_template(backend: &Backend, name: &str) -> Result<Template, Refusal> {
    if !manages(backend) {
        return Err(Refusal::forbidden("needs plugin:infra:pluginuser:templates:rw"));
    }
    let store = Store(backend);
    let template = store
        .template(name)
        .await?
        .ok_or_else(|| Refusal::missing(format!("there is no template {name}")))?;
    if !store.delete_template(template.id).await? {
        return Err(Refusal::bad(
            "resources were requested from it, so it stays; change it instead",
        ));
    }
    audit(backend, "template.deleted", &template.name, json!({ "by": login(backend)? })).await;
    Ok(template)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewRequest {
    pub template: String,
    pub name: String,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub size: Option<String>,
    #[serde(default)]
    pub lifetime: Option<String>,
    pub team: String,
    #[serde(default)]
    pub service: Option<String>,
    /// `development` or `production`; development when it is left out.
    #[serde(default)]
    pub environment: Option<String>,
}

/// Checks a request against permissions, the template's limits and the team's quota, then queues it.
pub async fn request(
    backend: &Backend,
    sight: &mut Sight<'_>,
    asked: NewRequest,
) -> Result<(Request, Uuid), Refusal> {
    let me = login(backend)?;
    let store = Store(backend);
    let template = store.template(asked.template.trim()).await?.ok_or_else(|| {
        Refusal::missing(format!("there is no template {}", asked.template.trim()))
    })?;
    if !may_request(backend, &template.vendor) {
        return Err(Refusal::forbidden(format!(
            "needs plugin:infra:pluginuser:selfservice-{}, or plugin:infra:pluginuser:selfservice",
            template.vendor
        )));
    }
    let name = asked.name.trim().to_ascii_lowercase();
    if !fine(&name, "-", 30) || name.len() < 3 || name.starts_with('-') || name.ends_with('-') {
        return Err(Refusal::bad(
            "a resource's name is 3 to 30 lowercase letters, digits and hyphens",
        ));
    }
    let region =
        asked.region.map(|region| region.trim().to_string()).filter(|region| !region.is_empty());
    let region = region.unwrap_or_else(|| template.default_region.clone());
    if !template.regions.contains(&region) {
        return Err(Refusal::bad(format!(
            "{} allows only {}",
            template.title,
            template.regions.join(", ")
        )));
    }
    let size = asked.size.map(|size| size.trim().to_string()).filter(|size| !size.is_empty());
    let size = size.or_else(|| template.default_size.clone());
    match &size {
        Some(_) if template.sizes.is_empty() => {
            return Err(Refusal::bad(format!("{} has no size", template.title)));
        }
        Some(size) if !template.sizes.contains(size) => {
            return Err(Refusal::bad(format!(
                "{} allows sizes {}",
                template.title,
                template.sizes.join(", ")
            )));
        }
        None if !template.sizes.is_empty() => return Err(Refusal::bad("choose a size")),
        _ => {}
    }
    let wanted = match asked.lifetime.as_deref().map(str::trim).filter(|text| !text.is_empty()) {
        Some(text) => minutes(text)?,
        None => template.default_lifetime_minutes,
    };
    if wanted > template.max_lifetime_minutes {
        return Err(Refusal::bad(format!(
            "{} lives at most {}",
            template.title,
            lifetime(template.max_lifetime_minutes)
        )));
    }
    let team = asked.team.trim().trim_start_matches("team:").to_string();
    if !sight.exists("team", &team).await {
        return Err(Refusal::bad(format!("there is no team called {team}, or you cannot see it")));
    }
    if !admin(backend) && !sight.member(&team, &me).await {
        return Err(Refusal::forbidden(format!("only members of {team} request resources for it")));
    }
    let service =
        asked.service.map(|service| service.trim().trim_start_matches("service:").to_string());
    let service = service.filter(|service| !service.is_empty());
    if let Some(service) = &service
        && !sight.exists("service", service).await
    {
        return Err(Refusal::bad(format!(
            "there is no service called {service}, or you cannot see it"
        )));
    }
    // Production is read: DOC shows what runs there, and nothing here makes it.
    let environment = store::environment_of(asked.environment.as_deref());
    if environment == store::PRODUCTION {
        return Err(Refusal::bad(
            "production resources are not asked for through DOC; Production shows what is \
             already running there",
        ));
    }
    let wanted_request = Request {
        id: Uuid::now_v7(),
        template: template.id,
        environment,
        vendor: template.vendor.clone(),
        resource_type: template.resource_type.clone(),
        name,
        region,
        size,
        team,
        service,
        requester: me.clone(),
        status: "pending".into(),
        vendor_id: None,
        detail: json!({}),
        error: None,
        expires_at: (Utc::now() + Duration::minutes(wanted)).to_rfc3339(),
        warned_at: None,
        checked_at: None,
        created_at: String::new(),
        updated_at: String::new(),
    };
    if let Err(live) = store.add_request(&wanted_request, template.team_quota).await? {
        return Err(Refusal::conflict(format!(
            "{} already has {live} of the {} it may have from {}",
            wanted_request.team, template.team_quota, template.title
        )));
    }
    let request =
        store.request(wanted_request.id).await?.ok_or_else(|| Refusal::missing("it has gone"))?;
    audit(backend, "request.created", &request.id.to_string(), request_shown(&request)).await;
    let payload = json!({ "request": request.id, "template": template.name, "vendor": request.vendor, "team": request.team, "requester": me, "url": page(&format!("requests/{}", request.id)) });
    announce(backend, "plugin.infra.request.created", payload).await;
    let task = backend.task(json!({ "provision": request.id })).await?;
    Ok((request, task))
}

pub async fn found(backend: &Backend, sight: &mut Sight<'_>, id: &str) -> Result<Request, Refusal> {
    let id: Uuid = id.parse().map_err(|_| Refusal::missing("there is no such request"))?;
    match Store(backend).request(id).await? {
        Some(request) if sees(backend, sight, &request).await => Ok(request),
        _ => Err(Refusal::missing("there is no such request, or it is not yours to see")),
    }
}

/// Whether the caller owns a request: who made it, its team's members, and admins.
async fn owns(
    backend: &Backend,
    sight: &mut Sight<'_>,
    request: &Request,
) -> Result<String, Refusal> {
    let me = login(backend)?;
    match admin(backend) || request.requester == me || sight.member(&request.team, &me).await {
        true => Ok(me),
        false => {
            Err(Refusal::forbidden(format!("only {}'s members change its resources", request.team)))
        }
    }
}

/// Pushes a live resource's expiry back, up to its template's longest lifetime from when it was asked for.
pub async fn extend(
    backend: &Backend,
    sight: &mut Sight<'_>,
    id: &str,
    by: &str,
) -> Result<Request, Refusal> {
    let store = Store(backend);
    let request = found(backend, sight, id).await?;
    let me = owns(backend, sight, &request).await?;
    if !matches!(request.status.as_str(), "active" | "expiring" | "pending" | "provisioning") {
        return Err(Refusal::bad(format!("it is {}, so it cannot be extended", request.status)));
    }
    let template = store
        .template_by_id(request.template)
        .await?
        .ok_or_else(|| Refusal::missing("its template has gone"))?;
    let asked = minutes(by)?;
    let parse =
        |text: &str| DateTime::parse_from_rfc3339(text).map(|at| at.with_timezone(&Utc)).ok();
    let (Some(created), Some(expires)) = (parse(&request.created_at), parse(&request.expires_at))
    else {
        return Err(Refusal::unavailable("its times cannot be read"));
    };
    let latest = created + Duration::minutes(template.max_lifetime_minutes);
    let wanted = expires.max(Utc::now()) + Duration::minutes(asked);
    if wanted > latest {
        return Err(Refusal::bad(format!(
            "{} lives at most {} from when it was asked for, until {}",
            template.title,
            lifetime(template.max_lifetime_minutes),
            latest.format("%-d %b %Y, %H:%M UTC")
        )));
    }
    store.extend(request.id, &wanted.to_rfc3339()).await?;
    audit(
        backend,
        "request.extended",
        &request.id.to_string(),
        json!({ "by": me, "until": wanted }),
    )
    .await;
    store.request(request.id).await?.ok_or_else(|| Refusal::missing("it has gone"))
}

/// Deletes a resource before it expires.
pub async fn delete_early(
    backend: &Backend,
    sight: &mut Sight<'_>,
    id: &str,
) -> Result<(Request, Uuid), Refusal> {
    let store = Store(backend);
    let request = found(backend, sight, id).await?;
    let me = owns(backend, sight, &request).await?;
    if !store.shift(request.id, &["active", "expiring", "failed"], "deleting").await? {
        return Err(Refusal::bad(format!("it is {}, so it cannot be deleted now", request.status)));
    }
    audit(backend, "request.deleting", &request.id.to_string(), json!({ "by": me, "early": true }))
        .await;
    let task = backend.task(json!({ "teardown": request.id })).await?;
    let request =
        store.request(request.id).await?.ok_or_else(|| Refusal::missing("it has gone"))?;
    Ok((request, task))
}

fn wanted_of(request: &Request) -> Wanted {
    Wanted {
        kind: request.resource_type.clone(),
        name: request
            .vendor_id
            .clone()
            .unwrap_or_else(|| vendors::vendor_name(&request.vendor, &request.name, request.id)),
        region: request.region.clone(),
        size: request.size.clone(),
        labels: BTreeMap::from([
            ("doc-request".to_string(), request.id.to_string()),
            ("doc-team".to_string(), request.team.clone()),
            ("doc-expires".to_string(), request.expires_at.clone()),
        ]),
    }
}

/// Tells Resource Definitions about a resource, connected to its team and service.
async fn synced(backend: &Backend, request: &Request) {
    let address = vendors::address(&request.detail);
    let mut connections = json!({ "Teams": [request.team] });
    if let Some(service) = &request.service {
        connections["Services"] = json!([service]);
    }
    let kind = catalog::resource_type(&request.vendor, &request.resource_type)
        .map_or("resource", |kind| kind.title);
    let document = json!({
        "kind": "CloudResource",
        "name": resource_name(request),
        "title": format!("{} ({})", request.name, kind),
        "metadata": {
            "vendor": request.vendor,
            "type": request.resource_type,
            "region": request.region,
            "request": request.id,
            "expires_at": request.expires_at,
            "address": address,
            "url": page(&format!("requests/{}", request.id)),
        },
        "connections": connections,
    });
    announce(backend, "plugin.infra.cloud-resource.synced", document).await;
}

/// The `provision` task: asks the vendor to make what a request asked for.
pub async fn provision(backend: &Backend, id: Uuid) -> Result<Value, Refusal> {
    let store = Store(backend);
    let Some(request) = store.request(id).await? else { return Ok(json!({ "skipped": "gone" })) };
    if !store.shift(id, &["pending"], "provisioning").await? {
        return Ok(json!({ "skipped": request.status }));
    }
    let wanted = wanted_of(&request);
    let made = match vendors::adapter(&request.vendor) {
        Ok(adapter) => adapter.create(&wanted).await,
        Err(err) => Err(err),
    };
    match made {
        Ok(made) => {
            let status = if made.ready { "active" } else { "provisioning" };
            store.settle(id, status, Some(&made.id), &made.detail, None).await?;
            let request =
                store.request(id).await?.ok_or_else(|| Refusal::missing("it has gone"))?;
            audit(
                backend,
                "request.provisioned",
                &id.to_string(),
                json!({ "vendor_id": made.id, "status": status }),
            )
            .await;
            if made.ready {
                became_active(backend, &request).await;
            }
            Ok(json!({ "request": id, "status": status, "vendor_id": made.id }))
        }
        Err(err) => {
            store.settle(id, "failed", None, &json!({}), Some(&err)).await?;
            audit(backend, "request.failed", &id.to_string(), json!({ "error": err })).await;
            let payload = json!({ "request": id, "team": request.team, "error": err, "url": page(&format!("requests/{id}")) });
            announce(backend, "plugin.infra.request.failed", payload).await;
            Ok(json!({ "request": id, "status": "failed", "error": err }))
        }
    }
}

/// What a resource that is now standing says about itself. The service it was stood up for and
/// the address it answers at are both here so a name can follow it: whoever cares — the DNS
/// plugin, an automation — hears it whether the resource came from a template or from the page.
fn active_payload(request: &Request) -> Value {
    json!({
        "request": request.id,
        "team": request.team,
        "service": request.service,
        "name": resource_name(request),
        "vendor": request.vendor,
        "type": request.resource_type,
        "region": request.region,
        "environment": request.environment,
        "address": vendors::address(&request.detail),
        "expires_at": request.expires_at,
        "url": page(&format!("requests/{}", request.id)),
    })
}

async fn became_active(backend: &Backend, request: &Request) {
    synced(backend, request).await;
    announce(backend, "plugin.infra.request.active", active_payload(request)).await;
}

/// The `teardown` task: deletes the resource at the vendor and forgets it in Resource Definitions.
pub async fn teardown(backend: &Backend, id: Uuid) -> Result<Value, Refusal> {
    let store = Store(backend);
    let Some(request) = store.request(id).await? else { return Ok(json!({ "skipped": "gone" })) };
    if request.status != "deleting" {
        return Ok(json!({ "skipped": request.status }));
    }
    let deleted = match (&request.vendor_id, vendors::adapter(&request.vendor)) {
        (None, _) => Ok(()),
        (Some(vendor_id), Ok(adapter)) => adapter.delete(&wanted_of(&request), vendor_id).await,
        (Some(_), Err(err)) => Err(err),
    };
    match deleted {
        Ok(()) => {
            store.settle(id, "deleted", None, &request.detail, None).await?;
            gone(backend, &request, "request.deleted", None).await;
            Ok(json!({ "request": id, "status": "deleted" }))
        }
        Err(err) => {
            store.settle(id, "deleting", None, &request.detail, Some(&err)).await?;
            audit(backend, "request.teardown-failed", &id.to_string(), json!({ "error": err }))
                .await;
            Err(Refusal::unavailable(format!("the resource was not deleted: {err}")))
        }
    }
}

async fn gone(backend: &Backend, request: &Request, action: &str, why: Option<&str>) {
    audit(
        backend,
        action,
        &request.id.to_string(),
        json!({ "resource": resource_name(request), "why": why }),
    )
    .await;
    announce(
        backend,
        "plugin.infra.cloud-resource.removed",
        json!({ "name": resource_name(request) }),
    )
    .await;
    let mut payload = active_payload(request);
    payload["why"] = json!(why);
    announce(backend, "plugin.infra.request.deleted", payload).await;
}

fn drift_minutes() -> i64 {
    vendors::setting("DOC_INFRA_DRIFT_MINUTES")
        .and_then(|minutes| minutes.parse().ok())
        .filter(|minutes| *minutes > 0)
        .unwrap_or(DRIFT_MINUTES)
}

fn instant(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text).ok().map(|at| at.with_timezone(&Utc))
}

/// The `lifecycle` schedule: finishes provisioning, warns before expiry, tears down what expired and notices drift.
pub async fn lifecycle(backend: &Backend) -> Result<Value, Refusal> {
    let store = Store(backend);
    let now = Utc::now();
    let (mut ready, mut warned, mut expired, mut drifted) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for request in store.in_status(&["provisioning", "active", "expiring"]).await? {
        let (Some(expires), Some(created)) =
            (instant(&request.expires_at), instant(&request.created_at))
        else {
            continue;
        };
        if request.status != "provisioning" && expires <= now {
            if store.shift(request.id, &["active", "expiring"], "deleting").await? {
                audit(
                    backend,
                    "request.expired",
                    &request.id.to_string(),
                    json!({ "expires_at": request.expires_at }),
                )
                .await;
                backend.task(json!({ "teardown": request.id })).await?;
                expired.push(request.id);
            }
            continue;
        }
        let lead = Duration::hours(WARN_HOURS).min((expires - created) / 4);
        if request.status == "active" && expires - lead <= now && store.warn(request.id).await? {
            let payload = json!({
                "team": request.team,
                "name": resource_name(&request),
                "vendor": request.vendor,
                "expires_at": request.expires_at,
                "request": request.id,
                "url": page(&format!("requests/{}", request.id)),
            });
            announce(backend, "plugin.infra.resource.expiring", payload).await;
            audit(
                backend,
                "request.warned",
                &request.id.to_string(),
                json!({ "expires_at": request.expires_at }),
            )
            .await;
            warned.push(request.id);
        }
        let due = request.status == "provisioning"
            || request
                .checked_at
                .as_deref()
                .and_then(instant)
                .is_none_or(|at| now - at >= Duration::minutes(drift_minutes()));
        let Some(vendor_id) = request.vendor_id.clone().filter(|_| due) else { continue };
        let Ok(adapter) = vendors::adapter(&request.vendor) else { continue };
        match adapter.inspect(&wanted_of(&request), &vendor_id).await {
            Ok(None) if request.status != "provisioning" => {
                if store.shift(request.id, &["active", "expiring"], "deleted").await? {
                    gone(backend, &request, "request.drifted", Some("deleted outside DOC")).await;
                    drifted.push(json!({ "request": request.id, "drift": "deleted outside DOC" }));
                }
            }
            Ok(Some(made)) if request.status == "provisioning" && made.ready => {
                store.settle(request.id, "active", Some(&made.id), &made.detail, None).await?;
                if let Some(request) = store.request(request.id).await? {
                    became_active(backend, &request).await;
                }
                ready.push(request.id);
            }
            Ok(Some(made)) => {
                let changes = changed(&request, &made.detail);
                if !changes.is_empty()
                    && request.detail.get("drift").is_none_or(|drift| drift != &json!(changes))
                {
                    let mut detail = made.detail.clone();
                    detail["drift"] = json!(changes);
                    store.settle(request.id, &request.status, None, &detail, None).await?;
                    audit(
                        backend,
                        "request.drifted",
                        &request.id.to_string(),
                        json!({ "changes": changes }),
                    )
                    .await;
                    let payload = json!({ "request": request.id, "team": request.team, "name": resource_name(&request), "changes": changes, "url": page(&format!("requests/{}", request.id)) });
                    announce(backend, "plugin.infra.resource.drifted", payload).await;
                    drifted.push(json!({ "request": request.id, "drift": changes }));
                } else {
                    store.checked(request.id).await?;
                    synced(backend, &request).await;
                }
            }
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(request = %request.id, %err, "a resource could not be inspected")
            }
        }
    }
    Ok(json!({ "ready": ready, "warned": warned, "expired": expired, "drifted": drifted }))
}

/// Which of its DOC labels the vendor no longer shows as they were made.
fn changed(request: &Request, now: &Value) -> Vec<String> {
    let shown: BTreeMap<String, String> = match (&now["tags"], &now["labels"]) {
        (Value::Array(tags), _) => tags
            .iter()
            .filter_map(|tag| {
                Some((tag["key"].as_str()?.to_string(), tag["value"].as_str()?.to_string()))
            })
            .collect(),
        (Value::Object(tags), _) | (_, Value::Object(tags)) => tags
            .iter()
            .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_string())))
            .collect(),
        _ => return Vec::new(),
    };
    [("doc-request", request.id.to_string()), ("doc-team", request.team.clone())]
        .into_iter()
        .filter(|(key, value)| shown.get(*key) != Some(value))
        .map(|(key, _)| format!("its {key} label was changed or removed"))
        .collect()
}

pub async fn handle(backend: &Backend, request: Call) -> Response {
    let path = request.path.trim_end_matches('/').to_string();
    let segments: Vec<&str> = path.split('/').collect();
    let answer = match segments.as_slice() {
        ["ui", route @ ..] => return crate::ui::handle(backend, &request, route).await,
        ["api", route @ ..] => api(backend, &request, route).await,
        _ => Err(Refusal::missing("no such route")),
    };
    match answer {
        Ok((204, _)) => Response::new(204, "application/json", Vec::new()),
        Ok((status, value)) => Response::new(
            status,
            "application/json",
            serde_json::to_vec(&value).unwrap_or_default(),
        ),
        Err(refusal) => refusal.response(),
    }
}

async fn api(backend: &Backend, request: &Call, path: &[&str]) -> Answer {
    let store = Store(backend);
    let mut sight = Sight::new(backend);
    match (request.method.as_str(), path) {
        ("GET", ["catalog"]) => Ok((200, catalog::shown())),
        ("GET", ["templates"]) => {
            let listed = store.templates().await?;
            Ok((200, json!(listed.iter().map(template_shown).collect::<Vec<_>>())))
        }
        ("POST", ["templates"]) => {
            let template = save_template(backend, None, body(request)?).await?;
            Ok((201, template_shown(&template)))
        }
        ("GET", ["templates", name]) => {
            let template = store
                .template(name)
                .await?
                .ok_or_else(|| Refusal::missing(format!("there is no template {name}")))?;
            Ok((200, template_shown(&template)))
        }
        ("PATCH", ["templates", name]) => {
            let existing = store
                .template(name)
                .await?
                .ok_or_else(|| Refusal::missing(format!("there is no template {name}")))?;
            let template = save_template(backend, Some(existing), body(request)?).await?;
            Ok((200, template_shown(&template)))
        }
        ("DELETE", ["templates", name]) => {
            delete_template(backend, name).await?;
            Ok((204, Value::Null))
        }
        ("GET", ["requests"]) => {
            let team = query(request, "team");
            let mut shown = Vec::new();
            for found in store.requests().await? {
                if team.as_deref().is_some_and(|team| team != found.team) {
                    continue;
                }
                if query(request, "all").is_none() && found.status == "deleted" {
                    continue;
                }
                if sees(backend, &mut sight, &found).await {
                    shown.push(request_shown(&found));
                }
            }
            Ok((200, json!(shown)))
        }
        ("POST", ["requests"]) => {
            let (made, task) = self::request(backend, &mut sight, body(request)?).await?;
            let mut shown = request_shown(&made);
            shown["task"] = json!(task);
            Ok((202, shown))
        }
        ("GET", ["requests", id]) => {
            Ok((200, request_shown(&found(backend, &mut sight, id).await?)))
        }
        ("POST", ["requests", id, "extend"]) => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Extending {
                by: String,
            }
            let asked: Extending = body(request)?;
            Ok((200, request_shown(&extend(backend, &mut sight, id, &asked.by).await?)))
        }
        ("POST", ["requests", id, "delete"]) | ("DELETE", ["requests", id]) => {
            let (deleting, task) = delete_early(backend, &mut sight, id).await?;
            let mut shown = request_shown(&deleting);
            shown["task"] = json!(task);
            Ok((202, shown))
        }
        _ => Err(Refusal::missing("no such route")),
    }
}
