//! People, for identity managers (`plugin:rbac:user:rw`): everyone DOC knows, each one person with
//! every account they sign in with (FEAT-PEOPLE). Somebody is added by an email address in a domain
//! their organisation approved, and given a DOC password so they can sign in before its SSO is set
//! up; an account can be linked to them before they first sign in with it (ADR-0005).

use askama::Template;
use axum::extract::{Extension, Form, Path, Query, State};
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::AppState;
use super::accounts::when;
use super::auth::provider_name;
use super::csrf::Csrf;
use super::error::WebError;
use super::me::how;
use super::navigation::{Crumb, crumbs};
use super::pages::{Chrome, Section};
use crate::backend::{BackendError, Organisation, UserView};
use crate::session::{self, Signed};

/// One account of a user's, as their page lists it.
pub struct AccountRow {
    pub id: String,
    pub provider: String,
    pub login: String,
    pub external_id: String,
    pub how: &'static str,
    pub last_used: Option<String>,
    /// What its provider last reported of them: name and email address.
    pub reported: Option<String>,
    /// Where it is managed, for a DOC password: a new one-time password, or turning it off.
    pub manage: Option<String>,
}

/// A user as the list shows them.
pub struct Row {
    pub id: String,
    pub login: String,
    pub name: Option<String>,
    pub organisation: String,
    pub accounts: Vec<(String, String)>,
    pub signed_in: Option<String>,
    pub disabled: bool,
}

/// A provider to suggest for an account: each one an organisation signs in with.
pub struct Choice {
    pub id: String,
    pub name: String,
}

#[derive(Template)]
#[template(path = "users.html")]
pub struct UsersPage {
    pub chrome: Chrome,
    pub users: Vec<Row>,
}

#[derive(Debug, Default, Deserialize)]
pub struct NewUser {
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub organisation: String,
    #[serde(default)]
    pub login: String,
}

#[derive(Template)]
#[template(path = "user_new.html")]
pub struct NewUserPage {
    pub chrome: Chrome,
    pub organisations: Vec<Organisation>,
    pub values: NewUser,
    pub error: Option<String>,
}

/// Somebody just added by email address, and how they will first sign in: the one place their
/// one-time password is shown, when it was not emailed to them.
#[derive(Template)]
#[template(path = "person_added.html")]
pub struct PersonAdded {
    pub chrome: Chrome,
    pub id: String,
    pub login: String,
    pub email: String,
    /// Whether they were made now, rather than found.
    pub created: bool,
    /// The team they were put in, by its ID and title, if they were.
    pub team: Option<(String, String)>,
    pub emailed: bool,
    pub password: Option<String>,
    /// Why the link was not emailed, or why they have no DOC password.
    pub why: Option<String>,
}

/// The page that says what adding somebody did, from what the platform answered.
pub(super) async fn added_page(
    state: &AppState,
    signed: &Signed,
    csrf: &Csrf,
    answer: &Value,
    team: Option<(String, String)>,
) -> Result<Html<String>, WebError> {
    let text = |value: &Value| value.as_str().filter(|text| !text.is_empty()).map(str::to_string);
    let user = &answer["user"];
    let login = text(&user["login"]).unwrap_or_default();
    let heading = match answer["created"].as_bool().unwrap_or(false) {
        true => format!("{login} is added"),
        false => format!("{login} was here already"),
    };
    let mut chrome =
        Chrome::new(heading, "/users").signed(signed, csrf).with_plugins(state, signed).await;
    if let Some((id, title)) = &team {
        let above = vec![Crumb::linked(title, &format!("/teams/{id}/members"))];
        chrome.crumbs = crumbs(&chrome.nav, above, "Added");
    }
    let given = &answer["login"];
    let page = PersonAdded {
        chrome,
        id: text(&user["id"]).unwrap_or_default(),
        email: text(&user["email"]).unwrap_or_default(),
        login,
        created: answer["created"].as_bool().unwrap_or(false),
        team: team.filter(|_| answer["added_to_team"].as_bool().unwrap_or(false)),
        emailed: given["emailed"].as_bool().unwrap_or(false),
        password: text(&given["password"]),
        why: text(&given["problem"]).or_else(|| text(&given["why"])),
    };
    Ok(Html(page.render()?))
}

/// The sections of a user's page, each a page of its own with the contents beside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserSection {
    Overview,
    Teams,
    Accounts,
}

impl UserSection {
    const ALL: [Self; 3] = [Self::Overview, Self::Teams, Self::Accounts];

    fn title(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Teams => "Teams",
            Self::Accounts => "Accounts",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Overview => "overview",
            Self::Teams => "teams",
            Self::Accounts => "accounts",
        }
    }

    fn href(self, user: &str) -> String {
        match self {
            Self::Overview => format!("/users/{user}"),
            Self::Teams => format!("/users/{user}/teams"),
            Self::Accounts => format!("/users/{user}/identities"),
        }
    }
}

#[derive(Template)]
#[template(path = "user.html")]
pub struct UserPage {
    pub chrome: Chrome,
    pub user: UserView,
    pub section: UserSection,
    pub sections: Vec<Section>,
    pub first_signed_in: Option<String>,
    pub last_signed_in: Option<String>,
    pub accounts: Vec<AccountRow>,
    /// Whether anybody who has never signed in could be merged into them.
    pub merges: bool,
    /// Whether there is another organisation they could be moved to.
    pub moves: bool,
    pub notice: Option<String>,
    pub error: Option<String>,
}

impl UserPage {
    fn on(&self, key: &str) -> bool {
        self.section.key() == key
    }
}

/// One thing done to a user, on a page of its own: linking an account, moving them to another
/// organisation, or merging somebody into them.
#[derive(Template)]
#[template(path = "user_form.html")]
pub struct UserForm {
    pub chrome: Chrome,
    pub user: UserView,
    /// `link`, `move` or `merge`.
    pub form: &'static str,
    pub choices: Vec<Choice>,
    /// Users who have never signed in, who can be merged into this one.
    pub mergeable: Vec<(String, String)>,
    /// The other organisations they could be moved to.
    pub organisations: Vec<Organisation>,
    pub values: Vec<(String, String)>,
    pub error: Option<String>,
}

impl UserForm {
    fn value(&self, name: &str) -> &str {
        self.values.iter().find(|(key, _)| key == name).map_or("", |(_, value)| value.as_str())
    }
}

fn optional(text: &str) -> Value {
    match text.trim() {
        "" => Value::Null,
        text => json!(text),
    }
}

/// A refusal the person can act on, as opposed to one that makes the page an error.
fn actionable(err: &BackendError) -> bool {
    matches!(err.status(), Some(400 | 403 | 404 | 409))
}

/// Every identity provider some organisation signs in with, as suggestions for an account.
async fn choices(state: &AppState) -> Vec<Choice> {
    let offered = state.backend.providers().await.unwrap_or_default();
    let mut choices: Vec<Choice> = offered
        .organisations
        .into_iter()
        .flat_map(|organisation| organisation.providers)
        .map(|provider| Choice { name: provider.title, id: provider.id })
        .collect();
    choices.sort_by(|a, b| a.id.cmp(&b.id));
    choices.dedup_by(|a, b| a.id == b.id);
    choices
}

/// How many people the picker offers at once.
const SUGGESTED: usize = 20;

pub struct Person {
    /// What the field takes: the login, or the user's ID where one is asked for.
    pub value: String,
    pub login: String,
    pub name: String,
}

#[derive(Template)]
#[template(path = "people_options.html")]
pub struct PeopleOptions {
    pub text: String,
    pub people: Vec<Person>,
}

#[derive(Debug, Deserialize)]
pub struct Looking {
    #[serde(default)]
    pub q: Option<String>,
    /// The form field the picker is filling, whose value is the text to match.
    #[serde(default)]
    pub field: Option<String>,
    /// `id` for a field that takes the user's ID rather than their login.
    #[serde(default)]
    pub values: Option<String>,
    #[serde(flatten)]
    pub rest: std::collections::BTreeMap<String, String>,
}

impl Looking {
    /// What was typed: `q`, or the value of whichever field the picker named.
    fn text(&self) -> String {
        self.q
            .clone()
            .or_else(|| self.field.as_ref().and_then(|field| self.rest.get(field).cloned()))
            .unwrap_or_default()
            .trim()
            .to_string()
    }
}

/// Everyone whose login or name is like what is being typed, for any field that names a person.
/// It is what anyone signed in may already see on the Users page, and nothing more.
pub async fn options(
    State(state): State<AppState>,
    signed: Signed,
    Query(looking): Query<Looking>,
) -> Result<Html<String>, WebError> {
    let text = looking.text();
    let wanted = text.to_lowercase();
    let by_id = looking.values.as_deref() == Some("id");
    let people = match state.backend.users(signed.token()).await {
        Ok(users) => users
            .into_iter()
            .filter(|user| {
                wanted.is_empty()
                    || user.login.to_lowercase().contains(&wanted)
                    || user.name.as_deref().unwrap_or_default().to_lowercase().contains(&wanted)
            })
            .take(SUGGESTED)
            .map(|user| Person {
                // A form that names a person by ID, such as adding them to a team, asks for it.
                value: match by_id {
                    true => user.id.to_string(),
                    false => user.login.clone(),
                },
                login: user.login,
                name: user.name.unwrap_or_default(),
            })
            .collect(),
        Err(err) => {
            tracing::warn!(%err, "people could not be listed for the picker");
            Vec::new()
        }
    };
    Ok(Html(PeopleOptions { text, people }.render()?))
}

pub async fn list(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
) -> Result<Html<String>, WebError> {
    let users = state.backend.users(signed.token()).await?;
    let rows = users
        .into_iter()
        .map(|user| Row {
            accounts: user
                .identities
                .iter()
                .map(|identity| (provider_name(&identity.provider), identity.login.clone()))
                .collect(),
            signed_in: user.last_signed_in_at.as_ref().map(when),
            organisation: user
                .organisation
                .map(|organisation| organisation.title)
                .unwrap_or_default(),
            id: user.id,
            login: user.login,
            name: user.name,
            disabled: user.disabled,
        })
        .collect();
    let chrome =
        Chrome::new("People", "/users").signed(&signed, &csrf).with_plugins(&state, &signed).await;
    Ok(Html(UsersPage { chrome, users: rows }.render()?))
}

async fn new_page(
    state: &AppState,
    signed: &Signed,
    csrf: &Csrf,
    values: NewUser,
    error: Option<String>,
) -> Result<Html<String>, WebError> {
    let chrome = Chrome::new("Add a person", "/users")
        .signed(signed, csrf)
        .with_plugins(state, signed)
        .await;
    let organisations = state.backend.organisations(signed.token()).await?;
    Ok(Html(NewUserPage { chrome, organisations, values, error }.render()?))
}

pub async fn new(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
) -> Result<Html<String>, WebError> {
    new_page(&state, &signed, &csrf, NewUser::default(), None).await
}

pub async fn create(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Form(form): Form<NewUser>,
) -> Result<Response, WebError> {
    let person = json!({
        "email": form.email.trim(),
        "name": optional(&form.name),
        "login": optional(&form.login),
        "organisation": optional(&form.organisation),
    });
    match state.backend.add_person(signed.token(), &person).await {
        Ok(answer) => Ok(added_page(&state, &signed, &csrf, &answer, None).await?.into_response()),
        Err(err) if actionable(&err) => {
            Ok(new_page(&state, &signed, &csrf, form, Some(err.detail())).await?.into_response())
        }
        Err(err) => Err(err.into()),
    }
}

/// Everyone else who has never signed in, whose record can be brought into `user`'s.
async fn mergeable(
    state: &AppState,
    signed: &Signed,
    user: &UserView,
) -> Result<Vec<(String, String)>, WebError> {
    Ok(state
        .backend
        .users(signed.token())
        .await?
        .into_iter()
        .filter(|other| other.id != user.id && other.first_signed_in_at.is_none())
        .map(|other| {
            let label = match &other.name {
                Some(name) => format!("{} ({name})", other.login),
                None => other.login.clone(),
            };
            (other.id, label)
        })
        .collect())
}

/// The organisations `user` could be moved to: every one but their own.
async fn elsewhere(
    state: &AppState,
    signed: &Signed,
    user: &UserView,
) -> Result<Vec<Organisation>, WebError> {
    Ok(state
        .backend
        .organisations(signed.token())
        .await?
        .into_iter()
        .filter(|organisation| {
            user.organisation.as_ref().is_none_or(|theirs| theirs.id != organisation.id)
        })
        .collect())
}

/// A user's chrome: their login, and the section or form it is as the last step there.
async fn user_chrome(
    state: &AppState,
    signed: &Signed,
    csrf: &Csrf,
    user: &UserView,
    here: &str,
) -> Chrome {
    let mut chrome = Chrome::new(user.login.clone(), "/users")
        .signed(signed, csrf)
        .with_plugins(state, signed)
        .await;
    if here != "Overview" {
        let above = vec![Crumb::linked(&user.login, &format!("/users/{}", user.id))];
        chrome.crumbs = crumbs(&chrome.nav, above, here);
    }
    chrome
}

async fn user_page(
    state: &AppState,
    signed: &Signed,
    csrf: &Csrf,
    id: Uuid,
    section: UserSection,
    notice: Option<String>,
    error: Option<String>,
) -> Result<Html<String>, WebError> {
    let user = state.backend.user(signed.token(), &id.to_string()).await?;
    let (merges, moves) = match section {
        UserSection::Overview => (
            !mergeable(state, signed, &user).await?.is_empty(),
            !elsewhere(state, signed, &user).await?.is_empty(),
        ),
        _ => (false, false),
    };
    let accounts = user
        .identities
        .iter()
        .map(|identity| AccountRow {
            id: identity.id.clone(),
            manage: (identity.provider == "local")
                .then(|| format!("/p/local/accounts/{}", identity.external_id)),
            provider: provider_name(&identity.provider),
            login: identity.login.clone(),
            external_id: identity.external_id.clone(),
            how: how(&identity.source),
            last_used: identity.last_used_at.as_ref().map(when),
            reported: {
                let said: Vec<&str> = [&identity.name, &identity.email]
                    .into_iter()
                    .filter_map(|value| value.as_deref())
                    .collect();
                (!said.is_empty()).then(|| said.join(", "))
            },
        })
        .collect();
    let chrome = user_chrome(state, signed, csrf, &user, section.title()).await;
    let sections = UserSection::ALL
        .into_iter()
        .map(|shown| Section::new(shown.title(), shown.href(&user.id), shown == section))
        .collect();
    let page = UserPage {
        chrome,
        section,
        sections,
        first_signed_in: user.first_signed_in_at.as_ref().map(when),
        last_signed_in: user.last_signed_in_at.as_ref().map(when),
        accounts,
        merges,
        moves,
        user,
        notice,
        error,
    };
    Ok(Html(page.render()?))
}

async fn user_form(
    state: &AppState,
    signed: &Signed,
    csrf: &Csrf,
    id: Uuid,
    form: &'static str,
    values: Vec<(String, String)>,
    error: Option<String>,
) -> Result<Html<String>, WebError> {
    let user = state.backend.user(signed.token(), &id.to_string()).await?;
    let here = match form {
        "link" => "Link an account",
        "move" => "Move to another organisation",
        _ => "Merge in a user",
    };
    let chrome = user_chrome(state, signed, csrf, &user, here).await;
    let (choices, mergeable, organisations) = match form {
        "link" => (choices(state).await, Vec::new(), Vec::new()),
        "move" => (Vec::new(), Vec::new(), elsewhere(state, signed, &user).await?),
        _ => (Vec::new(), mergeable(state, signed, &user).await?, Vec::new()),
    };
    let page = UserForm { chrome, user, form, choices, mergeable, organisations, values, error };
    Ok(Html(page.render()?))
}

pub async fn show(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<Uuid>,
) -> Result<Html<String>, WebError> {
    user_page(&state, &signed, &csrf, id, UserSection::Overview, None, None).await
}

pub async fn teams(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<Uuid>,
) -> Result<Html<String>, WebError> {
    user_page(&state, &signed, &csrf, id, UserSection::Teams, None, None).await
}

pub async fn identities(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<Uuid>,
) -> Result<Html<String>, WebError> {
    user_page(&state, &signed, &csrf, id, UserSection::Accounts, None, None).await
}

pub async fn link_form(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<Uuid>,
) -> Result<Html<String>, WebError> {
    user_form(&state, &signed, &csrf, id, "link", Vec::new(), None).await
}

pub async fn move_form(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<Uuid>,
) -> Result<Html<String>, WebError> {
    user_form(&state, &signed, &csrf, id, "move", Vec::new(), None).await
}

pub async fn merge_form(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<Uuid>,
) -> Result<Html<String>, WebError> {
    user_form(&state, &signed, &csrf, id, "merge", Vec::new(), None).await
}

/// The section of the user's page something was done in, saying what happened; or, when it was
/// refused and came from a form, that form again with why.
async fn after(
    state: &AppState,
    signed: &Signed,
    csrf: &Csrf,
    id: Uuid,
    section: UserSection,
    form: Option<(&'static str, Vec<(String, String)>)>,
    done: Result<String, BackendError>,
) -> Result<Html<String>, WebError> {
    match done {
        Ok(notice) => {
            session::forget_access(state).await;
            user_page(state, signed, csrf, id, section, Some(notice), None).await
        }
        Err(err) if actionable(&err) => match form {
            Some((form, values)) => {
                user_form(state, signed, csrf, id, form, values, Some(err.detail())).await
            }
            None => user_page(state, signed, csrf, id, section, None, Some(err.detail())).await,
        },
        Err(err) => Err(err.into()),
    }
}

#[derive(Debug, Deserialize)]
pub struct AccountForm {
    pub provider: String,
    pub external_id: String,
    #[serde(default)]
    pub login: String,
}

pub async fn attach(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<Uuid>,
    Form(form): Form<AccountForm>,
) -> Result<Html<String>, WebError> {
    let account = json!({
        "provider": form.provider.trim(),
        "external_id": form.external_id.trim(),
        "login": optional(&form.login),
    });
    let name = provider_name(form.provider.trim());
    let done = state.backend.attach_identity(signed.token(), &id.to_string(), &account).await.map(
        |merged| match merged {
            Some(_) => format!(
                "The {name} account is linked, and the user who had it, who had never signed in, \
                 was merged into this one."
            ),
            None => format!("The {name} account is linked."),
        },
    );
    let values = vec![
        ("provider".to_string(), form.provider.clone()),
        ("external_id".to_string(), form.external_id.clone()),
        ("login".to_string(), form.login.clone()),
    ];
    after(&state, &signed, &csrf, id, UserSection::Accounts, Some(("link", values)), done).await
}

pub async fn detach(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path((id, identity)): Path<(Uuid, Uuid)>,
) -> Result<Html<String>, WebError> {
    let done = state
        .backend
        .detach_identity(signed.token(), &id.to_string(), &identity.to_string())
        .await
        .map(|()| "The account is unlinked.".to_string());
    after(&state, &signed, &csrf, id, UserSection::Accounts, None, done).await
}

#[derive(Debug, Deserialize)]
pub struct MergeForm {
    pub from: Uuid,
}

pub async fn merge(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<Uuid>,
    Form(form): Form<MergeForm>,
) -> Result<Html<String>, WebError> {
    let done = state
        .backend
        .merge_user(signed.token(), &id.to_string(), &form.from.to_string())
        .await
        .map(|()| "The other user was merged into this one.".to_string());
    after(&state, &signed, &csrf, id, UserSection::Overview, Some(("merge", Vec::new())), done)
        .await
}

#[derive(Debug, Deserialize)]
pub struct Disabled {
    pub disabled: bool,
}

pub async fn set_disabled(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<Uuid>,
    Form(form): Form<Disabled>,
) -> Result<Html<String>, WebError> {
    let done = state
        .backend
        .set_user_disabled(signed.token(), &id.to_string(), form.disabled)
        .await
        .map(|()| match form.disabled {
            true => "The user is disabled, and their sessions and tokens stop working.".to_string(),
            false => "The user is enabled.".to_string(),
        });
    after(&state, &signed, &csrf, id, UserSection::Overview, None, done).await
}

#[derive(Debug, Deserialize)]
pub struct Moving {
    pub organisation: Uuid,
}

/// Moves someone to another organisation: out of their old one's teams, into the new one's
/// default teams.
pub async fn move_to(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<Uuid>,
    Form(moving): Form<Moving>,
) -> Result<Html<String>, WebError> {
    let done = state
        .backend
        .move_user(signed.token(), &id.to_string(), &moving.organisation.to_string())
        .await
        .map(|()| {
            "They belong to that organisation now: out of their old one's teams, and in its \
             default teams."
                .to_string()
        });
    after(&state, &signed, &csrf, id, UserSection::Overview, Some(("move", Vec::new())), done).await
}
