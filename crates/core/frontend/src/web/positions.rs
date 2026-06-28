//! A team's lead and the positions its people hold (FEAT-TEAMS). Identity managers define an
//! organisation's positions on its page; a team's lead, a lead above it, or an identity manager
//! changes them for the team, adds its own, and says who leads it and who holds which.

use askama::Template;
use axum::extract::{Extension, Form, Path, State};
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::AppState;
use super::csrf::Csrf;
use super::error::WebError;
use super::navigation::{Crumb, crumbs};
use super::pages::Chrome;
use super::teams::{
    OrganisationSection, TeamSection, actionable, chrome, organisation_page, team_page,
};
use crate::backend::{BackendError, Position, PositionChange, TeamPosition};
use crate::session::Signed;

#[derive(Debug, Default, Deserialize)]
pub struct PositionValues {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    /// One per line.
    #[serde(default)]
    pub responsibilities: String,
}

#[derive(Template)]
#[template(path = "position_form.html")]
pub struct PositionForm {
    pub chrome: Chrome,
    pub organisation: String,
    /// The position being changed, or `None` for a new one.
    pub name: Option<String>,
    pub values: PositionValues,
    pub error: Option<String>,
}

/// What a team's form says about one position.
#[derive(Debug, Default)]
pub struct ChangeValues {
    pub name: String,
    pub title: String,
    pub description: String,
    /// One per line.
    pub added: String,
    pub removed: Vec<String>,
    /// Empty to keep what comes from above, `hide` or `show`.
    pub hidden: String,
}

impl ChangeValues {
    fn from_form(fields: Vec<(String, String)>) -> Self {
        let mut values = Self::default();
        for (field, value) in fields {
            match field.as_str() {
                "name" => values.name = value,
                "title" => values.title = value,
                "description" => values.description = value,
                "added" => values.added = value,
                "removed" => values.removed.push(value),
                "hidden" => values.hidden = value,
                _ => {}
            }
        }
        values
    }

    fn from_change(name: &str, change: Option<&PositionChange>) -> Self {
        let Some(change) = change else {
            return Self { name: name.to_string(), ..Self::default() };
        };
        Self {
            name: name.to_string(),
            title: change.title.clone().unwrap_or_default(),
            description: change.description.clone().unwrap_or_default(),
            added: change.added.join("\n"),
            removed: change.removed.clone(),
            hidden: match change.hidden {
                Some(true) => "hide".into(),
                Some(false) => "show".into(),
                None => String::new(),
            },
        }
    }

    fn body(&self) -> Value {
        let text = |value: &str| (!value.trim().is_empty()).then(|| value.trim().to_string());
        json!({
            "title": text(&self.title),
            "description": text(&self.description),
            "added": lines(&self.added),
            "removed": self.removed,
            "hidden": match self.hidden.as_str() {
                "hide" => Some(true),
                "show" => Some(false),
                _ => None,
            },
        })
    }
}

#[derive(Template)]
#[template(path = "team_position_form.html")]
pub struct TeamPositionForm {
    pub chrome: Chrome,
    pub team: String,
    pub team_title: String,
    /// What comes from above, for a position the team inherits; `None` for one of its own.
    pub inherited: Option<TeamPosition>,
    /// Whether it is being made rather than changed.
    pub new: bool,
    /// Whether the team has changed or made it already, and so can take that back.
    pub changed: bool,
    pub values: ChangeValues,
    pub error: Option<String>,
}

impl TeamPositionForm {
    fn dropped(&self, responsibility: &str) -> bool {
        self.values.removed.iter().any(|removed| removed == responsibility)
    }
}

#[derive(Debug, Deserialize)]
pub struct LeadValues {
    #[serde(default)]
    pub user: String,
}

#[derive(Debug, Deserialize)]
pub struct HeldValues {
    #[serde(default)]
    pub position: String,
}

fn lines(text: &str) -> Vec<String> {
    text.lines().map(str::trim).filter(|line| !line.is_empty()).map(str::to_string).collect()
}

/// Says how many people a change left holding no position, when it left any.
fn vacated(vacated: &[Value], done: &str) -> String {
    match vacated.len() {
        0 => done.to_string(),
        1 => format!("{done} One person held a position that is gone, and now holds none."),
        many => format!("{done} {many} people held a position that is gone, and now hold none."),
    }
}

async fn organisation_form(
    state: &AppState,
    signed: &Signed,
    csrf: &Csrf,
    organisation: Uuid,
    name: Option<String>,
    values: PositionValues,
    error: Option<String>,
) -> Result<Html<String>, WebError> {
    let (found, _, _) =
        state.backend.organisation(signed.token(), &organisation.to_string()).await?;
    let title = match &name {
        Some(_) => format!("Change {}", values.title),
        None => "Add a position".to_string(),
    };
    let mut chrome = chrome(state, signed, csrf, title.clone(), "/organisations").await;
    let between = vec![
        Crumb::linked("Organisations", "/organisations"),
        Crumb::linked(&found.title, &format!("/organisations/{}", found.id)),
    ];
    chrome.crumbs = crumbs(&chrome.nav, between, &title);
    let organisation = found.id;
    Ok(Html(PositionForm { chrome, organisation, name, values, error }.render()?))
}

pub async fn new_organisation_position(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<Uuid>,
) -> Result<Html<String>, WebError> {
    organisation_form(&state, &signed, &csrf, id, None, PositionValues::default(), None).await
}

pub async fn edit_organisation_position(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path((id, name)): Path<(Uuid, String)>,
) -> Result<Html<String>, WebError> {
    let positions = state.backend.organisation_positions(signed.token(), &id.to_string()).await?;
    let Some(position) = positions.into_iter().find(|position| position.name == name) else {
        return Err(WebError::NotFound);
    };
    let values = values_of(position);
    organisation_form(&state, &signed, &csrf, id, Some(name), values, None).await
}

fn values_of(position: Position) -> PositionValues {
    PositionValues {
        name: position.name,
        title: position.title,
        description: position.description,
        responsibilities: position.responsibilities.join("\n"),
    }
}

async fn put_organisation_position(
    state: &AppState,
    signed: &Signed,
    csrf: &Csrf,
    id: Uuid,
    name: Option<String>,
    values: PositionValues,
) -> Result<Response, WebError> {
    let key = name.clone().unwrap_or_else(|| values.name.trim().to_string());
    if name.is_none() {
        let held = state.backend.organisation_positions(signed.token(), &id.to_string()).await?;
        if held.iter().any(|position| position.name == key) {
            let error = Some(format!("There is a position called {key} already."));
            return Ok(organisation_form(state, signed, csrf, id, name, values, error)
                .await?
                .into_response());
        }
    }
    let body = json!({
        "title": values.title,
        "description": values.description,
        "responsibilities": lines(&values.responsibilities),
    });
    let saved =
        state.backend.put_organisation_position(signed.token(), &id.to_string(), &key, &body).await;
    match saved {
        Ok(()) => {
            let notice = Some(format!("{} is saved; every team has it now.", values.title.trim()));
            let section = OrganisationSection::Positions;
            Ok(organisation_page(state, signed, csrf, id, section, notice, None)
                .await?
                .into_response())
        }
        Err(err) if actionable(&err) => {
            let error = Some(err.detail());
            Ok(organisation_form(state, signed, csrf, id, name, values, error)
                .await?
                .into_response())
        }
        Err(err) => Err(err.into()),
    }
}

pub async fn create_organisation_position(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<Uuid>,
    Form(values): Form<PositionValues>,
) -> Result<Response, WebError> {
    put_organisation_position(&state, &signed, &csrf, id, None, values).await
}

pub async fn update_organisation_position(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path((id, name)): Path<(Uuid, String)>,
    Form(values): Form<PositionValues>,
) -> Result<Response, WebError> {
    put_organisation_position(&state, &signed, &csrf, id, Some(name), values).await
}

pub async fn delete_organisation_position(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path((id, name)): Path<(Uuid, String)>,
) -> Result<Html<String>, WebError> {
    let deleted =
        state.backend.delete_organisation_position(signed.token(), &id.to_string(), &name).await;
    let (notice, error) = match deleted {
        Ok(held) => (Some(vacated(&held, "The position is deleted.")), None),
        Err(err) if actionable(&err) => (None, Some(err.detail())),
        Err(err) => return Err(err.into()),
    };
    let section = OrganisationSection::Positions;
    organisation_page(&state, &signed, &csrf, id, section, notice, error).await
}

/// The positions a team inherits: its parent's, or its organisation's for a top-level team.
async fn from_above(
    state: &AppState,
    signed: &Signed,
    parent: Option<&str>,
    organisation: &str,
) -> Result<Vec<TeamPosition>, BackendError> {
    if let Some(parent) = parent {
        return Ok(state.backend.team(signed.token(), parent).await?.positions);
    }
    let positions = state.backend.organisation_positions(signed.token(), organisation).await?;
    Ok(positions
        .into_iter()
        .map(|position| TeamPosition {
            name: position.name,
            title: position.title,
            description: position.description,
            responsibilities: position.responsibilities,
            hidden: false,
            origin: "organisation".into(),
            changed_by: Vec::new(),
            changed_here: false,
            holders: Vec::new(),
            change: None,
        })
        .collect())
}

async fn team_form(
    state: &AppState,
    signed: &Signed,
    csrf: &Csrf,
    team: Uuid,
    name: Option<&str>,
    values: Option<ChangeValues>,
    error: Option<String>,
) -> Result<Html<String>, WebError> {
    let detail = state.backend.team(signed.token(), &team.to_string()).await?;
    let above =
        from_above(state, signed, detail.team.parent_id.as_deref(), &detail.team.organisation_id)
            .await?;
    let inherited = name.and_then(|name| above.into_iter().find(|position| position.name == name));
    let here = name.and_then(|name| detail.positions.iter().find(|position| position.name == name));
    if name.is_some() && inherited.is_none() && here.is_none() {
        return Err(WebError::NotFound);
    }
    let change = here.and_then(|position| position.change.as_ref());
    let values = values.unwrap_or_else(|| ChangeValues::from_change(name.unwrap_or(""), change));
    let title = match (name, &inherited, here) {
        (None, _, _) => "Add a position of its own".to_string(),
        (Some(_), Some(inherited), _) => format!("{} in {}", inherited.title, detail.team.title),
        (Some(_), None, Some(here)) => format!("{} in {}", here.title, detail.team.title),
        _ => "A position".to_string(),
    };
    let mut chrome = chrome(state, signed, csrf, title.clone(), "/teams").await;
    let mut between: Vec<Crumb> = detail
        .ancestors
        .iter()
        .map(|above| Crumb::linked(&above.title, &format!("/teams/{}", above.id)))
        .collect();
    between.push(Crumb::linked(&detail.team.title, &format!("/teams/{}", detail.team.id)));
    chrome.crumbs = crumbs(&chrome.nav, between, &title);
    let page = TeamPositionForm {
        chrome,
        team: detail.team.id.clone(),
        team_title: detail.team.title.clone(),
        inherited,
        new: name.is_none(),
        changed: change.is_some(),
        values,
        error,
    };
    Ok(Html(page.render()?))
}

pub async fn new_team_position(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<Uuid>,
) -> Result<Html<String>, WebError> {
    team_form(&state, &signed, &csrf, id, None, None, None).await
}

pub async fn edit_team_position(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path((id, name)): Path<(Uuid, String)>,
) -> Result<Html<String>, WebError> {
    team_form(&state, &signed, &csrf, id, Some(&name), None, None).await
}

async fn put_team_position(
    state: &AppState,
    signed: &Signed,
    csrf: &Csrf,
    id: Uuid,
    name: Option<String>,
    values: ChangeValues,
) -> Result<Response, WebError> {
    let key = name.clone().unwrap_or_else(|| values.name.trim().to_string());
    if name.is_none() {
        let detail = state.backend.team(signed.token(), &id.to_string()).await?;
        if detail.positions.iter().any(|position| position.name == key) {
            let error = Some(format!("This team has a position called {key} already."));
            return Ok(team_form(state, signed, csrf, id, None, Some(values), error)
                .await?
                .into_response());
        }
    }
    let saved = state
        .backend
        .put_team_position(signed.token(), &id.to_string(), &key, &values.body())
        .await;
    match saved {
        Ok(held) => {
            let notice = Some(vacated(&held, "Saved, for this team and the teams inside it."));
            let page = team_page(state, signed, csrf, id, TeamSection::Positions, notice, None);
            Ok(page.await?.into_response())
        }
        Err(err) if actionable(&err) => {
            let error = Some(err.detail());
            Ok(team_form(state, signed, csrf, id, name.as_deref(), Some(values), error)
                .await?
                .into_response())
        }
        Err(err) => Err(err.into()),
    }
}

pub async fn create_team_position(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<Uuid>,
    Form(fields): Form<Vec<(String, String)>>,
) -> Result<Response, WebError> {
    put_team_position(&state, &signed, &csrf, id, None, ChangeValues::from_form(fields)).await
}

pub async fn update_team_position(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path((id, name)): Path<(Uuid, String)>,
    Form(fields): Form<Vec<(String, String)>>,
) -> Result<Response, WebError> {
    let values = ChangeValues::from_form(fields);
    put_team_position(&state, &signed, &csrf, id, Some(name), values).await
}

pub async fn revert_team_position(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path((id, name)): Path<(Uuid, String)>,
) -> Result<Html<String>, WebError> {
    let reverted = state.backend.delete_team_position(signed.token(), &id.to_string(), &name).await;
    let (notice, error) = match reverted {
        Ok(held) => (Some(vacated(&held, "This team's changes to it are gone.")), None),
        Err(err) if actionable(&err) => (None, Some(err.detail())),
        Err(err) => return Err(err.into()),
    };
    team_page(&state, &signed, &csrf, id, TeamSection::Positions, notice, error).await
}

pub async fn set_lead(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<Uuid>,
    Form(values): Form<LeadValues>,
) -> Result<Html<String>, WebError> {
    let user = Some(values.user.trim()).filter(|user| !user.is_empty());
    let done = state.backend.set_team_lead(signed.token(), &id.to_string(), user).await;
    let (notice, error) = match done {
        Ok(()) if user.is_some() => (Some("They lead the team now.".to_string()), None),
        Ok(()) => (Some("Nobody leads the team now.".to_string()), None),
        Err(err) if actionable(&err) => (None, Some(err.detail())),
        Err(err) => return Err(err.into()),
    };
    team_page(&state, &signed, &csrf, id, TeamSection::People, notice, error).await
}

pub async fn set_member_position(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path((id, user)): Path<(Uuid, Uuid)>,
    Form(values): Form<HeldValues>,
) -> Result<Html<String>, WebError> {
    let position = Some(values.position.trim()).filter(|position| !position.is_empty());
    let done = state
        .backend
        .set_member_position(signed.token(), &id.to_string(), &user.to_string(), position)
        .await;
    let (notice, error) = match done {
        Ok(()) => (Some("Their position is saved.".to_string()), None),
        Err(err) if actionable(&err) => (None, Some(err.detail())),
        Err(err) => return Err(err.into()),
    };
    team_page(&state, &signed, &csrf, id, TeamSection::People, notice, error).await
}
