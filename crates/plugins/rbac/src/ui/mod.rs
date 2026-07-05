//! The plugin's pages, shown by the frontend at `/p/rbac/...`. Every form posts through HTMX, which
//! carries the page's CSRF token, and gets the whole page back with a notice or the refusal.

mod wizard;

use std::collections::BTreeMap;

use askama::Template;
use doc_permissions::MemberKind;
use doc_plugin_sdk::{Backend, Request, Response};
use serde_json::{Value, json};

use crate::api::query;
use crate::model::{
    Assignment, Conditions, GroupRecord, GroupRef, Holder, Refusal, Rule, RuleGrants,
};
use crate::offboarding::OffboardingRule;
use crate::ops::{self, RuleChange};
use crate::store::Store;

#[derive(Default)]
pub struct Flash {
    pub notice: Option<String>,
    pub error: Option<String>,
}

impl Flash {
    fn done(notice: impl Into<String>) -> Self {
        Self { notice: Some(notice.into()), error: None }
    }

    fn refused(refusal: &Refusal) -> Self {
        Self { notice: None, error: Some(refusal.detail.clone()) }
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

fn fields(form: &Form, name: &str) -> Vec<String> {
    form.iter()
        .filter(|(key, value)| key == name && !value.trim().is_empty())
        .map(|(_, value)| value.trim().to_string())
        .collect()
}

/// `key=value` pairs separated by commas, as the rule form writes attributes.
fn pairs(text: &str) -> Result<BTreeMap<String, String>, Refusal> {
    text.split(',')
        .map(str::trim)
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair
                .split_once('=')
                .ok_or_else(|| Refusal::bad(format!("`{pair}` is not written key=value")))?;
            Ok((key.trim().to_string(), value.trim().to_string()))
        })
        .collect()
}

fn written(pairs: &BTreeMap<String, String>) -> String {
    pairs.iter().map(|(key, value)| format!("{key}={value}")).collect::<Vec<_>>().join(", ")
}

fn kind_of(text: &str) -> Result<MemberKind, Refusal> {
    match text {
        "user" => Ok(MemberKind::User),
        "service" => Ok(MemberKind::Service),
        other => Err(Refusal::bad(format!("`{other}` is not user or service"))),
    }
}

/// A user by login, written `provider/login` when several providers share it.
async fn user_named(store: &Store<'_>, login: &str) -> Result<Holder, Refusal> {
    let (provider, login) = match login.split_once('/') {
        Some((provider, login)) => (Some(provider), login),
        None => (None, login),
    };
    let found = store.users(login).await?;
    let matching: Vec<&Value> = found
        .iter()
        .filter(|user| provider.is_none_or(|provider| user["provider"] == provider))
        .collect();
    match matching.as_slice() {
        [one] => one["id"]
            .as_str()
            .and_then(|id| id.parse().ok())
            .map(|id| Holder::User { id })
            .ok_or_else(|| Refusal::unavailable("a user's ID could not be read")),
        [] => Err(Refusal::missing(format!("nobody signs in as {login}"))),
        _ => {
            Err(Refusal::bad(format!("several providers sign in {login}; write provider/{login}")))
        }
    }
}

/// How many people a member field offers at once.
const OFFERED: usize = 10;

/// One person or service account a member field offers: the name a form takes, and who they are.
pub struct MemberChoice {
    pub value: String,
    pub label: String,
    pub hint: String,
}

#[derive(Template)]
#[template(path = "member_options.html")]
struct MemberOptionsFragment {
    typed: String,
    found: Vec<MemberChoice>,
    more: bool,
    people: bool,
}

/// What a member field offers: the people or service accounts whose name holds what is being
/// typed, closest first, leaving out the ones the field already names. The field may name
/// several, separated by commas, so only the part after the last comma is being typed.
async fn member_options(store: &Store<'_>, request: &Request) -> Result<String, Refusal> {
    let kind = kind_of(query(request, "kind").as_deref().unwrap_or("user"))?;
    let written =
        query(request, "members").or_else(|| query(request, "member")).unwrap_or_default();
    let (named, typed) = match written.rsplit_once(',') {
        Some((before, last)) => (before.to_string(), last.trim().to_lowercase()),
        None => (String::new(), written.trim().to_lowercase()),
    };
    let mut already: Vec<String> = named
        .split(',')
        .map(|name| name.trim().to_lowercase())
        .filter(|name| !name.is_empty())
        .collect();
    // Somebody already in the group is not worth offering to add to it again.
    if let Some((plugin, name)) = query(request, "in").as_deref().and_then(|id| id.split_once('/'))
        && let Some(group) = store.group(plugin, name).await?
    {
        let members = store.members(&group).await?;
        already.extend(members.iter().filter_map(|member| member.label.clone()));
    }
    already.iter_mut().for_each(|name| *name = name.to_lowercase());
    let wanted = match kind {
        MemberKind::User => "user",
        MemberKind::Service => "service",
    };
    let people = store.people().await?;
    // A login two providers share is offered as `provider/login`, which is how one is named.
    let mut logins: BTreeMap<String, usize> = BTreeMap::new();
    for person in people.iter().filter(|person| person["kind"] == wanted) {
        *logins.entry(text_of(person, "name").to_lowercase()).or_default() += 1;
    }
    let mut found: Vec<(u8, String, MemberChoice)> = Vec::new();
    for person in people.iter().filter(|person| person["kind"] == wanted) {
        let name = text_of(person, "name");
        let provider = text_of(person, "provider");
        let shared = logins.get(&name.to_lowercase()).is_some_and(|many| *many > 1);
        let value = match shared && !provider.is_empty() {
            true => format!("{provider}/{name}"),
            false => name.clone(),
        };
        let held = name.to_lowercase();
        if already.contains(&value.to_lowercase()) || already.contains(&held) {
            continue;
        }
        // What is typed at the start of a name comes before what is in the middle of one; with
        // nothing typed they are all as close as each other and sort by name.
        let closeness: u8 = if typed.is_empty() || held.starts_with(&typed) {
            0
        } else if held.contains(&typed) {
            1
        } else {
            continue;
        };
        let mut hint = match person["kind"] == "service" {
            true => "service account".to_string(),
            false => provider,
        };
        if person["disabled"] == true {
            hint = match hint.is_empty() {
                true => "disabled".to_string(),
                false => format!("{hint} · disabled"),
            };
        }
        found.push((closeness, held, MemberChoice { value, label: name, hint }));
    }
    found.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    let more = found.len() > OFFERED;
    let found = found.into_iter().take(OFFERED).map(|(_, _, choice)| choice).collect();
    render(&MemberOptionsFragment { typed, found, more, people: matches!(kind, MemberKind::User) })
}

#[derive(Template)]
#[template(path = "permission_options.html")]
struct PermissionOptionsFragment {
    typed: String,
    found: Vec<MemberChoice>,
    more: bool,
}

/// What a permission is, in a few words beside it: what its scope allows, and whether it is one
/// of a plugin's own or a group's membership.
fn permission_hint(permission: &str) -> String {
    let scope = match permission.rsplit(':').next() {
        Some("ro") => "read",
        Some("rw") => "read and write",
        Some("wo") => "write only",
        _ => "",
    };
    let parts: Vec<&str> = permission.split(':').collect();
    match parts.get(2).copied() {
        Some("group") => "group".to_string(),
        Some("pluginuser" | "pluginservice") => format!("custom, {scope}"),
        _ => scope.to_string(),
    }
}

/// What a permission field offers while it is typed into, on the platform's own grant forms as
/// much as here: every permission a holder of that kind can be given in any plugin — its scopes,
/// the plugin's custom ones and its groups — those starting with what is typed first.
async fn permission_options(store: &Store<'_>, request: &Request) -> Result<String, Refusal> {
    let kind = kind_of(query(request, "kind").as_deref().unwrap_or("service"))?;
    let typed = query(request, "permission").unwrap_or_default().trim().to_lowercase();
    let known = store.known().await?;
    let groups = store.groups(None).await?;
    let mut found: Vec<(u8, String)> = Vec::new();
    for plugin in known.names() {
        for choice in ops::choices(&known, &groups, &plugin, kind, true) {
            let held = choice.to_lowercase();
            // A plugin's name is what people type first, so `kb` reads as `plugin:kb`.
            let named = held.strip_prefix("plugin:").is_some_and(|rest| rest.starts_with(&typed));
            let closeness: u8 = if typed.is_empty() || held.starts_with(&typed) || named {
                0
            } else if held.contains(&typed) {
                1
            } else {
                continue;
            };
            found.push((closeness, choice));
        }
    }
    found.sort();
    found.dedup();
    let more = found.len() > OFFERED;
    let found = found
        .into_iter()
        .take(OFFERED)
        .map(|(_, permission)| MemberChoice {
            hint: permission_hint(&permission),
            label: permission.clone(),
            value: permission,
        })
        .collect();
    render(&PermissionOptionsFragment { typed, found, more })
}

fn text_of(person: &Value, key: &str) -> String {
    person[key].as_str().unwrap_or_default().to_string()
}

async fn member_named(store: &Store<'_>, kind: MemberKind, name: &str) -> Result<Holder, Refusal> {
    match kind {
        MemberKind::User => user_named(store, name).await,
        MemberKind::Service => store
            .service_account(name)
            .await?
            .map(|id| Holder::Service { id })
            .ok_or_else(|| Refusal::missing(format!("no service account called {name}"))),
    }
}

pub async fn handle(backend: &Backend, request: &Request, path: &[&str]) -> Response {
    let store = Store(backend);
    let form = form(request);
    // Where the address bar goes when a form lands somewhere other than where it was.
    let mut pushed: Option<String> = None;
    let page = match (request.method.as_str(), path) {
        ("GET", [] | ["groups"]) => {
            groups(&store, query(request, "plugin"), Flash::default()).await
        }
        ("GET", ["groups", "new"]) => wizard::start(&store).await,
        ("POST", ["groups", "new"]) => wizard::answer(backend, &store, &form).await,
        ("GET", ["choices"]) => choices(&store, request).await,
        ("GET", ["member-options"]) => member_options(&store, request).await,
        ("GET", ["permission-options"]) => permission_options(&store, request).await,
        ("GET", ["groups", plugin, name]) => {
            group(&store, plugin, name, "permissions", shown(request), Flash::default()).await
        }
        ("GET", ["groups", plugin, name, section @ ("members" | "attributes")]) => {
            let section = if *section == "members" { "members" } else { "attributes" };
            group(&store, plugin, name, section, Vec::new(), Flash::default()).await
        }
        ("GET", ["groups", plugin, name, adding @ ("members" | "attributes"), "new"]) => {
            let adding = if *adding == "members" { "members" } else { "attributes" };
            group_add(&store, plugin, name, adding, &Form::new(), Flash::default()).await
        }
        ("POST", ["groups", plugin, name, action @ ..]) => {
            let (page, url) = group_change(backend, &store, plugin, name, action, &form).await;
            pushed = url;
            page
        }
        ("GET", ["people"]) => people(&store).await,
        ("GET", ["principals", kind, id]) => {
            let tab = Tab::named(query(request, "tab").as_deref());
            principal(&store, kind, id, tab, query(request, "plugin"), Flash::default()).await
        }
        ("GET", ["principals", kind, id, "attributes", "new"]) => {
            principal_attribute(&store, kind, id, &Form::new(), Flash::default()).await
        }
        ("POST", ["principals", kind, id, action @ ..]) => {
            let (page, url) = principal_change(backend, &store, kind, id, action, &form).await;
            pushed = url;
            page
        }
        ("GET", ["access-map"]) => access_page(backend, request).await,
        ("GET", ["access-map", "choices"]) => {
            let typed = query(request, "resource").unwrap_or_default();
            let mut choices =
                ResourceChoices::of(crate::access_map::choices(backend, &typed).await);
            choices.typed = typed;
            render(&choices)
        }
        ("GET", ["access-map", "panel"]) => {
            let resource = query(request, "resource").unwrap_or_default();
            drawn(crate::access_map::resource(backend, &resource).await)
        }
        ("GET", ["access-map", "principal", kind, id]) => match ops::holder_of(kind, id) {
            Ok(holder) => drawn(crate::access_map::principal(backend, &store, &holder).await),
            Err(refusal) => Err(refusal),
        },
        ("GET", ["offboarding"]) => offboarding(&store, Flash::default()).await,
        ("GET", ["offboarding", "new"]) => new_offboarding(&store, &Form::new(), None).await,
        ("POST", ["offboarding"]) => {
            let (page, url) = add_offboarding(backend, &store, &form).await;
            pushed = url;
            page
        }
        ("POST", ["offboarding", id, action @ ..]) => {
            change_offboarding(backend, &store, id, action, &form).await
        }
        ("GET", ["rules"]) => rules(&store, Flash::default()).await,
        ("GET", ["rules", "new"]) => new_rule(&store, &Form::new(), None).await,
        ("POST", ["rules"]) => {
            let (page, url) = create_rule(backend, &store, &form).await;
            pushed = url;
            page
        }
        ("GET", ["rules", id]) => rule(&store, id, None, Flash::default()).await,
        ("GET", ["rules", id, "try"]) => {
            rule_section(&store, id, true, None, Flash::default()).await
        }
        ("POST", ["rules", id, action @ ..]) => {
            rule_change(backend, &store, id, action, &form).await
        }
        _ => Err(Refusal::missing("no such page")),
    };
    match page {
        Ok(html) => match pushed {
            Some(url) => Response::html(html).with_header("hx-push-url", &url),
            None => Response::html(html),
        },
        Err(refusal) => {
            let page = Blank { flash: Flash::refused(&refusal) };
            let html = page.render().unwrap_or_else(|_| refusal.detail.clone());
            Response::new(refusal.status, "text/html; charset=utf-8", html)
        }
    }
}

#[derive(Template)]
#[template(path = "access_map.html")]
struct AccessMapPage {
    flash: Flash,
    resource: String,
    map: Option<MapFragment>,
    choices: Option<ResourceChoices>,
}

#[derive(Template)]
#[template(path = "map.html")]
struct MapFragment {
    error: Option<String>,
    svg: String,
    notes: Vec<String>,
}

impl MapFragment {
    fn of(drawn: Result<crate::access_map::Drawn, Refusal>) -> Self {
        match drawn {
            Ok(drawn) => Self { error: None, svg: drawn.svg, notes: drawn.notes },
            Err(refusal) => {
                Self { error: Some(refusal.detail), svg: String::new(), notes: Vec::new() }
            }
        }
    }
}

/// A map on its own, for a resource page's panel or a principal's page; a refusal is said in it.
fn drawn(drawn: Result<crate::access_map::Drawn, Refusal>) -> Result<String, Refusal> {
    render(&MapFragment::of(drawn))
}

async fn access_page(backend: &Backend, request: &Request) -> Result<String, Refusal> {
    let resource = query(request, "resource").unwrap_or_default();
    let (map, choices) = match resource.is_empty() {
        true => (None, Some(ResourceChoices::of(crate::access_map::choices(backend, "").await))),
        false => {
            (Some(MapFragment::of(crate::access_map::resource(backend, &resource).await)), None)
        }
    };
    render(&AccessMapPage { flash: Flash::default(), resource, map, choices })
}

/// Resources to map, narrowed as the viewer types in the resource field.
#[derive(Template)]
#[template(path = "map_choices.html")]
struct ResourceChoices {
    error: Option<String>,
    kinds: Vec<crate::access_map::Kinded>,
    typed: String,
}

impl ResourceChoices {
    fn of(chosen: Result<Vec<crate::access_map::Kinded>, Refusal>) -> Self {
        match chosen {
            Ok(kinds) => Self { error: None, kinds, typed: String::new() },
            Err(refusal) => {
                Self { error: Some(refusal.detail), kinds: Vec::new(), typed: String::new() }
            }
        }
    }
}

fn render<T: Template>(page: &T) -> Result<String, Refusal> {
    page.render().map_err(|err| Refusal::unavailable(format!("the page could not be drawn: {err}")))
}

#[derive(Template)]
#[template(path = "blank.html")]
struct Blank {
    flash: Flash,
}

#[derive(Template)]
#[template(path = "groups.html")]
struct GroupsPage {
    flash: Flash,
    plugins: Vec<String>,
    filter: Option<String>,
    groups: Vec<GroupRecord>,
}

async fn groups(
    store: &Store<'_>,
    filter: Option<String>,
    flash: Flash,
) -> Result<String, Refusal> {
    let filter = filter.filter(|plugin| !plugin.is_empty());
    let plugins = store.known().await?.names();
    // A group may grant in plugins other than the one it is listed under, so the filter is what
    // each grants in rather than the column it is stored under.
    let mut groups = store.groups(None).await?;
    if let Some(plugin) = &filter {
        groups.retain(|group| group.plugins().contains(plugin));
    }
    render(&GroupsPage { flash, plugins, filter, groups })
}

#[derive(Template)]
#[template(path = "choices.html")]
struct ChoicesFragment {
    choices: Vec<String>,
    select: bool,
}

/// The permissions a picker may offer, redrawn whenever the plugin or member kind changes.
async fn choices(store: &Store<'_>, request: &Request) -> Result<String, Refusal> {
    let plugin = query(request, "plugin").unwrap_or_else(|| "core".into());
    let kind = kind_of(query(request, "kind").as_deref().unwrap_or("user"))?;
    let memberships = query(request, "memberships").is_some_and(|value| value == "yes");
    let known = store.known().await?;
    let groups = store.groups(Some(&plugin)).await?;
    let choices = ops::choices(&known, &groups, &plugin, kind, memberships);
    render(&ChoicesFragment {
        choices,
        select: query(request, "as").is_some_and(|value| value == "select"),
    })
}

#[derive(Template)]
#[template(path = "group.html")]
struct GroupPage {
    flash: Flash,
    /// The section open: `permissions`, `members` or `attributes`.
    section: &'static str,
    group: GroupRecord,
    members: Vec<Assignment>,
    attributes: BTreeMap<String, String>,
    /// One fieldset for each plugin it grants in, and each the viewer has asked to see as well.
    offers: Vec<wizard::Offer>,
    /// The plugins it does not grant in yet, to add a fieldset for.
    others: Vec<String>,
    /// The ones being shown although the group grants nothing in them, carried by the forms.
    shown: Vec<String>,
}

impl GroupPage {
    fn url(&self) -> String {
        format!("/p/rbac/groups/{}/{}", self.group.plugin, self.group.name)
    }

    fn attributes_url(&self) -> String {
        format!("{}/attributes", self.url())
    }

    /// The sections, each a page of its own: what it grants, who is in it and its attributes.
    fn sections(&self) -> Vec<(&'static str, &'static str, String)> {
        let url = self.url();
        vec![
            ("permissions", "Permissions", url.clone()),
            ("members", "Members", format!("{url}/members")),
            ("attributes", "Attributes", format!("{url}/attributes")),
        ]
    }
}

/// Adding a member or setting an attribute on a group, on a page of its own, with what was typed
/// when it was refused.
#[derive(Template)]
#[template(path = "group_add.html")]
struct GroupAddPage {
    flash: Flash,
    /// `members` or `attributes`.
    adding: &'static str,
    group: GroupRecord,
    form: Form,
}

impl GroupAddPage {
    fn url(&self) -> String {
        format!("/p/rbac/groups/{}/{}", self.group.plugin, self.group.name)
    }

    fn attributes_url(&self) -> String {
        format!("{}/attributes", self.url())
    }

    fn typed(&self, name: &str) -> String {
        field(&self.form, name)
    }
}

async fn group_add(
    store: &Store<'_>,
    plugin: &str,
    name: &str,
    adding: &'static str,
    form: &Form,
    flash: Flash,
) -> Result<String, Refusal> {
    let group = ops::found_group(store, plugin, name).await?;
    render(&GroupAddPage { flash, adding, group, form: form.clone() })
}

/// Plugins a page was asked to show permissions for although the group grants nothing in them.
fn shown(request: &Request) -> Vec<String> {
    query(request, "show")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|plugin| !plugin.is_empty())
        .map(str::to_string)
        .collect()
}

async fn group_page(
    store: &Store<'_>,
    plugin: &str,
    name: &str,
    flash: Flash,
) -> Result<String, Refusal> {
    group(store, plugin, name, "permissions", Vec::new(), flash).await
}

async fn group(
    store: &Store<'_>,
    plugin: &str,
    name: &str,
    section: &'static str,
    shown: Vec<String>,
    flash: Flash,
) -> Result<String, Refusal> {
    let group = ops::found_group(store, plugin, name).await?;
    let members = store.members(&group).await?;
    let attributes =
        store.attributes(&Holder::Group { plugin: plugin.into(), name: name.into() }).await?;
    let offers = wizard::editing(store, &group, &shown).await?;
    let others = store
        .known()
        .await?
        .names()
        .into_iter()
        .filter(|plugin| !offers.iter().any(|offer| offer.plugin == *plugin))
        .collect();
    let shown = shown.into_iter().filter(|plugin| !group.plugins().contains(plugin)).collect();
    render(&GroupPage { flash, section, group, members, attributes, offers, others, shown })
}

/// Changes a group, then shows the section the change was made in, with where the address bar
/// goes; a refused addition shows its form again, with why.
async fn group_change(
    backend: &Backend,
    store: &Store<'_>,
    plugin: &str,
    name: &str,
    action: &[&str],
    form: &Form,
) -> (Result<String, Refusal>, Option<String>) {
    let section: &'static str = match action.first() {
        Some(&"members") => "members",
        Some(&"attributes") => "attributes",
        _ => "permissions",
    };
    let adding = matches!(action, ["members"] | ["attributes"]);
    let page = group_changed(backend, store, plugin, name, action, form).await;
    match page {
        Ok(Ok(notice)) => {
            let url = match section {
                "permissions" => format!("/p/rbac/groups/{plugin}/{name}"),
                section => format!("/p/rbac/groups/{plugin}/{name}/{section}"),
            };
            let flash = Flash::done(notice);
            (group(store, plugin, name, section, Vec::new(), flash).await, Some(url))
        }
        Ok(Err(refusal)) if adding => {
            let flash = Flash::refused(&refusal);
            (group_add(store, plugin, name, section, form, flash).await, None)
        }
        Ok(Err(refusal)) => {
            let flash = Flash::refused(&refusal);
            (group(store, plugin, name, section, Vec::new(), flash).await, None)
        }
        Err(page) => (page, Some(format!("/p/rbac/groups/{plugin}"))),
    }
}

/// What a change to a group said, or why it was refused; or, for a deletion, the page to show.
async fn group_changed(
    backend: &Backend,
    store: &Store<'_>,
    plugin: &str,
    name: &str,
    action: &[&str],
    form: &Form,
) -> Result<Result<String, Refusal>, Result<String, Refusal>> {
    if action == ["delete"] {
        return match ops::delete_group(backend, store, plugin, name).await {
            Ok(()) => {
                let notice = Flash::done(format!("Deleted {plugin}/{name}."));
                Err(groups(store, Some(plugin.to_string()), notice).await)
            }
            Err(refusal) => Ok(Err(refusal)),
        };
    }
    let holder = Holder::Group { plugin: plugin.into(), name: name.into() };
    let result: Result<String, Refusal> = async {
        match action {
            ["permissions"] => {
                let permissions = fields(form, "permission");
                let updated =
                    ops::update_group(backend, store, plugin, name, None, Some(&permissions))
                        .await?;
                Ok(match permissions.is_empty() {
                    true => format!(
                        "Saved. Nothing is ticked, so it grants {plugin}'s read-only default."
                    ),
                    false => format!(
                        "Saved: {} in {}.",
                        updated.permissions.len(),
                        updated.plugins().join(", ")
                    ),
                })
            }
            ["members"] => {
                let kind = ops::found_group(store, plugin, name).await?.kind;
                let member = member_named(store, kind, &field(form, "member")).await?;
                ops::grant(backend, store, &member, &crate::model::membership(plugin, name))
                    .await?;
                Ok(format!("Added {} to the group.", field(form, "member")))
            }
            ["members", "remove"] => {
                let member = ops::holder_of(&field(form, "kind"), &field(form, "id"))?;
                ops::revoke(backend, store, &member, &crate::model::membership(plugin, name))
                    .await?;
                Ok("Removed them from the group.".into())
            }
            ["attributes"] => {
                ops::set_attribute(
                    backend,
                    store,
                    &holder,
                    &field(form, "key"),
                    &field(form, "value"),
                )
                .await?;
                Ok(format!("Set {}.", field(form, "key")))
            }
            ["attributes", "remove"] => {
                ops::remove_attribute(backend, store, &holder, &field(form, "key")).await?;
                Ok(format!("Removed {}.", field(form, "key")))
            }
            _ => Err(Refusal::missing("no such change")),
        }
    }
    .await;
    Ok(result)
}

struct Person {
    kind: String,
    id: String,
    name: String,
    detail: String,
    disabled: bool,
}

/// A team, which holds permissions of its own like a person does.
struct Team {
    id: String,
    title: String,
    name: String,
    provider: String,
    default: bool,
}

#[derive(Template)]
#[template(path = "people.html")]
struct PeoplePage {
    flash: Flash,
    users: Vec<Person>,
    accounts: Vec<Person>,
    teams: Vec<Team>,
}

async fn people(store: &Store<'_>) -> Result<String, Refusal> {
    let mut users = Vec::new();
    let mut accounts = Vec::new();
    for person in store.people().await? {
        let text = |key: &str| person[key].as_str().unwrap_or_default().to_string();
        let entry = Person {
            kind: text("kind"),
            id: text("id"),
            name: text("name"),
            detail: text("provider"),
            disabled: person["disabled"].as_bool().unwrap_or(false),
        };
        match entry.kind.as_str() {
            "user" => users.push(entry),
            _ => accounts.push(entry),
        }
    }
    let teams = store
        .teams()
        .await?
        .into_iter()
        .map(|team| {
            let text = |key: &str| team[key].as_str().unwrap_or_default().to_string();
            Team {
                id: text("id"),
                title: text("title"),
                name: text("name"),
                provider: text("provider"),
                default: team["default"].as_bool().unwrap_or(false),
            }
        })
        .collect();
    render(&PeoplePage { flash: Flash::default(), users, accounts, teams })
}

struct Reach {
    plugin: String,
    scope: String,
    custom: String,
}

/// A permission or group membership, shown with the plugin it belongs to.
struct Choice {
    permission: String,
    plugin: String,
}

/// A group the principal is in, for the details tab.
struct Membership {
    plugin: String,
    name: String,
}

/// A team somebody is in, or somebody in a team, for the details tab.
struct Tie {
    id: String,
    label: String,
    /// How they came to be there: by hand, or put there by a provider.
    how: String,
}

/// How somebody came to be in a team, as core records it: `admin`, `default` or `provider`.
fn how(record: &Value) -> String {
    let text = |key: &str| record[key].as_str().unwrap_or_default();
    match (text("source"), text("provider")) {
        ("provider", "" | "none") => "put there by a provider".to_string(),
        ("provider", provider) => format!("put there by {provider}"),
        ("default", _) => "everybody joins this team".to_string(),
        _ => "added by hand".to_string(),
    }
}

/// The principal page's tabs: who it is by default, and what it may do.
/// The sections of a person's, a team's or a service account's page, each a page of its own
/// with the contents beside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Groups,
    /// The teams a person is in, or the people in a team.
    Ties,
    Attributes,
    Map,
    Permissions,
}

impl Tab {
    fn named(name: Option<&str>) -> Self {
        match name {
            Some("permissions") => Self::Permissions,
            Some("teams" | "people") => Self::Ties,
            Some("attributes") => Self::Attributes,
            Some("map") => Self::Map,
            _ => Self::Groups,
        }
    }

    fn key(&self) -> &'static str {
        match self {
            Self::Groups => "groups",
            Self::Ties => "ties",
            Self::Attributes => "attributes",
            Self::Map => "map",
            Self::Permissions => "permissions",
        }
    }
}

/// Setting an attribute on a person, a team or a service account, on a page of its own.
#[derive(Template)]
#[template(path = "principal_attribute.html")]
struct PrincipalAttributePage {
    flash: Flash,
    kind: String,
    id: String,
    label: String,
    form: Form,
}

impl PrincipalAttributePage {
    fn url(&self) -> String {
        format!("/p/rbac/principals/{}/{}", self.kind, self.id)
    }

    fn attributes_url(&self) -> String {
        format!("{}/attributes", self.url())
    }

    fn typed(&self, name: &str) -> String {
        field(&self.form, name)
    }
}

async fn principal_attribute(
    store: &Store<'_>,
    kind: &str,
    id: &str,
    form: &Form,
    flash: Flash,
) -> Result<String, Refusal> {
    let holder = ops::holder_of(kind, id)?;
    let view = ops::principal(store, &holder).await?;
    let label = view["label"].as_str().unwrap_or_default().to_string();
    render(&PrincipalAttributePage {
        flash,
        kind: kind.to_string(),
        id: id.to_string(),
        label,
        form: form.clone(),
    })
}

fn plugin_of(permission: &str) -> String {
    permission.split(':').nth(1).unwrap_or_default().to_string()
}

#[derive(Template)]
#[template(path = "principal.html")]
struct PrincipalPage {
    flash: Flash,
    tab: Tab,
    kind: String,
    id: String,
    label: String,
    memberships: Vec<Membership>,
    /// The teams a person is in, or, on a team's page, the people in it.
    ties: Vec<Tie>,
    assignments: Vec<Assignment>,
    attributes: BTreeMap<String, String>,
    access: Vec<Reach>,
    plugins: Vec<String>,
    /// Empty for every plugin.
    plugin: String,
    not_granted: Vec<Choice>,
}

impl PrincipalPage {
    fn attributes_url(&self) -> String {
        format!("/p/rbac/principals/{}/{}/attributes", self.kind, self.id)
    }

    fn url(&self) -> String {
        format!("/p/rbac/principals/{}/{}", self.kind, self.id)
    }

    fn plugin_of(permission: &str) -> String {
        plugin_of(permission)
    }

    fn shown(&self, permission: &str) -> bool {
        self.plugin.is_empty() || plugin_of(permission) == self.plugin
    }

    fn on(&self, key: &str) -> bool {
        self.tab.key() == key
    }

    /// The sections, each a page of its own: a service account is in no team.
    fn sections(&self) -> Vec<(&'static str, &'static str, String)> {
        let url = self.url();
        let mut sections = vec![("groups", "Groups", url.clone())];
        match self.kind.as_str() {
            "team" => sections.push(("ties", "People in it", format!("{url}?tab=people"))),
            "user" => sections.push(("ties", "Teams", format!("{url}?tab=teams"))),
            _ => {}
        }
        sections.push(("attributes", "Attributes", format!("{url}?tab=attributes")));
        sections.push(("map", "Access map", format!("{url}?tab=map")));
        sections.push(("permissions", "Permissions", format!("{url}?tab=permissions")));
        sections
    }

    fn granted(&self) -> usize {
        self.assignments.iter().filter(|assignment| self.shown(&assignment.permission)).count()
    }
}

async fn principal(
    store: &Store<'_>,
    kind: &str,
    id: &str,
    tab: Tab,
    plugin: Option<String>,
    flash: Flash,
) -> Result<String, Refusal> {
    let holder = ops::holder_of(kind, id)?;
    let view = ops::principal(store, &holder).await?;
    let assignments: Vec<Assignment> =
        serde_json::from_value(view["assignments"].clone()).unwrap_or_default();
    let attributes: BTreeMap<String, String> =
        serde_json::from_value(view["attributes"].clone()).unwrap_or_default();
    let access = view["access"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(plugin, reach)| Reach {
            plugin: plugin.clone(),
            scope: reach["scope"].as_str().unwrap_or("none").to_string(),
            custom: reach["custom"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(name, scope)| format!("{name} {}", scope.as_str().unwrap_or_default()))
                .collect::<Vec<_>>()
                .join(", "),
        })
        .collect();
    let memberships = assignments
        .iter()
        .filter_map(|assignment| {
            let (plugin, name) =
                assignment.permission.strip_prefix("plugin:")?.split_once(":group:")?;
            Some(Membership { plugin: plugin.to_string(), name: name.to_string() })
        })
        .collect();
    // A team's page shows who is in it; a person's, the teams they are in. Either way it is the
    // same tie, seen from one end or the other (T67).
    let ties: Vec<Tie> = match &holder {
        Holder::Team { id } => store
            .team_members(*id)
            .await?
            .iter()
            .map(|person| Tie {
                id: format!("user/{}", person["id"].as_str().unwrap_or_default()),
                label: person["login"].as_str().unwrap_or_default().to_string(),
                how: how(person),
            })
            .collect(),
        Holder::User { id } => store
            .teams_of(*id)
            .await?
            .iter()
            .map(|team| Tie {
                id: format!("team/{}", team["id"].as_str().unwrap_or_default()),
                label: team["title"].as_str().unwrap_or_default().to_string(),
                how: how(team),
            })
            .collect(),
        _ => Vec::new(),
    };
    let known = store.known().await?;
    // `*` is offered beside the plugins, for read access to all of them at once (T67).
    let plugins = known.namable();
    let plugin = plugin.filter(|plugin| plugins.contains(plugin)).unwrap_or_default();
    let member_kind = holder.member_kind().unwrap_or(MemberKind::User);
    let held: Vec<&str> =
        assignments.iter().map(|assignment| assignment.permission.as_str()).collect();
    let mut not_granted = Vec::new();
    if tab == Tab::Permissions {
        let groups = store.groups(None).await?;
        let wanted: Vec<&String> =
            plugins.iter().filter(|name| plugin.is_empty() || **name == plugin).collect();
        for name in wanted {
            not_granted.extend(
                ops::choices(&known, &groups, name, member_kind, true)
                    .into_iter()
                    .filter(|choice| !held.contains(&choice.as_str()))
                    .map(|permission| Choice { permission, plugin: name.clone() }),
            );
        }
    }
    let label = view["label"].as_str().unwrap_or_default().to_string();
    render(&PrincipalPage {
        flash,
        tab,
        kind: kind.to_string(),
        id: id.to_string(),
        label,
        memberships,
        ties,
        assignments,
        attributes,
        access,
        plugins,
        plugin,
        not_granted,
    })
}

async fn principal_change(
    backend: &Backend,
    store: &Store<'_>,
    kind: &str,
    id: &str,
    action: &[&str],
    form: &Form,
) -> (Result<String, Refusal>, Option<String>) {
    let holder = match ops::holder_of(kind, id) {
        Ok(holder) => holder,
        Err(refusal) => return (Err(refusal), None),
    };
    let result: Result<String, Refusal> = async {
        match action {
            ["grants"] => {
                let (permission, _) =
                    ops::grant(backend, store, &holder, &field(form, "permission")).await?;
                Ok(format!("Granted {permission}."))
            }
            ["grants", "remove"] => {
                let permission =
                    ops::revoke(backend, store, &holder, &field(form, "permission")).await?;
                Ok(format!("Revoked {permission}."))
            }
            ["attributes"] => {
                ops::set_attribute(
                    backend,
                    store,
                    &holder,
                    &field(form, "key"),
                    &field(form, "value"),
                )
                .await?;
                Ok(format!("Set {}.", field(form, "key")))
            }
            ["attributes", "remove"] => {
                ops::remove_attribute(backend, store, &holder, &field(form, "key")).await?;
                Ok(format!("Removed {}.", field(form, "key")))
            }
            _ => Err(Refusal::missing("no such change")),
        }
    }
    .await;
    let plugin = Some(field(form, "plugin")).filter(|plugin| !plugin.is_empty());
    let flash = match result {
        Ok(notice) => Flash::done(notice),
        Err(refusal) => Flash::refused(&refusal),
    };
    let tab = match action.first() {
        Some(&"grants") => Tab::Permissions,
        _ => Tab::Attributes,
    };
    let url = format!("/p/rbac/principals/{kind}/{id}?tab={}", tab.key());
    match (action, flash.error.is_some()) {
        // A refused attribute goes back to its form, with what was typed.
        (["attributes"], true) => (principal_attribute(store, kind, id, form, flash).await, None),
        (_, true) => (principal(store, kind, id, tab, plugin, flash).await, None),
        (_, false) => (principal(store, kind, id, tab, plugin, flash).await, Some(url)),
    }
}

#[derive(Template)]
#[template(path = "offboarding.html")]
struct OffboardingPage {
    flash: Flash,
    rules: Vec<OffboardingRule>,
}

/// A new offboarding rule, on a page of its own, with whatever was typed when it was refused.
#[derive(Template)]
#[template(path = "offboarding_new.html")]
struct NewOffboardingPage {
    flash: Flash,
    /// The identity providers registered now, offered when writing a rule.
    providers: Vec<String>,
    form: Form,
}

impl NewOffboardingPage {
    fn value(&self, name: &str) -> String {
        field(&self.form, name)
    }

    /// Whether a box is ticked: as it was sent, or as a new rule starts.
    fn ticked(&self, name: &str) -> bool {
        match self.form.is_empty() {
            true => name != "remove_memberships",
            false => !field(&self.form, name).is_empty(),
        }
    }
}

#[derive(Template)]
#[template(path = "rules.html")]
struct RulesPage {
    flash: Flash,
    rules: Vec<Rule>,
}

/// A new onboarding rule, on a page of its own, with whatever was typed when it was refused.
#[derive(Template)]
#[template(path = "rule_new.html")]
struct NewRulePage {
    flash: Flash,
    groups: Vec<GroupRecord>,
    form: Form,
}

impl NewRulePage {
    fn value(&self, name: &str) -> String {
        match name {
            // A textarea keeps its lines.
            "permissions" => self
                .form
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
                .unwrap_or_default(),
            _ => field(&self.form, name),
        }
    }

    fn chosen(&self, group: &GroupRecord) -> bool {
        fields(&self.form, "group").contains(&format!("{}/{}", group.plugin, group.name))
    }

    fn enabled(&self) -> bool {
        self.form.is_empty() || !field(&self.form, "enabled").is_empty()
    }
}

async fn offboarding(store: &Store<'_>, flash: Flash) -> Result<String, Refusal> {
    let rules = store.offboarding_rules().await?;
    render(&OffboardingPage { flash, rules })
}

async fn new_offboarding(
    store: &Store<'_>,
    form: &Form,
    refused: Option<&Refusal>,
) -> Result<String, Refusal> {
    let providers = store.identity_providers().await?;
    let flash = refused.map(Flash::refused).unwrap_or_default();
    render(&NewOffboardingPage { flash, providers, form: form.clone() })
}

/// A rule as its form writes it. An unticked box sends nothing, so each one is read as a flag.
fn offboarding_from(form: &Form) -> Value {
    let ticked = |name: &str| !field(form, name).is_empty();
    json!({
        "name": field(form, "name"),
        "description": field(form, "description"),
        "provider": field(form, "provider"),
        "disable": ticked("disable"),
        "remove_identity": ticked("remove_identity"),
        "remove_provided_memberships": ticked("remove_provided_memberships"),
        "remove_memberships": ticked("remove_memberships"),
        "revoke_tokens": ticked("revoke_tokens"),
        "enabled": ticked("enabled"),
    })
}

/// Adds a rule and shows the list with it, or the form again with what was refused.
async fn add_offboarding(
    backend: &Backend,
    store: &Store<'_>,
    form: &Form,
) -> (Result<String, Refusal>, Option<String>) {
    let made = async {
        ops::require_admin(backend)?;
        let rule = offboarding_from(form);
        if rule["name"].as_str().unwrap_or_default().is_empty() {
            return Err(Refusal::bad("Give the rule a name."));
        }
        store.add_offboarding_rule(rule).await
    }
    .await;
    match made {
        Ok(made) => {
            let id = made.id.to_string();
            let _ = backend.audit("offboarding-rule.created", Some(&id), json!({})).await;
            let page = offboarding(store, Flash::done(format!("{} is added.", made.name))).await;
            (page, Some("/p/rbac/offboarding".into()))
        }
        Err(refusal) => (new_offboarding(store, form, Some(&refusal)).await, None),
    }
}

async fn change_offboarding(
    backend: &Backend,
    store: &Store<'_>,
    id: &str,
    action: &[&str],
    form: &Form,
) -> Result<String, Refusal> {
    ops::require_admin(backend)?;
    let id: uuid::Uuid = id.parse().map_err(|_| Refusal::bad("a rule is named by its ID"))?;
    match action {
        ["delete"] => {
            let gone = store.delete_offboarding_rule(id).await?;
            let _ =
                backend.audit("offboarding-rule.deleted", Some(&id.to_string()), json!({})).await;
            let flash = match gone {
                true => Flash::done("The rule is deleted."),
                false => Flash::refused(&Refusal::missing("That rule is already gone.")),
            };
            offboarding(store, flash).await
        }
        ["enabled"] => {
            let enabled = field(form, "enabled") == "true";
            let changed = store.set_offboarding_rule(id, json!({ "enabled": enabled })).await?;
            let _ = backend
                .audit(
                    "offboarding-rule.changed",
                    Some(&id.to_string()),
                    json!({ "enabled": enabled }),
                )
                .await;
            let flash = match changed {
                Some(_) if enabled => Flash::done("The rule is in use."),
                Some(_) => Flash::done("The rule is no longer in use."),
                None => Flash::refused(&Refusal::missing("That rule is gone.")),
            };
            offboarding(store, flash).await
        }
        _ => Err(Refusal::missing("no such route")),
    }
}

async fn rules(store: &Store<'_>, flash: Flash) -> Result<String, Refusal> {
    let rules = store.rules().await?;
    render(&RulesPage { flash, rules })
}

async fn new_rule(
    store: &Store<'_>,
    form: &Form,
    refused: Option<&Refusal>,
) -> Result<String, Refusal> {
    let groups = store
        .groups(None)
        .await?
        .into_iter()
        .filter(|group| group.kind == MemberKind::User)
        .collect();
    let flash = refused.map(Flash::refused).unwrap_or_default();
    render(&NewRulePage { flash, groups, form: form.clone() })
}

/// A rule as the form writes it: conditions, group memberships, extra permissions and attributes.
fn rule_from(form: &Form) -> Result<RuleChange, Refusal> {
    let optional = |name: &str| Some(field(form, name)).filter(|value| !value.is_empty());
    let groups = fields(form, "group")
        .into_iter()
        .filter_map(|group| {
            group
                .split_once('/')
                .map(|(plugin, name)| GroupRef { plugin: plugin.into(), name: name.into() })
        })
        .collect();
    let permissions = field(form, "permissions")
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    Ok(RuleChange {
        name: Some(field(form, "name")),
        description: Some(field(form, "description")),
        conditions: Some(Conditions {
            provider: optional("provider"),
            organisation: optional("organisation"),
            team: None,
            attributes: pairs(&field(form, "when_attributes"))?,
        }),
        grants: Some(RuleGrants {
            groups,
            permissions,
            attributes: pairs(&field(form, "set_attributes"))?,
        }),
        enabled: Some(field(form, "enabled") == "yes"),
    })
}

/// Creates a rule and shows it, or the form again with what was refused.
async fn create_rule(
    backend: &Backend,
    store: &Store<'_>,
    form: &Form,
) -> (Result<String, Refusal>, Option<String>) {
    let result = async { ops::create_rule(backend, store, rule_from(form)?).await }.await;
    match result {
        Ok(rule) => {
            let id = rule.id.to_string();
            let flash = Flash::done(format!("Created the rule {}.", rule.name));
            let page = self::rule(store, &id, None, flash).await;
            (page, Some(format!("/p/rbac/rules/{id}")))
        }
        Err(refusal) => (new_rule(store, form, Some(&refusal)).await, None),
    }
}

#[derive(Template)]
#[template(path = "rule.html")]
struct RulePage {
    flash: Flash,
    /// Whether the section open is trying it on someone, rather than the rule itself.
    trying: bool,
    rule: Rule,
    groups: Vec<GroupRecord>,
    when_attributes: String,
    set_attributes: String,
    permissions: String,
    /// A preview of the rule against one user, when one was asked for.
    test: Option<Value>,
}

impl RulePage {
    fn chosen(&self, group: &GroupRecord) -> bool {
        self.rule
            .grants
            .groups
            .iter()
            .any(|chosen| chosen.plugin == group.plugin && chosen.name == group.name)
    }

    fn enabled(&self) -> bool {
        self.rule.enabled
    }

    fn value(&self, field: &str) -> String {
        let conditions = &self.rule.conditions;
        match field {
            "name" => self.rule.name.clone(),
            "description" => self.rule.description.clone(),
            "provider" => conditions.provider.clone().unwrap_or_default(),
            "organisation" => conditions.organisation.clone().unwrap_or_default(),
            "when_attributes" => self.when_attributes.clone(),
            "set_attributes" => self.set_attributes.clone(),
            "permissions" => self.permissions.clone(),
            _ => String::new(),
        }
    }

    /// What a preview would add, as permissions or `key=value` attributes.
    fn list(&self, test: &Value, key: &str) -> Vec<String> {
        match &test["would_add"][key] {
            Value::Array(items) => {
                items.iter().filter_map(Value::as_str).map(str::to_string).collect()
            }
            Value::Object(pairs) => pairs
                .iter()
                .map(|(name, value)| format!("{name}={}", value.as_str().unwrap_or_default()))
                .collect(),
            _ => Vec::new(),
        }
    }
}

async fn rule(
    store: &Store<'_>,
    id: &str,
    test: Option<Value>,
    flash: Flash,
) -> Result<String, Refusal> {
    rule_section(store, id, false, test, flash).await
}

async fn rule_section(
    store: &Store<'_>,
    id: &str,
    trying: bool,
    test: Option<Value>,
    flash: Flash,
) -> Result<String, Refusal> {
    let rule = ops::rule(store, id).await?;
    let groups = store
        .groups(None)
        .await?
        .into_iter()
        .filter(|group| group.kind == MemberKind::User)
        .collect();
    let when_attributes = written(&rule.conditions.attributes);
    let set_attributes = written(&rule.grants.attributes);
    let permissions = rule.grants.permissions.join("\n");
    render(&RulePage {
        flash,
        trying,
        rule,
        groups,
        when_attributes,
        set_attributes,
        permissions,
        test,
    })
}

async fn rule_change(
    backend: &Backend,
    store: &Store<'_>,
    id: &str,
    action: &[&str],
    form: &Form,
) -> Result<String, Refusal> {
    match action {
        ["delete"] => match ops::delete_rule(backend, store, id).await {
            Ok(rule) => rules(store, Flash::done(format!("Deleted the rule {}.", rule.name))).await,
            Err(refusal) => rule(store, id, None, Flash::refused(&refusal)).await,
        },
        ["test"] => {
            let found = async {
                let Holder::User { id: user } = user_named(store, &field(form, "login")).await?
                else {
                    return Err(Refusal::bad("rules onboard users"));
                };
                ops::test_rule(store, &ops::rule(store, id).await?, user).await
            }
            .await;
            match found {
                Ok(test) => rule_section(store, id, true, Some(test), Flash::default()).await,
                Err(refusal) => rule_section(store, id, true, None, Flash::refused(&refusal)).await,
            }
        }
        [] => {
            let result =
                async { ops::update_rule(backend, store, id, rule_from(form)?).await }.await;
            let flash = match result {
                Ok(rule) => Flash::done(format!("Saved the rule {}.", rule.name)),
                Err(refusal) => Flash::refused(&refusal),
            };
            rule(store, id, None, flash).await
        }
        _ => Err(Refusal::missing("no such change")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(tab: Tab, plugin: &str) -> PrincipalPage {
        let id = uuid::Uuid::nil();
        let assignment = |permission: &str| Assignment {
            holder: Holder::User { id },
            label: Some("ada".into()),
            permission: permission.into(),
            source: "api".into(),
            granted_by: "root".into(),
            granted_at: chrono::Utc::now(),
        };
        PrincipalPage {
            flash: Flash::default(),
            tab,
            kind: "user".into(),
            id: id.to_string(),
            label: "ada".into(),
            memberships: vec![Membership { plugin: "kb".into(), name: "readers".into() }],
            ties: vec![Tie {
                id: format!("team/{id}"),
                label: "Platform".into(),
                how: "added by hand".into(),
            }],
            assignments: vec![
                assignment("plugin:kb:group:readers"),
                assignment("plugin:core:user:ro"),
            ],
            attributes: BTreeMap::new(),
            access: vec![Reach { plugin: "kb".into(), scope: "ro".into(), custom: String::new() }],
            plugins: vec!["core".into(), "kb".into()],
            plugin: plugin.into(),
            not_granted: vec![Choice {
                permission: "plugin:kb:user:rw".into(),
                plugin: "kb".into(),
            }],
        }
    }

    fn section<'a>(html: &'a str, from: &str, to: &str) -> &'a str {
        let start = html.find(from).unwrap_or_else(|| panic!("no {from} in {html}"));
        let rest = &html[start..];
        &rest[..rest.find(to).unwrap_or(rest.len())]
    }

    #[test]
    fn the_resource_chooser_links_each_resource_to_its_map() {
        let chosen = ResourceChoices {
            error: None,
            kinds: vec![crate::access_map::Kinded {
                plural: "Services".into(),
                picks: vec![crate::access_map::Pick {
                    value: "Service:card-gateway".into(),
                    name: "card-gateway".into(),
                    title: "Card gateway".into(),
                }],
                more: 3,
            }],
            typed: String::new(),
        };
        let html = chosen.render().expect("rendered");
        assert!(
            html.contains(
                r#"href="/p/rbac/access-map?resource=Service%3Acard-gateway">card-gateway</a>"#
            ),
            "{html}"
        );
        assert!(html.contains("And 3 more"));
        let failed = ResourceChoices::of(Err(Refusal::unavailable("down")));
        assert!(failed.render().unwrap().contains("You can still write one as kind:name"));
    }

    #[test]
    fn the_groups_section_is_the_default_and_the_map_has_its_own() {
        assert_eq!(Tab::named(None), Tab::Groups);
        assert_eq!(Tab::named(Some("nonsense")), Tab::Groups);
        let html = page(Tab::Groups, "").render().expect("rendered");
        assert!(html.contains(r#"href="/p/rbac/principals/user/00000000-0000-0000-0000-000000000000" aria-current="page">Groups"#));
        assert!(
            !html.contains("/p/rbac/access-map/principal/user/"),
            "the map waits for its own section"
        );
        let map = page(Tab::Map, "").render().expect("rendered");
        assert!(map.contains("/p/rbac/access-map/principal/user/"));
        assert!(
            html.contains(r#"href="/p/rbac/people" aria-current="page">People and teams"#),
            "the plugin's own tabs mark the section the page is in"
        );
        assert!(!html.contains(r#"href="/p/rbac/groups" aria-current"#));
        assert!(!html.contains("doc-button--small\" href=\"/p/rbac/"), "no buttons as tabs");
        assert!(html.contains(r#"href="/p/rbac/groups/kb/readers""#));
        assert!(!html.contains("Not granted"), "permissions wait for their own tab");
    }

    #[test]
    fn the_permissions_tab_separates_what_is_granted_from_what_is_not() {
        let html = page(Tab::Permissions, "").render().expect("rendered");
        assert!(html.contains(r#"?tab=permissions" aria-current="page">Permissions"#));
        assert!(!html.contains("access-map/principal"), "the map is in its own section");
        let granted = section(&html, "<h3>Granted</h3>", "<h3>Not granted</h3>");
        assert!(granted.contains("plugin:core:user:ro") && granted.contains("Revoke"));
        let not_granted = section(&html, "<h3>Not granted</h3>", "{% endblock");
        assert!(not_granted.contains("plugin:kb:user:rw") && not_granted.contains("Grant"));

        let html = page(Tab::Permissions, "kb").render().expect("rendered");
        let granted = section(&html, "<h3>Granted</h3>", "<h3>Not granted</h3>");
        assert!(!granted.contains("plugin:core:user:ro"), "filtered to kb");
        assert!(granted.contains("plugin:kb:group:readers"));
    }
}
