//! The plugin's routes. A calendar is seen by whoever can see its resource in Resource Definitions,
//! asked as them, and a person's own calendar by them alone. Feeds are served at secret URLs under
//! `public/feeds/`, and other plugins find a resource's calendar through `discovery/`.

use std::collections::BTreeMap;

use doc_plugin_sdk::protocol::Secret;
use doc_plugin_sdk::{Backend, Caller, Query, Request, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use url::form_urlencoded::byte_serialize;
use uuid::Uuid;

use crate::store::{Calendar, Feed, Store};
use crate::{Refusal, ics};

type Answer = Result<(u16, Value), Refusal>;

/// The widest agenda one request may ask for.
const MAX_DAYS: i64 = 400;

pub fn query(request: &Request, key: &str) -> Option<String> {
    url::form_urlencoded::parse(request.query.as_bytes())
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn body<T: for<'de> Deserialize<'de>>(request: &Request) -> Result<T, Refusal> {
    let bytes = if request.body.is_empty() { &b"{}"[..] } else { &request.body[..] };
    serde_json::from_slice(bytes)
        .map_err(|err| Refusal::bad(format!("the body is not what this takes: {err}")))
}

fn id_of(text: &str) -> Result<Uuid, Refusal> {
    text.parse().map_err(|_| Refusal::missing("there is no such calendar"))
}

fn encoded(text: &str) -> String {
    byte_serialize(text.as_bytes()).collect::<String>().replace('+', "%20")
}

/// `kind:name`, with the kind in one spelling.
pub fn normalised(text: &str) -> Result<String, Refusal> {
    let refused = || {
        Refusal::bad(format!(
            "write the resource as kind:name, such as team:payments-core, not `{text}`"
        ))
    };
    let (kind, name) = text.trim().split_once(':').ok_or_else(refused)?;
    let kind: String =
        kind.chars().filter(char::is_ascii_alphanumeric).collect::<String>().to_ascii_lowercase();
    match (kind.is_empty(), name.trim()) {
        (false, name) if !name.is_empty() => Ok(format!("{kind}:{name}")),
        _ => Err(refused()),
    }
}

/// The person or service account a call is for, as `user:<id>`, and how they are known.
pub fn principal(backend: &Backend) -> Result<(String, String), Refusal> {
    match backend.caller() {
        Some(Caller { kind, id: Some(id), label, .. }) if kind == "user" || kind == "service" => {
            Ok((format!("{kind}:{id}"), label.clone().unwrap_or_else(|| id.clone())))
        }
        _ => Err(Refusal::forbidden("calendars are for people and service accounts")),
    }
}

/// Answers whether the caller may see calendars, remembering each resource it has asked about.
pub struct Sight<'a> {
    backend: &'a Backend,
    seen: BTreeMap<String, Result<Value, u16>>,
}

impl<'a> Sight<'a> {
    pub fn new(backend: &'a Backend) -> Self {
        Self { backend, seen: BTreeMap::new() }
    }

    /// The resource as Resource Definitions shows it to the caller, or the status it refused with.
    async fn resource(&mut self, resource: &str) -> Result<Value, u16> {
        if let Some(found) = self.seen.get(resource) {
            return found.clone();
        }
        let (kind, name) = resource.split_once(':').unwrap_or_default();
        let path: Vec<String> = name.split('/').map(encoded).collect();
        let route = format!("resources/{kind}/{}", path.join("/"));
        let found = match self.backend.ask("resources", "GET", &route, None, None).await {
            Ok((200, mut answer)) => Ok(answer["resource"].take()),
            Ok((status, _)) => Err(status),
            Err(_) => Err(503),
        };
        self.seen.insert(resource.to_string(), found.clone());
        found
    }

    pub async fn sees(&mut self, calendar: &Calendar) -> bool {
        let admin = self.backend.caller().is_some_and(|caller| caller.admin);
        match &calendar.owner {
            Some(owner) => admin || principal(self.backend).is_ok_and(|(me, _)| &me == owner),
            None => self.resource(&calendar.resource).await.is_ok(),
        }
    }

    async fn visible(&mut self, calendar: Calendar) -> Result<Calendar, Refusal> {
        match self.sees(&calendar).await {
            true => Ok(calendar),
            false => Err(Refusal::missing("there is no such calendar, or you cannot see it")),
        }
    }
}

/// `subscribed` is every calendar in the caller's own view, and `by_default` the ones that are
/// there because they are in the team, rather than because they asked for them.
fn shown(calendar: &Calendar, subscribed: &[Uuid], by_default: &[String]) -> Value {
    json!({
        "id": calendar.id,
        "resource": calendar.resource,
        "name": calendar.name,
        "description": calendar.description,
        "timezone": calendar.timezone,
        "personal": calendar.owner.is_some(),
        "subscribed": subscribed.contains(&calendar.id),
        "by_default": by_default.contains(&calendar.resource),
        "created_at": calendar.created_at,
    })
}

/// One calendar as the caller sees it, with whether it is in their own view and why.
async fn seen(backend: &Backend, me: &str, calendar: &Calendar) -> Result<Value, Refusal> {
    let following = following(backend, me).await?;
    Ok(shown(calendar, &following, &teams_of(backend, me).await?))
}

fn checked_zone(zone: &str) -> Result<String, Refusal> {
    match zone.parse::<chrono_tz::Tz>() {
        Ok(_) => Ok(zone.to_string()),
        Err(_) => Err(Refusal::bad(format!("`{zone}` is not a time zone, such as Europe/London"))),
    }
}

/// The resource's calendar, made the first time it is asked for, named after the resource.
pub async fn ensure(
    backend: &Backend,
    sight: &mut Sight<'_>,
    resource: &str,
    by: &str,
) -> Result<Calendar, Refusal> {
    if let Some(found) = Store(backend).for_resource(resource).await? {
        return sight.visible(found).await;
    }
    let shown = sight.resource(resource).await.map_err(|status| match status {
        403 | 404 => Refusal::missing(format!(
            "{resource} is not a resource you can see in Resource Definitions"
        )),
        _ => Refusal::unavailable("Resource Definitions could not be asked"),
    })?;
    let name = shown["title"].as_str().filter(|title| !title.is_empty()).map_or_else(
        || resource.split_once(':').map_or(resource, |(_, name)| name).to_string(),
        str::to_string,
    );
    let calendar = Calendar {
        id: Uuid::now_v7(),
        resource: resource.to_string(),
        name,
        description: String::new(),
        timezone: "UTC".into(),
        owner: None,
        created_by: by.to_string(),
        created_at: String::new(),
        updated_at: String::new(),
    };
    Store(backend).ensure(&calendar).await
}

/// The caller's own calendar, made the first time they use it.
pub async fn personal(backend: &Backend) -> Result<Calendar, Refusal> {
    let (owner, label) = principal(backend)?;
    if let Some(found) = Store(backend).for_owner(&owner).await? {
        return Ok(found);
    }
    let calendar = Calendar {
        id: Uuid::now_v7(),
        resource: format!("user:{label}"),
        name: format!("{label}'s calendar"),
        description: String::new(),
        timezone: "UTC".into(),
        owner: Some(owner),
        created_by: label,
        created_at: String::new(),
        updated_at: String::new(),
    };
    Store(backend).ensure(&calendar).await
}

fn secret() -> Result<Secret<String>, Refusal> {
    let mut bytes = [0u8; 32];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes)
        .map_err(|_| Refusal::unavailable("no randomness for a feed's secret"))?;
    Ok(Secret::new(hex::encode(bytes)))
}

/// Where the backend is reached from outside, since calendar apps fetch feeds from it directly.
fn feed_url(token: &str) -> String {
    let base = std::env::var("DOC_API_PUBLIC_URL").unwrap_or_default();
    format!("{}/api/v1/plugins/calendar/public/feeds/{token}.ics", base.trim_end_matches('/'))
}

pub fn feed_shown(feed: &Feed, names: &BTreeMap<Uuid, String>) -> Value {
    json!({
        "id": feed.id,
        "calendar": feed.calendar,
        "of": feed.calendar.and_then(|id| names.get(&id).cloned()).unwrap_or_else(|| "Everything you subscribe to".into()),
        "url": feed_url(feed.token.expose()),
        "created_at": feed.created_at,
        "rotated_at": feed.rotated_at,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewCalendar {
    resource: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    timezone: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct Change {
    name: Option<String>,
    description: Option<String>,
    timezone: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Subscription {
    calendar: Uuid,
    #[serde(default = "yes")]
    subscribed: bool,
}

fn yes() -> bool {
    true
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct NewFeed {
    calendar: Option<Uuid>,
}

/// `from` and `to` as RFC 3339, the next four weeks unless asked otherwise.
fn range(request: &Request) -> Result<(String, String), Refusal> {
    let parse = |key: &str| -> Result<Option<chrono::DateTime<chrono::Utc>>, Refusal> {
        query(request, key)
            .map(|text| {
                chrono::DateTime::parse_from_rfc3339(&text)
                    .map(|at| at.with_timezone(&chrono::Utc))
                    .or_else(|_| {
                        chrono::NaiveDate::parse_from_str(&text, "%Y-%m-%d")
                            .map(|day| day.and_hms_opt(0, 0, 0).unwrap_or_default().and_utc())
                    })
                    .map_err(|_| {
                        Refusal::bad(format!(
                            "`{text}` is not a time, such as 2026-09-21 or 2026-09-21T09:00:00Z"
                        ))
                    })
            })
            .transpose()
    };
    let from = parse("from")?.unwrap_or_else(chrono::Utc::now);
    let to = parse("to")?.unwrap_or(from + chrono::Duration::days(28));
    if to <= from || (to - from).num_days() > MAX_DAYS {
        return Err(Refusal::bad(format!(
            "`to` comes after `from`, at most {MAX_DAYS} days later"
        )));
    }
    Ok((from.to_rfc3339(), to.to_rfc3339()))
}

/// Occurrences on these calendars from `calendar-events`, each with its calendar's name.
pub async fn occurrences(
    backend: &Backend,
    calendars: &[Calendar],
    from: &str,
    to: &str,
) -> Result<Vec<Value>, Refusal> {
    if calendars.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<String> = calendars.iter().map(|calendar| calendar.id.to_string()).collect();
    let asked = format!("calendars={}&from={}&to={}", ids.join(","), encoded(from), encoded(to));
    let (status, answer) = backend
        .discovery("calendar-events", "GET", "occurrences", Some(&asked), None)
        .await
        .map_err(|err| {
            Refusal::unavailable(format!("calendar-events could not be asked: {err}"))
        })?;
    if status != 200 {
        return Err(Refusal::unavailable(format!(
            "calendar-events answered {status}: {}",
            answer["detail"]
        )));
    }
    let names: BTreeMap<String, &Calendar> =
        calendars.iter().map(|calendar| (calendar.id.to_string(), calendar)).collect();
    Ok(answer["occurrences"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|occurrence| {
            let mut occurrence = occurrence.clone();
            if let Some(calendar) = occurrence["calendar"].as_str().and_then(|id| names.get(id)) {
                occurrence["calendar_name"] = json!(calendar.name);
                occurrence["calendar_resource"] = json!(calendar.resource);
            }
            occurrence
        })
        .collect())
}

/// The resources of the teams somebody is in, as `team:<name>`, in name order. Core keeps teams
/// and who is in them (T64, T69); a person's calendar holds their teams' without their asking.
pub async fn teams_of(backend: &Backend, principal: &str) -> Result<Vec<String>, Refusal> {
    let Some(user) = principal.strip_prefix("user:") else { return Ok(Vec::new()) };
    let held: Vec<Value> = backend
        .query_all(
            Query::new("core.team-members").filter(json!({ "user_id": user })).fields(&["team_id"]),
        )
        .await?;
    let ids: Vec<String> =
        held.iter().filter_map(|row| Some(row.get("team_id")?.as_str()?.to_string())).collect();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let teams: Vec<Value> = backend
        .query_all(
            Query::new("core.teams").filter(json!({ "id": { "in": ids } })).fields(&["name"]),
        )
        .await?;
    let mut resources: Vec<String> = teams
        .iter()
        .filter_map(|team| team.get("name")?.as_str())
        .filter(|name| !name.is_empty())
        .map(|name| format!("team:{name}"))
        .collect();
    resources.sort();
    resources.dedup();
    Ok(resources)
}

/// Every calendar in somebody's own view, whoever is asking: their own first, then their teams'
/// in name order, then the ones they subscribed to, less any they have turned off. Nothing here
/// is made on the way: a team whose calendar nobody has opened yet has no events to show.
pub async fn mine(backend: &Backend, principal: &str) -> Result<Vec<Calendar>, Refusal> {
    let store = Store(backend);
    let held = store.subscriptions(principal).await?;
    let muted: Vec<Uuid> = held.iter().filter(|one| one.muted).map(|one| one.calendar).collect();
    let mut calendars: Vec<Calendar> = store.for_owner(principal).await?.into_iter().collect();
    let add = |calendars: &mut Vec<Calendar>, calendar: Calendar| {
        if !calendars.iter().any(|known| known.id == calendar.id) {
            calendars.push(calendar);
        }
    };
    for resource in teams_of(backend, principal).await? {
        if let Some(calendar) = store.for_resource(&resource).await?
            && !muted.contains(&calendar.id)
        {
            add(&mut calendars, calendar);
        }
    }
    for one in held.iter().filter(|one| !one.muted) {
        if let Some(calendar) = store.calendar(one.calendar).await? {
            add(&mut calendars, calendar);
        }
    }
    Ok(calendars)
}

/// The calendars in somebody's own view, by ID: what a subscribe button shows the state of.
pub async fn following(backend: &Backend, principal: &str) -> Result<Vec<Uuid>, Refusal> {
    Ok(mine(backend, principal).await?.iter().map(|calendar| calendar.id).collect())
}

/// What a person sees across calendars: their own, their teams', and those they subscribe to,
/// each of which they can still see.
pub async fn theirs(backend: &Backend, sight: &mut Sight<'_>) -> Result<Vec<Calendar>, Refusal> {
    let (me, _) = principal(backend)?;
    let mut calendars = vec![personal(backend).await?];
    for calendar in mine(backend, &me).await? {
        if !calendars.iter().any(|known| known.id == calendar.id) && sight.sees(&calendar).await {
            calendars.push(calendar);
        }
    }
    Ok(calendars)
}

pub async fn handle(backend: &Backend, request: Request) -> Response {
    let path = request.path.trim_end_matches('/').to_string();
    let segments: Vec<&str> = path.split('/').collect();
    let answer = match segments.as_slice() {
        ["public", "feeds", file] if request.method == "GET" => return feed(backend, file).await,
        ["discovery", route @ ..] => discovery(backend, &request, route).await,
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

async fn api(backend: &Backend, request: &Request, path: &[&str]) -> Answer {
    let store = Store(backend);
    let mut sight = Sight::new(backend);
    let (me, label) = principal(backend)?;
    match (request.method.as_str(), path) {
        ("GET", ["calendars"]) => Ok((200, json!(visible_calendars(backend, &mut sight).await?))),
        ("POST", ["calendars"]) => {
            let asked: NewCalendar = body(request)?;
            let resource = normalised(&asked.resource)?;
            if resource.starts_with("user:") {
                return Err(Refusal::bad(
                    "each person has their own calendar already, at calendars/mine",
                ));
            }
            let mut calendar = ensure(backend, &mut sight, &resource, &label).await?;
            let change = Change {
                name: asked.name,
                description: asked.description,
                timezone: asked.timezone,
            };
            if change.name.is_some() || change.description.is_some() || change.timezone.is_some() {
                calendar = changed(backend, calendar, change).await?;
            }
            Ok((201, seen(backend, &me, &calendar).await?))
        }
        ("GET", ["calendars", "mine"]) => {
            Ok((200, seen(backend, &me, &personal(backend).await?).await?))
        }
        ("GET", ["calendars", "for", kind, name @ ..]) if !name.is_empty() => {
            let resource = normalised(&format!("{kind}:{}", name.join("/")))?;
            let calendar = ensure(backend, &mut sight, &resource, &label).await?;
            Ok((200, seen(backend, &me, &calendar).await?))
        }
        ("GET", ["calendars", id]) => {
            let calendar = found(&store, &mut sight, id).await?;
            Ok((200, seen(backend, &me, &calendar).await?))
        }
        ("PATCH", ["calendars", id]) => {
            let calendar = found(&store, &mut sight, id).await?;
            let calendar = changed(backend, calendar, body(request)?).await?;
            Ok((200, seen(backend, &me, &calendar).await?))
        }
        ("DELETE", ["calendars", id]) => {
            let calendar = found(&store, &mut sight, id).await?;
            if calendar.owner.is_some() {
                return Err(Refusal::bad(
                    "a person's own calendar stays; delete its events instead",
                ));
            }
            let _ = backend
                .queue("calendar-events", &format!("calendars/{}/delete", calendar.id), json!({}))
                .await;
            store.delete(calendar.id).await?;
            Ok((204, Value::Null))
        }
        ("GET", ["calendars", id, "agenda"]) => {
            let calendar = found(&store, &mut sight, id).await?;
            let (from, to) = range(request)?;
            let listed = occurrences(backend, std::slice::from_ref(&calendar), &from, &to).await?;
            Ok((200, json!({ "from": from, "to": to, "occurrences": listed })))
        }
        ("GET", ["agenda"]) => {
            let (from, to) = range(request)?;
            let calendars = theirs(backend, &mut sight).await?;
            let listed = occurrences(backend, &calendars, &from, &to).await?;
            let shown: Vec<Value> = calendars.iter().map(|calendar| json!({ "id": calendar.id, "name": calendar.name, "resource": calendar.resource })).collect();
            Ok((200, json!({ "from": from, "to": to, "calendars": shown, "occurrences": listed })))
        }
        ("GET", ["subscriptions"]) => {
            let by_default = teams_of(backend, &me).await?;
            let listed_calendars = mine(backend, &me).await?;
            let ids: Vec<Uuid> = listed_calendars.iter().map(|calendar| calendar.id).collect();
            let mut listed = Vec::new();
            for calendar in listed_calendars.iter().filter(|one| one.owner.is_none()) {
                let visible = sight.sees(calendar).await;
                let mut entry = shown(calendar, &ids, &by_default);
                entry["visible"] = json!(visible);
                listed.push(entry);
            }
            Ok((200, json!(listed)))
        }
        ("POST", ["subscriptions"]) => {
            let asked: Subscription = body(request)?;
            let calendar = found(&store, &mut sight, &asked.calendar.to_string()).await?;
            let by_default = teams_of(backend, &me).await?.contains(&calendar.resource);
            store.subscribe(&me, calendar.id, asked.subscribed, by_default).await?;
            Ok((200, seen(backend, &me, &calendar).await?))
        }
        ("GET", ["feeds"]) => {
            let names: BTreeMap<Uuid, String> = store
                .calendars()
                .await?
                .into_iter()
                .map(|calendar| (calendar.id, calendar.name))
                .collect();
            let listed: Vec<Value> =
                store.feeds(&me).await?.iter().map(|feed| feed_shown(feed, &names)).collect();
            Ok((200, json!(listed)))
        }
        ("POST", ["feeds"]) => {
            let bytes = if request.body.is_empty() { &b"{}"[..] } else { &request.body[..] };
            let asked: Value = serde_json::from_slice(bytes)
                .map_err(|err| Refusal::bad(format!("the body is not what this takes: {err}")))?;
            Ok((201, feeds_add(backend, &mut sight, asked).await?))
        }
        ("POST", ["feeds", id, "rotate"]) => Ok((200, feeds_rotate(backend, id).await?)),
        ("POST", ["feeds", id, "delete"]) => {
            let id: Uuid = id.parse().map_err(|_| Refusal::missing("there is no such feed"))?;
            store
                .feed(id)
                .await?
                .filter(|feed| feed.principal == me)
                .ok_or_else(|| Refusal::missing("there is no such feed"))?;
            store.delete_feed(id).await?;
            Ok((204, Value::Null))
        }
        _ => Err(Refusal::missing("no such route")),
    }
}

/// A person's own calendar, then each shared one they can see.
pub async fn visible_calendars(
    backend: &Backend,
    sight: &mut Sight<'_>,
) -> Result<Vec<Value>, Refusal> {
    let (me, _) = principal(backend)?;
    let store = Store(backend);
    let following = following(backend, &me).await?;
    let by_default = teams_of(backend, &me).await?;
    let mut listed = vec![shown(&personal(backend).await?, &following, &by_default)];
    for calendar in store.calendars().await? {
        if calendar.owner.is_none() && sight.sees(&calendar).await {
            listed.push(shown(&calendar, &following, &by_default));
        }
    }
    Ok(listed)
}

/// The caller's feed of one calendar they can see, or of everything they subscribe to.
pub async fn feeds_add(
    backend: &Backend,
    sight: &mut Sight<'_>,
    asked: Value,
) -> Result<Value, Refusal> {
    let asked: NewFeed = serde_json::from_value(asked)
        .map_err(|err| Refusal::bad(format!("the body is not what this takes: {err}")))?;
    let (me, label) = principal(backend)?;
    let store = Store(backend);
    let mut names = BTreeMap::new();
    if let Some(id) = asked.calendar {
        let calendar = found(&store, sight, &id.to_string()).await?;
        names.insert(calendar.id, calendar.name);
    }
    let feed = Feed {
        id: Uuid::now_v7(),
        principal: me,
        label,
        calendar: asked.calendar,
        token: secret()?,
        created_at: String::new(),
        rotated_at: None,
    };
    let feed = store.ensure_feed(&feed).await?;
    Ok(feed_shown(&feed, &names))
}

/// A new secret for one of the caller's feeds, so the old URL stops working.
pub async fn feeds_rotate(backend: &Backend, id: &str) -> Result<Value, Refusal> {
    let (me, _) = principal(backend)?;
    let store = Store(backend);
    let id: Uuid = id.parse().map_err(|_| Refusal::missing("there is no such feed"))?;
    let feed = store
        .feed(id)
        .await?
        .filter(|feed| feed.principal == me)
        .ok_or_else(|| Refusal::missing("there is no such feed"))?;
    let fresh = secret()?;
    store.rotate(id, &fresh).await?;
    let names: BTreeMap<Uuid, String> =
        store.calendars().await?.into_iter().map(|calendar| (calendar.id, calendar.name)).collect();
    let rotated = Feed { token: fresh, rotated_at: Some(chrono::Utc::now().to_rfc3339()), ..feed };
    Ok(feed_shown(&rotated, &names))
}

pub async fn found(
    store: &Store<'_>,
    sight: &mut Sight<'_>,
    id: &str,
) -> Result<Calendar, Refusal> {
    let calendar = store
        .calendar(id_of(id)?)
        .await?
        .ok_or_else(|| Refusal::missing("there is no such calendar"))?;
    sight.visible(calendar).await
}

async fn changed(
    backend: &Backend,
    mut calendar: Calendar,
    change: Change,
) -> Result<Calendar, Refusal> {
    if let Some(name) = change.name {
        let name = name.trim().to_string();
        if name.is_empty() || name.chars().count() > 120 {
            return Err(Refusal::bad("a calendar's name is 1 to 120 characters"));
        }
        calendar.name = name;
    }
    if let Some(description) = change.description {
        calendar.description = description.trim().chars().take(2000).collect();
    }
    if let Some(zone) = change.timezone {
        calendar.timezone = checked_zone(zone.trim())?;
    }
    Store(backend).update(&calendar).await?;
    Ok(calendar)
}

/// The plugins an administrator lets put events on people's own calendars, such as `rota` for the
/// shifts someone is on.
pub const PERSONAL_FROM: &str = "personal-events-from";

/// A person's own calendar, for a plugin allowed to put their events there. Anyone else's plugin
/// is refused: a person's calendar is theirs, apart from what an administrator lets in.
async fn personal_for(backend: &Backend, plugin: &str, login: &str) -> Result<Calendar, Refusal> {
    if !backend.settings().list(PERSONAL_FROM).iter().any(|allowed| allowed == plugin) {
        return Err(Refusal::forbidden(format!(
            "a person's own calendar is theirs alone, unless an administrator adds {plugin} to \
             Calendar's personal-events-from setting"
        )));
    }
    let users: Vec<Value> = backend
        .query_all(
            Query::new("core.users").filter(json!({ "login": login })).fields(&["id", "login"]),
        )
        .await
        .map_err(|err| Refusal::unavailable(format!("people could not be read: {err}")))?;
    let Some(id) = users.first().and_then(|user| user["id"].as_str()) else {
        return Err(Refusal::missing(format!("nobody signs in as {login}")));
    };
    let owner = format!("user:{id}");
    let store = Store(backend);
    if let Some(found) = store.for_owner(&owner).await? {
        return Ok(found);
    }
    let calendar = Calendar {
        id: Uuid::now_v7(),
        resource: format!("user:{login}"),
        name: format!("{login}'s calendar"),
        description: String::new(),
        timezone: "UTC".into(),
        owner: Some(owner),
        created_by: format!("plugin:{plugin}"),
        created_at: String::new(),
        updated_at: String::new(),
    };
    store.ensure(&calendar).await
}

/// Another plugin, such as Process or Watercooler, finding or making a resource's calendar.
async fn discovery(backend: &Backend, request: &Request, path: &[&str]) -> Answer {
    let plugin = match backend.caller() {
        Some(Caller { kind, id: Some(id), .. }) if kind == "plugin" => id.clone(),
        _ => return Err(Refusal::forbidden("discovery routes are for plugins")),
    };
    let store = Store(backend);
    match (request.method.as_str(), path) {
        ("GET", ["calendars", "for", kind, name @ ..]) if !name.is_empty() => {
            let resource = normalised(&format!("{kind}:{}", name.join("/")))?;
            if let Some(login) = resource.strip_prefix("user:") {
                // Events reach here through calendar-events, which says which plugin made them.
                let maker = match plugin.as_str() {
                    "calendar-events" => query(request, "for").unwrap_or(plugin.clone()),
                    _ => plugin.clone(),
                };
                return Ok((200, shown(&personal_for(backend, &maker, login).await?, &[], &[])));
            }
            if let Some(found) = store.for_resource(&resource).await? {
                return Ok((200, shown(&found, &[], &[])));
            }
            let name =
                resource.split_once(':').map_or(resource.as_str(), |(_, name)| name).to_string();
            let calendar = Calendar {
                id: Uuid::now_v7(),
                resource,
                name,
                description: String::new(),
                timezone: "UTC".into(),
                owner: None,
                created_by: format!("plugin:{plugin}"),
                created_at: String::new(),
                updated_at: String::new(),
            };
            Ok((200, shown(&store.ensure(&calendar).await?, &[], &[])))
        }
        ("GET", ["calendars", id]) => {
            let calendar = store
                .calendar(id_of(id)?)
                .await?
                .ok_or_else(|| Refusal::missing("there is no such calendar"))?;
            Ok((
                200,
                json!({ "id": calendar.id, "resource": calendar.resource, "name": calendar.name, "timezone": calendar.timezone, "owner": calendar.owner }),
            ))
        }
        _ => Err(Refusal::missing("no such route")),
    }
}

/// A feed, fetched by a calendar app with nothing but its secret URL.
async fn feed(backend: &Backend, file: &str) -> Response {
    let refused =
        || Response::new(404, "text/plain; charset=utf-8", "there is no such feed".to_string());
    let Some(token) = file.strip_suffix(".ics").filter(|token| token.len() == 64) else {
        return refused();
    };
    let store = Store(backend);
    let Ok(Some(feed)) = store.feed_by_token(token).await else { return refused() };
    let calendars: Vec<Calendar> = match feed.calendar {
        Some(id) => store.calendar(id).await.ok().flatten().into_iter().collect(),
        None => mine(backend, &feed.principal).await.unwrap_or_default(),
    };
    let name = match (&feed.calendar, calendars.first()) {
        (Some(_), Some(calendar)) => calendar.name.clone(),
        _ => format!("{}'s calendars", feed.label),
    };
    let ids: Vec<String> = calendars.iter().map(|calendar| calendar.id.to_string()).collect();
    let events = match ids.is_empty() {
        true => Vec::new(),
        false => match backend
            .discovery(
                "calendar-events",
                "GET",
                "events",
                Some(&format!("calendars={}", ids.join(","))),
                None,
            )
            .await
        {
            Ok((200, answer)) => answer["events"].as_array().cloned().unwrap_or_default(),
            Ok((status, _)) => {
                tracing::warn!(status, "calendar-events refused a feed's events");
                return Response::new(
                    503,
                    "text/plain; charset=utf-8",
                    "the calendar's events are unavailable".to_string(),
                );
            }
            Err(err) => {
                tracing::warn!(%err, "calendar-events could not be asked for a feed");
                return Response::new(
                    503,
                    "text/plain; charset=utf-8",
                    "the calendar's events are unavailable".to_string(),
                );
            }
        },
    };
    Response::new(200, "text/calendar; charset=utf-8", ics::calendar(&name, &events))
        .with_header("cache-control", "private, max-age=300")
}
