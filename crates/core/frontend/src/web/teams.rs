//! Teams and organisations (ADR-0004): who works together. Everyone signed in sees them. Identity
//! managers change them, apart from what a plugin provides, which changes there.

use askama::Template;
use axum::extract::{Extension, Form, Path, Query, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::AppState;
use super::auth::provider_name;
use super::csrf::Csrf;
use super::error::WebError;
use super::navigation::{Crumb, crumbs};
use super::pages::{Chrome, Section};
use crate::backend::{
    Access, BackendError, Member, Organisation, Position, ProviderChoice, Team, TeamDetail,
};
use crate::session::{self, Signed};

/// A team in a list, indented below its parent.
pub struct Row {
    pub id: String,
    pub title: String,
    pub name: String,
    pub depth: usize,
    pub members: usize,
    pub default: bool,
    pub provider: Option<String>,
}

impl Row {
    /// How far in to draw it, as a class suffix: nesting stops showing past four levels.
    pub fn indent(&self) -> usize {
        self.depth.min(4)
    }
}

/// What the org chart is of: `acme` for an organisation, `acme/platform` for a team in it.
#[derive(Debug, Default, Deserialize)]
pub struct Charted {
    #[serde(default)]
    pub of: String,
}

/// A team in the org chart. The chart is drawn from these in order as nested lists: each opens a
/// list for the teams below it when it has any, and the last team in a list closes it.
pub struct Branch {
    pub title: String,
    /// Its own chart.
    pub chart: String,
    pub lead: Option<String>,
    pub members: usize,
    pub default: bool,
    pub provider: Option<String>,
    pub opens: bool,
    /// How many lists end after it.
    pub closes: usize,
}

/// Someone in the team a chart is of.
pub struct Person {
    pub name: String,
    pub login: String,
    pub position: Option<String>,
    pub lead: bool,
}

/// What sits at the top of the chart.
pub enum Top {
    Organisation { title: String, teams: usize },
    Team { id: String, title: String, lead: Option<String>, people: Vec<Person> },
}

/// An organisation or a team the chart can be of.
pub struct Choice {
    pub value: String,
    pub label: String,
    pub chosen: bool,
}

pub struct ChoiceGroup {
    pub title: String,
    pub choices: Vec<Choice>,
}

#[derive(Template)]
#[template(path = "teams.html")]
pub struct TeamsPage {
    pub chrome: Chrome,
    pub manages: bool,
    pub choices: Vec<ChoiceGroup>,
    /// The organisation and the teams above the one charted, each linked to its own chart.
    pub above: Vec<Crumb>,
    /// `None` when there are no organisations.
    pub top: Option<Top>,
    pub branches: Vec<Branch>,
}

#[derive(Template)]
#[template(path = "organisations.html")]
pub struct OrganisationsPage {
    pub chrome: Chrome,
    pub organisations: Vec<Organisation>,
    pub manages: bool,
}

#[derive(Template)]
#[template(path = "organisation.html")]
pub struct OrganisationPage {
    pub chrome: Chrome,
    pub organisation: Organisation,
    pub section: OrganisationSection,
    pub sections: Vec<Section>,
    pub rows: Vec<Row>,
    pub manages: bool,
    /// Every identity provider there is to choose, and who chose each.
    pub providers: Vec<ProviderChoice>,
    /// The positions its teams inherit (FEAT-TEAMS).
    pub positions: Vec<Position>,
    pub notice: Option<String>,
    pub error: Option<String>,
}

/// The sections of an organisation's page, each a page of its own with the contents beside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrganisationSection {
    Overview,
    SigningIn,
    Teams,
    Positions,
}

impl OrganisationSection {
    const ALL: [Self; 4] = [Self::Overview, Self::SigningIn, Self::Teams, Self::Positions];

    fn title(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::SigningIn => "Signing in",
            Self::Teams => "Teams",
            Self::Positions => "Positions",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Overview => "overview",
            Self::SigningIn => "signing-in",
            Self::Teams => "teams",
            Self::Positions => "positions",
        }
    }

    fn href(self, organisation: &str) -> String {
        match self {
            Self::Overview => format!("/organisations/{organisation}"),
            Self::SigningIn => format!("/organisations/{organisation}/sign-in"),
            Self::Teams => format!("/organisations/{organisation}/teams"),
            Self::Positions => format!("/organisations/{organisation}/positions"),
        }
    }
}

/// Choosing how an organisation's people sign in, on a page of its own.
#[derive(Template)]
#[template(path = "organisation_providers.html")]
pub struct ProvidersPage {
    pub chrome: Chrome,
    pub organisation: Organisation,
    pub providers: Vec<ProviderChoice>,
    pub error: Option<String>,
}

impl ProvidersPage {
    fn chosen(&self, provider: &ProviderChoice) -> bool {
        provider.organisation.as_deref() == Some(self.organisation.id.as_str())
    }

    fn taken(&self, provider: &ProviderChoice) -> bool {
        provider.organisation.as_ref().is_some_and(|by| by != &self.organisation.id)
    }
}

impl OrganisationPage {
    fn on(&self, key: &str) -> bool {
        self.section.key() == key
    }

    /// Whether this organisation signs in with it.
    fn chosen(&self, provider: &ProviderChoice) -> bool {
        provider.organisation.as_deref() == Some(self.organisation.id.as_str())
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct OrganisationValues {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Template)]
#[template(path = "organisation_form.html")]
pub struct OrganisationForm {
    pub chrome: Chrome,
    /// The organisation being changed, or `None` for a new one.
    pub id: Option<String>,
    pub values: OrganisationValues,
    pub error: Option<String>,
}

/// Someone who could be added to a team.
pub struct Candidate {
    pub id: String,
    pub label: String,
}

/// The sections of a team's page, each a page of its own with the contents beside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeamSection {
    Overview,
    People,
    Positions,
    Teams,
    Accounts,
}

impl TeamSection {
    const ALL: [Self; 5] =
        [Self::Overview, Self::People, Self::Positions, Self::Teams, Self::Accounts];

    fn title(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::People => "People",
            Self::Positions => "Positions",
            Self::Teams => "Teams inside it",
            Self::Accounts => "Service accounts",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Overview => "overview",
            Self::People => "people",
            Self::Positions => "positions",
            Self::Teams => "teams",
            Self::Accounts => "accounts",
        }
    }

    pub fn href(self, team: &str) -> String {
        match self {
            Self::Overview => format!("/teams/{team}"),
            Self::People => format!("/teams/{team}/members"),
            Self::Positions => format!("/teams/{team}/positions"),
            Self::Teams => format!("/teams/{team}/teams"),
            Self::Accounts => format!("/teams/{team}/service-accounts"),
        }
    }
}

#[derive(Template)]
#[template(path = "team.html")]
pub struct TeamPage {
    pub chrome: Chrome,
    pub detail: TeamDetail,
    pub manages: bool,
    pub section: TeamSection,
    pub sections: Vec<Section>,
    pub notice: Option<String>,
    pub error: Option<String>,
    /// The team's page in the Catalogue, for a viewer who can read it: the same team, with what it
    /// owns, works on and is tagged with (T69).
    pub catalogue: Option<String>,
}

/// Adding somebody to a team, on a page of its own.
#[derive(Template)]
#[template(path = "team_member_new.html")]
pub struct NewMemberPage {
    pub chrome: Chrome,
    pub team: Team,
    /// Whether the viewer is an identity manager, who may also add somebody already in DOC.
    pub manages: bool,
    pub candidates: Vec<Candidate>,
    /// What was typed for somebody new: their email address and name.
    pub values: (String, String),
    pub error: Option<String>,
}

/// Choosing who leads a team, on a page of its own.
#[derive(Template)]
#[template(path = "team_lead.html")]
pub struct LeadPage {
    pub chrome: Chrome,
    pub detail: TeamDetail,
}

impl LeadPage {
    fn leads(&self, member: &Member) -> bool {
        self.detail.team.lead_id.as_deref() == Some(member.user_id.as_str())
    }
}

impl TeamPage {
    /// Whether `key` is the section open.
    fn on(&self, key: &str) -> bool {
        self.section.key() == key
    }

    fn how(source: &str, provider: Option<&str>) -> String {
        match (source, provider) {
            ("default", _) => "As everyone is, to a default team".into(),
            ("provider", Some(provider)) => format!("By {}", provider_name(provider)),
            _ => "By hand".into(),
        }
    }

    /// The plugin that provides the team, by its name.
    fn provided_by(&self) -> Option<String> {
        self.detail.team.provider.as_deref().map(provider_name)
    }

    /// The member who leads it.
    fn lead(&self) -> Option<&Member> {
        let lead = self.detail.team.lead_id.as_deref()?;
        self.detail.members.iter().find(|member| member.user_id == lead)
    }

    fn leads(&self, member: &Member) -> bool {
        self.detail.team.lead_id.as_deref() == Some(member.user_id.as_str())
    }

    /// The title of the position someone holds, by its name.
    fn titled(&self, name: &str) -> String {
        let found = self.detail.positions.iter().find(|position| position.name == name);
        found.map_or_else(|| name.to_string(), |position| position.title.clone())
    }

    /// Where a position comes from, in words.
    fn provenance(&self, position: &crate::backend::TeamPosition) -> String {
        let from = match position.origin.as_str() {
            "organisation" => "The organisation's".to_string(),
            team if team == self.detail.team.name => "This team's own".to_string(),
            team => format!("{team}'s own"),
        };
        let changed: Vec<&str> = position
            .changed_by
            .iter()
            .map(String::as_str)
            .filter(|team| *team != self.detail.team.name)
            .collect();
        let mut said = from;
        if !changed.is_empty() {
            said.push_str(&format!(", changed by {}", changed.join(" then ")));
        }
        if position.changed_here && position.origin != self.detail.team.name {
            said.push_str(", changed here");
        }
        said
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct TeamValues {
    #[serde(default)]
    pub organisation: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub parent: String,
    /// A checkbox: present when ticked.
    #[serde(default)]
    pub default: Option<String>,
}

/// A team a new or moved team could sit inside.
pub struct Parent {
    pub id: String,
    pub label: String,
}

#[derive(Template)]
#[template(path = "team_form.html")]
pub struct TeamForm {
    pub chrome: Chrome,
    /// The team being changed, or `None` for a new one.
    pub id: Option<String>,
    /// A provider's team: only its default mark changes here.
    pub provider: Option<String>,
    pub organisations: Vec<Organisation>,
    pub parents: Vec<Parent>,
    pub values: TeamValues,
    pub error: Option<String>,
}

/// Whether the viewer administers identity, and so changes teams.
pub(super) fn manages(access: &Access) -> bool {
    access.admin || access.plugins.get("rbac").is_some_and(|rbac| rbac.write)
}

/// A refusal the person can act on, as opposed to one that makes the page an error.
pub(super) fn actionable(err: &BackendError) -> bool {
    matches!(err.status(), Some(400 | 403 | 404 | 409))
}

/// The team a form puts another inside, or none; anything but a team's ID is a mistake to show.
fn parent_of(values: &TeamValues) -> Result<Value, String> {
    match values.parent.trim() {
        "" => Ok(Value::Null),
        text => text
            .parse::<Uuid>()
            .map(|id| json!(id))
            .map_err(|_| "Choose a team for it to sit inside, or none.".to_string()),
    }
}

/// An organisation's teams, each below its parent, in title order at every level.
pub fn tree(teams: &[Team], organisation: &str) -> Vec<Row> {
    let mine: Vec<&Team> =
        teams.iter().filter(|team| team.organisation_id == organisation).collect();
    let is_root = |team: &&Team| {
        team.parent_id.as_ref().is_none_or(|parent| !mine.iter().any(|other| &other.id == parent))
    };
    let mut roots: Vec<&Team> = mine.iter().copied().filter(is_root).collect();
    roots.sort_by_key(|team| team.title.to_lowercase());
    let mut rows = Vec::new();
    let mut stack: Vec<(&Team, usize)> = roots.into_iter().rev().map(|team| (team, 0)).collect();
    while let Some((team, depth)) = stack.pop() {
        if rows.iter().any(|row: &Row| row.id == team.id) {
            continue;
        }
        rows.push(Row {
            id: team.id.clone(),
            title: team.title.clone(),
            name: team.name.clone(),
            depth,
            members: team.members,
            default: team.default,
            provider: team.provider.as_deref().map(provider_name),
        });
        let mut children: Vec<&Team> = mine
            .iter()
            .copied()
            .filter(|child| child.parent_id.as_deref() == Some(team.id.as_str()))
            .collect();
        children.sort_by_key(|child| child.title.to_lowercase());
        stack.extend(children.into_iter().rev().map(|child| (child, depth + 1)));
    }
    rows
}

/// Every team below `team`, which it cannot be moved inside.
fn below(teams: &[Team], team: &str) -> Vec<String> {
    let mut found = vec![team.to_string()];
    let mut index = 0;
    while index < found.len() {
        let current = found[index].clone();
        for child in
            teams.iter().filter(|child| child.parent_id.as_deref() == Some(current.as_str()))
        {
            if !found.contains(&child.id) {
                found.push(child.id.clone());
            }
        }
        index += 1;
    }
    found
}

/// Each team, labelled with its organisation, that `team` (when moving one) could sit inside.
fn parents(organisations: &[Organisation], teams: &[Team], team: Option<&str>) -> Vec<Parent> {
    let excluded = team.map(|team| below(teams, team)).unwrap_or_default();
    let mut parents = Vec::new();
    for organisation in organisations {
        for row in tree(teams, &organisation.id) {
            if excluded.contains(&row.id) {
                continue;
            }
            let indent = "– ".repeat(row.depth);
            let label = format!("{} › {indent}{}", organisation.title, row.title);
            parents.push(Parent { id: row.id, label });
        }
    }
    parents
}

/// The teams the viewer is in and every team above them, whose service accounts they manage.
pub async fn reach(state: &AppState, signed: &Signed) -> Result<Vec<Team>, BackendError> {
    let Some(me) = signed.me.get("id").and_then(Value::as_str) else { return Ok(Vec::new()) };
    let mine = state.backend.user(signed.token(), me).await?.teams;
    if mine.is_empty() {
        return Ok(Vec::new());
    }
    let teams = state.backend.teams(signed.token()).await?;
    let mut reached: Vec<Team> = Vec::new();
    for held in mine {
        let mut next = Some(held.id);
        while let Some(id) = next {
            if reached.iter().any(|team| team.id == id) {
                break;
            }
            let Some(team) = teams.iter().find(|team| team.id == id) else { break };
            next = team.parent_id.clone();
            reached.push(team.clone());
        }
    }
    reached.sort_by_key(|team| team.title.to_lowercase());
    Ok(reached)
}

pub(super) async fn chrome(
    state: &AppState,
    signed: &Signed,
    csrf: &Csrf,
    title: String,
    path: &str,
) -> Chrome {
    Chrome::new(title, path).signed(signed, csrf).with_plugins(state, signed).await
}

/// A team's name in the chart: the name of whoever leads it, or their login.
fn lead_of(team: &Team) -> Option<String> {
    team.lead.as_ref().map(|lead| lead.name.clone().unwrap_or_else(|| lead.login.clone()))
}

fn chart_of(organisation: &str, team: Option<&str>) -> String {
    let of = match team {
        Some(team) => format!("{organisation}/{team}"),
        None => organisation.to_string(),
    };
    let query =
        url::form_urlencoded::Serializer::new(String::new()).append_pair("of", &of).finish();
    format!("/teams?{query}")
}

/// The teams below `parent` in an organisation, or its top-level teams for `None`, and every team
/// below them, in title order at every level.
pub fn branches(teams: &[Team], organisation: &Organisation, parent: Option<&str>) -> Vec<Branch> {
    let mine: Vec<&Team> =
        teams.iter().filter(|team| team.organisation_id == organisation.id).collect();
    let mut drawn = Vec::new();
    let mut seen: Vec<&str> = parent.into_iter().collect();
    grow(&mine, organisation, parent, &mut seen, &mut drawn);
    drawn
}

fn grow<'a>(
    teams: &[&'a Team],
    organisation: &Organisation,
    parent: Option<&str>,
    seen: &mut Vec<&'a str>,
    drawn: &mut Vec<Branch>,
) {
    let below_parent = |team: &&&Team| match parent {
        Some(parent) => team.parent_id.as_deref() == Some(parent),
        None => team
            .parent_id
            .as_ref()
            .is_none_or(|above| !teams.iter().any(|other| &other.id == above)),
    };
    let mut children: Vec<&Team> = teams.iter().filter(below_parent).copied().collect();
    children.retain(|team| !seen.contains(&team.id.as_str()));
    children.sort_by_key(|team| team.title.to_lowercase());
    for team in children {
        seen.push(&team.id);
        drawn.push(Branch {
            title: team.title.clone(),
            chart: chart_of(&organisation.name, Some(&team.name)),
            lead: lead_of(team),
            members: team.members,
            default: team.default,
            provider: team.provider.as_deref().map(provider_name),
            opens: false,
            closes: 0,
        });
        let at = drawn.len();
        grow(teams, organisation, Some(&team.id), seen, drawn);
        if drawn.len() > at {
            drawn[at - 1].opens = true;
            if let Some(last) = drawn.last_mut() {
                last.closes += 1;
            }
        }
    }
}

/// Every organisation, and every team in each, to chart.
fn choices(organisations: &[Organisation], teams: &[Team], of: &str) -> Vec<ChoiceGroup> {
    organisations
        .iter()
        .map(|organisation| {
            let mut choices = vec![Choice {
                label: format!("All of {}", organisation.title),
                chosen: of == organisation.name,
                value: organisation.name.clone(),
            }];
            for row in tree(teams, &organisation.id) {
                let value = format!("{}/{}", organisation.name, row.name);
                choices.push(Choice {
                    label: format!("{}{}", "– ".repeat(row.depth + 1), row.title),
                    chosen: of == value,
                    value,
                });
            }
            ChoiceGroup { title: organisation.title.clone(), choices }
        })
        .collect()
}

/// The org chart of an organisation or of a team, the viewer's own organisation until they choose.
pub async fn list(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Query(charted): Query<Charted>,
) -> Result<Html<String>, WebError> {
    let organisations = state.backend.organisations(signed.token()).await?;
    let teams = state.backend.teams(signed.token()).await?;
    let manages = manages(&session::access(&state, &signed).await);
    let chrome = chrome(&state, &signed, &csrf, "Teams".into(), "/teams").await;
    let mut of = charted.of.trim().to_string();
    if of.is_empty() {
        of = own_organisation(&state, &signed)
            .await
            .or_else(|| organisations.first().map(|organisation| organisation.name.clone()))
            .unwrap_or_default();
    }
    let mut page = TeamsPage {
        chrome,
        manages,
        choices: choices(&organisations, &teams, &of),
        above: Vec::new(),
        top: None,
        branches: Vec::new(),
    };
    if organisations.is_empty() {
        return Ok(Html(page.render()?));
    }
    let (organisation, team) = match of.split_once('/') {
        Some((organisation, team)) => (organisation, Some(team)),
        None => (of.as_str(), None),
    };
    let organisation =
        organisations.iter().find(|found| found.name == organisation).ok_or(WebError::NotFound)?;
    let Some(team) = team else {
        let count = teams.iter().filter(|team| team.organisation_id == organisation.id).count();
        page.top = Some(Top::Organisation { title: organisation.title.clone(), teams: count });
        page.branches = branches(&teams, organisation, None);
        return Ok(Html(page.render()?));
    };
    let team = teams
        .iter()
        .find(|found| found.organisation_id == organisation.id && found.name == team)
        .ok_or(WebError::NotFound)?;
    let detail = state.backend.team(signed.token(), &team.id).await?;
    page.above.push(Crumb::linked(&organisation.title, &chart_of(&organisation.name, None)));
    for above in &detail.ancestors {
        page.above
            .push(Crumb::linked(&above.title, &chart_of(&organisation.name, Some(&above.name))));
    }
    let titled = |name: &str| {
        let found = detail.positions.iter().find(|position| position.name == name);
        found.map_or_else(|| name.to_string(), |position| position.title.clone())
    };
    let lead = detail.team.lead_id.as_deref();
    let mut people: Vec<Person> = detail
        .members
        .iter()
        .map(|member| Person {
            name: member.name.clone().unwrap_or_else(|| member.login.clone()),
            login: member.login.clone(),
            position: member.position.as_deref().map(titled),
            lead: lead == Some(member.user_id.as_str()),
        })
        .collect();
    people.sort_by_key(|person| (!person.lead, person.name.to_lowercase()));
    page.top = Some(Top::Team {
        id: team.id.clone(),
        title: team.title.clone(),
        lead: people.iter().find(|person| person.lead).map(|person| person.name.clone()),
        people,
    });
    page.branches = branches(&teams, organisation, Some(&team.id));
    Ok(Html(page.render()?))
}

/// The name of the organisation the viewer belongs to.
async fn own_organisation(state: &AppState, signed: &Signed) -> Option<String> {
    let me = signed.me.get("id").and_then(Value::as_str)?;
    let user = state.backend.user(signed.token(), me).await.ok()?;
    user.organisation.map(|organisation| organisation.name)
}

pub async fn organisations(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
) -> Result<Html<String>, WebError> {
    let organisations = state.backend.organisations(signed.token()).await?;
    let manages = manages(&session::access(&state, &signed).await);
    let chrome = chrome(&state, &signed, &csrf, "Organisations".into(), "/organisations").await;
    Ok(Html(OrganisationsPage { chrome, organisations, manages }.render()?))
}

pub(super) async fn organisation_page(
    state: &AppState,
    signed: &Signed,
    csrf: &Csrf,
    id: Uuid,
    section: OrganisationSection,
    notice: Option<String>,
    error: Option<String>,
) -> Result<Html<String>, WebError> {
    let (organisation, teams, providers) =
        state.backend.organisation(signed.token(), &id.to_string()).await?;
    let rows = tree(&teams, &organisation.id);
    let positions = match section {
        OrganisationSection::Positions => {
            state.backend.organisation_positions(signed.token(), &organisation.id).await?
        }
        _ => Vec::new(),
    };
    let manages = manages(&session::access(state, signed).await);
    let chrome = organisation_chrome(state, signed, csrf, &organisation, section.title()).await;
    let sections = OrganisationSection::ALL
        .into_iter()
        .map(|shown| Section::new(shown.title(), shown.href(&organisation.id), shown == section))
        .collect();
    let page = OrganisationPage {
        chrome,
        organisation,
        section,
        sections,
        rows,
        manages,
        providers,
        positions,
        notice,
        error,
    };
    Ok(Html(page.render()?))
}

/// An organisation's chrome: under Organisations, with the section or form it is last.
async fn organisation_chrome(
    state: &AppState,
    signed: &Signed,
    csrf: &Csrf,
    organisation: &Organisation,
    here: &str,
) -> Chrome {
    let mut chrome =
        chrome(state, signed, csrf, organisation.title.clone(), "/organisations").await;
    let mut between = vec![Crumb::linked("Organisations", "/organisations")];
    let current = match here {
        "Overview" => organisation.title.clone(),
        _ => {
            let href = format!("/organisations/{}", organisation.id);
            between.push(Crumb::linked(&organisation.title, &href));
            here.to_string()
        }
    };
    chrome.crumbs = crumbs(&chrome.nav, between, &current);
    chrome
}

async fn providers_page(
    state: &AppState,
    signed: &Signed,
    csrf: &Csrf,
    id: Uuid,
    error: Option<String>,
) -> Result<Html<String>, WebError> {
    if !manages(&session::access(state, signed).await) {
        return Err(WebError::NotFound);
    }
    let (organisation, _, providers) =
        state.backend.organisation(signed.token(), &id.to_string()).await?;
    let chrome = organisation_chrome(state, signed, csrf, &organisation, "Signing in").await;
    Ok(Html(ProvidersPage { chrome, organisation, providers, error }.render()?))
}

pub async fn providers_form(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<Uuid>,
) -> Result<Html<String>, WebError> {
    providers_page(&state, &signed, &csrf, id, None).await
}

/// Chooses the identity providers the organisation's people sign in with, from the ticked boxes.
pub async fn set_providers(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<Uuid>,
    Form(ticked): Form<Vec<(String, String)>>,
) -> Result<Response, WebError> {
    let providers: Vec<String> = ticked
        .iter()
        .filter(|(name, _)| name == "provider")
        .map(|(_, provider)| provider.clone())
        .collect();
    // One a line, or separated by commas or spaces, as people paste them.
    let domains: Vec<String> = ticked
        .iter()
        .filter(|(name, _)| name == "domains")
        .flat_map(|(_, written)| written.split([',', ' ', '\n', '\r']))
        .map(str::trim)
        .filter(|domain| !domain.is_empty())
        .map(str::to_string)
        .collect();
    let id_text = id.to_string();
    let chosen = match state
        .backend
        .set_organisation_providers(signed.token(), &id_text, &providers)
        .await
    {
        Ok(()) => state.backend.set_organisation_domains(signed.token(), &id_text, &domains).await,
        Err(err) => Err(err),
    };
    match chosen {
        Ok(()) => {
            let notice = Some("Its people sign in with those now.".to_string());
            let section = OrganisationSection::SigningIn;
            Ok(organisation_page(&state, &signed, &csrf, id, section, notice, None)
                .await?
                .into_response())
        }
        Err(err) if actionable(&err) => {
            Ok(providers_page(&state, &signed, &csrf, id, Some(err.detail()))
                .await?
                .into_response())
        }
        Err(err) => Err(err.into()),
    }
}

pub async fn organisation(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<Uuid>,
) -> Result<Html<String>, WebError> {
    let section = OrganisationSection::Overview;
    organisation_page(&state, &signed, &csrf, id, section, None, None).await
}

pub async fn organisation_signing_in(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<Uuid>,
) -> Result<Html<String>, WebError> {
    let section = OrganisationSection::SigningIn;
    organisation_page(&state, &signed, &csrf, id, section, None, None).await
}

pub async fn organisation_teams(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<Uuid>,
) -> Result<Html<String>, WebError> {
    let section = OrganisationSection::Teams;
    organisation_page(&state, &signed, &csrf, id, section, None, None).await
}

pub async fn organisation_positions(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<Uuid>,
) -> Result<Html<String>, WebError> {
    let section = OrganisationSection::Positions;
    organisation_page(&state, &signed, &csrf, id, section, None, None).await
}

async fn organisation_form(
    state: &AppState,
    signed: &Signed,
    csrf: &Csrf,
    id: Option<String>,
    values: OrganisationValues,
    error: Option<String>,
) -> Result<Html<String>, WebError> {
    let title = match &id {
        Some(_) => format!("Change {}", values.title),
        None => "Add an organisation".to_string(),
    };
    let chrome = chrome(state, signed, csrf, title, "/organisations").await;
    Ok(Html(OrganisationForm { chrome, id, values, error }.render()?))
}

pub async fn new_organisation(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
) -> Result<Html<String>, WebError> {
    organisation_form(&state, &signed, &csrf, None, OrganisationValues::default(), None).await
}

pub async fn edit_organisation(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<Uuid>,
) -> Result<Html<String>, WebError> {
    let (organisation, _, _) = state.backend.organisation(signed.token(), &id.to_string()).await?;
    let values = OrganisationValues {
        name: organisation.name,
        title: organisation.title,
        description: organisation.description,
    };
    organisation_form(&state, &signed, &csrf, Some(id.to_string()), values, None).await
}

pub async fn create_organisation(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Form(values): Form<OrganisationValues>,
) -> Result<Response, WebError> {
    let body = json!({ "name": values.name.trim(), "title": values.title, "description": values.description });
    match state.backend.create_organisation(signed.token(), &body).await {
        Ok(made) => Ok(Redirect::to(&format!("/organisations/{}", made.id)).into_response()),
        Err(err) if actionable(&err) => {
            let error = Some(err.detail());
            Ok(organisation_form(&state, &signed, &csrf, None, values, error)
                .await?
                .into_response())
        }
        Err(err) => Err(err.into()),
    }
}

pub async fn update_organisation(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<Uuid>,
    Form(values): Form<OrganisationValues>,
) -> Result<Response, WebError> {
    let body = json!({ "name": values.name.trim(), "title": values.title, "description": values.description });
    match state.backend.update_organisation(signed.token(), &id.to_string(), &body).await {
        Ok(_) => Ok(Redirect::to(&format!("/organisations/{id}")).into_response()),
        Err(err) if actionable(&err) => {
            let error = Some(err.detail());
            let id = Some(id.to_string());
            Ok(organisation_form(&state, &signed, &csrf, id, values, error).await?.into_response())
        }
        Err(err) => Err(err.into()),
    }
}

pub async fn delete_organisation(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<Uuid>,
) -> Result<Response, WebError> {
    match state.backend.delete_organisation(signed.token(), &id.to_string()).await {
        Ok(()) => Ok(Redirect::to("/organisations").into_response()),
        Err(err) if actionable(&err) => {
            let error = Some(err.detail());
            let section = OrganisationSection::Overview;
            Ok(organisation_page(&state, &signed, &csrf, id, section, None, error)
                .await?
                .into_response())
        }
        Err(err) => Err(err.into()),
    }
}

pub(super) async fn team_page(
    state: &AppState,
    signed: &Signed,
    csrf: &Csrf,
    id: Uuid,
    section: TeamSection,
    notice: Option<String>,
    error: Option<String>,
) -> Result<Html<String>, WebError> {
    let detail = state.backend.team(signed.token(), &id.to_string()).await?;
    if section == TeamSection::Accounts && detail.service_accounts.is_none() {
        return Err(WebError::NotFound);
    }
    let access = session::access(state, signed).await;
    let catalogue = access
        .plugins
        .get("resources")
        .filter(|resources| resources.read)
        .map(|_| format!("/p/resources/r/team/{}", detail.team.name));
    let manages = manages(&access);
    let sections = TeamSection::ALL
        .into_iter()
        .filter(|shown| *shown != TeamSection::Accounts || detail.service_accounts.is_some())
        .map(|shown| Section::new(shown.title(), shown.href(&detail.team.id), shown == section))
        .collect();
    let chrome = team_chrome(state, signed, csrf, &detail, section.title()).await;
    let page = TeamPage { chrome, detail, manages, section, sections, notice, error, catalogue };
    Ok(Html(page.render()?))
}

/// A page of a team's: headed with the team, below the teams it is inside, and the section or
/// form it is as the last step of the way there.
async fn team_chrome(
    state: &AppState,
    signed: &Signed,
    csrf: &Csrf,
    detail: &TeamDetail,
    here: &str,
) -> Chrome {
    let mut chrome = chrome(state, signed, csrf, detail.team.title.clone(), "/teams").await;
    let mut above: Vec<Crumb> = detail
        .ancestors
        .iter()
        .map(|team| Crumb::linked(&team.title, &format!("/teams/{}", team.id)))
        .collect();
    let current = match here {
        "Overview" => detail.team.title.clone(),
        _ => {
            above.push(Crumb::linked(&detail.team.title, &format!("/teams/{}", detail.team.id)));
            here.to_string()
        }
    };
    chrome.crumbs = crumbs(&chrome.nav, above, &current);
    chrome
}

pub async fn team(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<Uuid>,
) -> Result<Html<String>, WebError> {
    team_page(&state, &signed, &csrf, id, TeamSection::Overview, None, None).await
}

pub async fn team_people(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<Uuid>,
) -> Result<Html<String>, WebError> {
    team_page(&state, &signed, &csrf, id, TeamSection::People, None, None).await
}

pub async fn team_positions(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<Uuid>,
) -> Result<Html<String>, WebError> {
    team_page(&state, &signed, &csrf, id, TeamSection::Positions, None, None).await
}

pub async fn team_teams(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<Uuid>,
) -> Result<Html<String>, WebError> {
    team_page(&state, &signed, &csrf, id, TeamSection::Teams, None, None).await
}

pub async fn team_accounts(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<Uuid>,
) -> Result<Html<String>, WebError> {
    team_page(&state, &signed, &csrf, id, TeamSection::Accounts, None, None).await
}

/// Adding somebody to a team: whoever arranges it adds somebody new by an approved email address
/// (FEAT-PEOPLE), and an identity manager may add somebody already in DOC too.
async fn member_form(
    state: &AppState,
    signed: &Signed,
    csrf: &Csrf,
    id: Uuid,
    values: (String, String),
    error: Option<String>,
) -> Result<Html<String>, WebError> {
    let manages = manages(&session::access(state, signed).await);
    let detail = state.backend.team(signed.token(), &id.to_string()).await?;
    if !manages && !detail.arranges {
        return Err(WebError::NotFound);
    }
    let candidates = match manages {
        true => state
            .backend
            .users(signed.token())
            .await?
            .into_iter()
            .filter(|user| !detail.members.iter().any(|member| member.user_id == user.id))
            .map(|user| Candidate {
                label: match &user.name {
                    Some(name) => format!("{} ({name})", user.login),
                    None => user.login.clone(),
                },
                id: user.id,
            })
            .collect(),
        false => Vec::new(),
    };
    let chrome = team_chrome(state, signed, csrf, &detail, "Add a person").await;
    let page = NewMemberPage { chrome, team: detail.team, manages, candidates, values, error };
    Ok(Html(page.render()?))
}

#[derive(Debug, Deserialize)]
pub struct Newcomer {
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub name: String,
}

/// Adds somebody new to a team by an approved email address, and shows how they will sign in.
pub async fn add_person(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<Uuid>,
    Form(newcomer): Form<Newcomer>,
) -> Result<Html<String>, WebError> {
    let name = Some(newcomer.name.trim()).filter(|name| !name.is_empty());
    let person = json!({ "email": newcomer.email.trim(), "name": name, "team": id });
    match state.backend.add_person(signed.token(), &person).await {
        Ok(answer) => {
            session::forget_access(&state).await;
            let team = state.backend.team(signed.token(), &id.to_string()).await?.team;
            let shown = Some((team.id, team.title));
            super::users::added_page(&state, &signed, &csrf, &answer, shown).await
        }
        Err(err) if actionable(&err) => {
            let values = (newcomer.email, newcomer.name);
            member_form(&state, &signed, &csrf, id, values, Some(err.detail())).await
        }
        Err(err) => Err(err.into()),
    }
}

pub async fn new_member(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<Uuid>,
) -> Result<Html<String>, WebError> {
    member_form(&state, &signed, &csrf, id, Default::default(), None).await
}

pub async fn lead_form(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<Uuid>,
) -> Result<Html<String>, WebError> {
    let detail = state.backend.team(signed.token(), &id.to_string()).await?;
    if !detail.arranges {
        return Err(WebError::NotFound);
    }
    let chrome = team_chrome(&state, &signed, &csrf, &detail, "Who leads it").await;
    Ok(Html(LeadPage { chrome, detail }.render()?))
}

async fn team_form(
    state: &AppState,
    signed: &Signed,
    csrf: &Csrf,
    team: Option<(String, Option<String>)>,
    values: TeamValues,
    error: Option<String>,
) -> Result<Html<String>, WebError> {
    let organisations = state.backend.organisations(signed.token()).await?;
    let teams = state.backend.teams(signed.token()).await?;
    let (id, provider) = team.unzip();
    let parents = parents(&organisations, &teams, id.as_deref());
    let title = match &id {
        Some(_) => format!("Change {}", values.title),
        None => "Add a team".to_string(),
    };
    let chrome = chrome(state, signed, csrf, title, "/teams").await;
    let provider = provider.flatten().as_deref().map(provider_name);
    Ok(Html(TeamForm { chrome, id, provider, organisations, parents, values, error }.render()?))
}

#[derive(Debug, Default, Deserialize)]
pub struct Placed {
    #[serde(default)]
    pub organisation: Option<String>,
    #[serde(default)]
    pub parent: Option<String>,
}

/// A new team, in the organisation or below the team the page came from.
pub async fn new_team(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Query(placed): Query<Placed>,
) -> Result<Html<String>, WebError> {
    let mut values = TeamValues {
        organisation: placed.organisation.unwrap_or_default(),
        parent: placed.parent.unwrap_or_default(),
        ..TeamValues::default()
    };
    if values.organisation.is_empty()
        && let Ok(parent) = values.parent.parse::<Uuid>()
    {
        let found = state.backend.team(signed.token(), &parent.to_string()).await?;
        values.organisation = found.team.organisation_id;
    }
    team_form(&state, &signed, &csrf, None, values, None).await
}

pub async fn edit_team(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<Uuid>,
) -> Result<Html<String>, WebError> {
    let found = state.backend.team(signed.token(), &id.to_string()).await?.team;
    let values = TeamValues {
        organisation: found.organisation_id,
        name: found.name,
        title: found.title,
        description: found.description,
        email: found.email,
        parent: found.parent_id.unwrap_or_default(),
        default: found.default.then(|| "on".to_string()),
    };
    team_form(&state, &signed, &csrf, Some((id.to_string(), found.provider)), values, None).await
}

pub async fn create_team(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Form(values): Form<TeamValues>,
) -> Result<Response, WebError> {
    let placed = values
        .organisation
        .trim()
        .parse::<Uuid>()
        .map_err(|_| "Choose the organisation it is in.".to_string())
        .and_then(|organisation| Ok((organisation, parent_of(&values)?)));
    let (organisation, parent) = match placed {
        Ok(placed) => placed,
        Err(error) => {
            let page = team_form(&state, &signed, &csrf, None, values, Some(error)).await?;
            return Ok(page.into_response());
        }
    };
    let body = json!({
        "organisation": organisation,
        "name": values.name.trim(),
        "title": values.title,
        "description": values.description,
        "email": values.email.trim(),
        "parent": parent,
        "default": values.default.is_some(),
    });
    match state.backend.create_team(signed.token(), &body).await {
        Ok(made) => Ok(Redirect::to(&format!("/teams/{}", made.id)).into_response()),
        Err(err) if actionable(&err) => {
            let error = Some(err.detail());
            Ok(team_form(&state, &signed, &csrf, None, values, error).await?.into_response())
        }
        Err(err) => Err(err.into()),
    }
}

pub async fn update_team(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<Uuid>,
    Form(values): Form<TeamValues>,
) -> Result<Response, WebError> {
    let found = state.backend.team(signed.token(), &id.to_string()).await?.team;
    let team = Some((id.to_string(), found.provider.clone()));
    // A provider's team keeps what the provider says; only its default mark is DOC's.
    let body = match &found.provider {
        Some(_) => json!({ "email": values.email.trim(), "default": values.default.is_some() }),
        None => match parent_of(&values) {
            Ok(parent) => json!({
                "name": values.name.trim(),
                "title": values.title,
                "description": values.description,
                "email": values.email.trim(),
                "parent": parent,
                "default": values.default.is_some(),
            }),
            Err(error) => {
                let page = team_form(&state, &signed, &csrf, team, values, Some(error)).await?;
                return Ok(page.into_response());
            }
        },
    };
    match state.backend.update_team(signed.token(), &id.to_string(), &body).await {
        Ok(_) => Ok(Redirect::to(&format!("/teams/{id}")).into_response()),
        Err(err) if actionable(&err) => {
            let error = Some(err.detail());
            Ok(team_form(&state, &signed, &csrf, team, values, error).await?.into_response())
        }
        Err(err) => Err(err.into()),
    }
}

pub async fn delete_team(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<Uuid>,
) -> Result<Response, WebError> {
    match state.backend.delete_team(signed.token(), &id.to_string()).await {
        Ok(()) => Ok(Redirect::to("/teams").into_response()),
        Err(err) if actionable(&err) => {
            let error = Some(err.detail());
            let page = team_page(&state, &signed, &csrf, id, TeamSection::Overview, None, error);
            Ok(page.await?.into_response())
        }
        Err(err) => Err(err.into()),
    }
}

#[derive(Debug, Deserialize)]
pub struct Adding {
    /// The user's ID, which the picker fills in when somebody is chosen from the list.
    #[serde(default)]
    pub user: String,
    /// What was typed into the picker, taken as a login when nobody was chosen from the list.
    #[serde(default)]
    pub q: String,
    /// Where to go afterwards, for a form on another page - a team's page in the Catalogue, say.
    #[serde(default)]
    pub return_to: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct Going {
    #[serde(default)]
    pub return_to: Option<String>,
}

/// Sends the browser back where the form was, rather than to the team's own page. HTMX follows
/// the header; a browser without it follows the refresh.
fn went_back(path: &str) -> Html<String> {
    let path = super::auth::local_path(Some(path));
    let escaped = path.replace('&', "&amp;").replace('"', "&quot;");
    Html(format!(
        "<meta http-equiv=\"refresh\" content=\"0; url={escaped}\" />\
         <p>Done. <a href=\"{escaped}\">Back to the page</a>.</p>"
    ))
}

/// Who the form names: the ID the picker fills in, or, when somebody typed a login and did not
/// choose anybody, whoever signs in as that. `None` when it names nobody there is.
async fn naming(state: &AppState, signed: &Signed, written: &str) -> Option<String> {
    let written = written.trim();
    if written.is_empty() {
        return None;
    }
    if written.parse::<Uuid>().is_ok() {
        return Some(written.to_string());
    }
    let users = state.backend.users(signed.token()).await.ok()?;
    users
        .into_iter()
        .find(|user| user.login.eq_ignore_ascii_case(written))
        .map(|user| user.id.to_string())
}

pub async fn add_member(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<Uuid>,
    Form(adding): Form<Adding>,
) -> Result<Html<String>, WebError> {
    let written = match adding.user.trim().is_empty() {
        true => adding.q.trim(),
        false => adding.user.trim(),
    };
    let Some(user) = naming(&state, &signed, written).await else {
        let refused = match written.is_empty() {
            true => "Start typing a name and choose somebody from the list.".to_string(),
            false => format!("Nobody here signs in as {written}."),
        };
        // A form somewhere else is sent back to, as it would be on success; otherwise the form
        // again, saying why nobody was added.
        if let Some(back) = adding.return_to.as_deref() {
            return Ok(went_back(back));
        }
        return member_form(&state, &signed, &csrf, id, Default::default(), Some(refused)).await;
    };
    let done = state.backend.add_team_member(signed.token(), &id.to_string(), &user).await;
    match done {
        Ok(()) => {
            session::forget_access(&state).await;
            // A form somewhere else says where it came from, and is sent back to it.
            if let Some(back) = adding.return_to.as_deref() {
                return Ok(went_back(back));
            }
            let notice = Some("They are in the team.".to_string());
            team_page(&state, &signed, &csrf, id, TeamSection::People, notice, None).await
        }
        Err(err) if actionable(&err) => {
            member_form(&state, &signed, &csrf, id, Default::default(), Some(err.detail())).await
        }
        Err(err) => Err(err.into()),
    }
}

pub async fn remove_member(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path((id, user)): Path<(Uuid, Uuid)>,
    Form(going): Form<Going>,
) -> Result<Html<String>, WebError> {
    let done =
        state.backend.remove_team_member(signed.token(), &id.to_string(), &user.to_string()).await;
    match done {
        Ok(()) => {
            session::forget_access(&state).await;
            if let Some(back) = going.return_to.as_deref() {
                return Ok(went_back(back));
            }
            let notice = Some("They are out of the team.".to_string());
            team_page(&state, &signed, &csrf, id, TeamSection::People, notice, None).await
        }
        Err(err) if actionable(&err) => {
            let error = Some(err.detail());
            team_page(&state, &signed, &csrf, id, TeamSection::People, None, error).await
        }
        Err(err) => Err(err.into()),
    }
}
