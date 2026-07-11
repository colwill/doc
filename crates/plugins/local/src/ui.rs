//! The pages at `/p/local/...`. Admins, who write to the plugin, list the accounts, add them and
//! hand out one-time passwords. Anyone with an account changes their own password.

use askama::Template;
use chrono::{DateTime, Duration, Utc};
use doc_plugin_sdk::protocol::calls::UserRequest;
use doc_plugin_sdk::{Backend, Request, Response};
use serde_json::json;

use crate::accounts::{self, Account, ONE_TIME_DAYS, Refusal, Store};

type Form = Vec<(String, String)>;

fn form(request: &Request) -> Form {
    url::form_urlencoded::parse(&request.body).into_owned().collect()
}

fn field(form: &Form, name: &str) -> String {
    form.iter().find(|(key, _)| key == name).map(|(_, value)| value.to_string()).unwrap_or_default()
}

fn when(at: &DateTime<Utc>) -> String {
    at.format("%-d %b %Y %H:%M UTC").to_string()
}

#[derive(Default)]
pub struct Flash {
    pub notice: Option<String>,
    pub error: Option<String>,
}

impl Flash {
    fn done(notice: impl Into<String>) -> Self {
        Self { notice: Some(notice.into()), error: None }
    }

    fn refused(error: impl Into<String>) -> Self {
        Self { notice: None, error: Some(error.into()) }
    }
}

/// An account as the list shows it.
pub struct Row {
    pub id: String,
    pub username: String,
    pub name: String,
    pub email: String,
    pub state: &'static str,
    pub signed_in: String,
}

#[derive(Template)]
#[template(path = "accounts.html")]
pub struct AccountsPage {
    pub writes: bool,
    pub flash: Flash,
    pub rows: Vec<Row>,
}

#[derive(Template)]
#[template(path = "account_form.html")]
pub struct NewAccountPage {
    pub writes: bool,
    pub flash: Flash,
    pub username: String,
    pub first_name: String,
    pub surname: String,
    pub email: String,
}

#[derive(Template)]
#[template(path = "account.html")]
pub struct AccountPage {
    pub writes: bool,
    pub flash: Flash,
    /// Whether this is the page its details are changed on, with what was typed if refused.
    pub editing: Option<Form>,
    pub account: Account,
    pub state: &'static str,
    pub signed_in: String,
    /// A one-time password just given, shown this once.
    pub one_time: Option<String>,
    pub expires: String,
}

impl AccountPage {
    /// A detail as the form shows it: what was typed, or what the account says.
    fn value(&self, name: &str) -> String {
        if let Some(typed) = &self.editing
            && typed.iter().any(|(key, _)| key == name)
        {
            return field(typed, name);
        }
        match name {
            "first_name" => self.account.first_name.clone(),
            "surname" => self.account.surname.clone(),
            _ => self.account.email.clone(),
        }
    }
}

#[derive(Template)]
#[template(path = "password.html")]
pub struct PasswordPage {
    pub writes: bool,
    pub flash: Flash,
    /// Their DOC account, if they have one.
    pub username: Option<String>,
}

fn html<T: Template>(page: &T) -> Response {
    match page.render() {
        Ok(html) => Response::html(html),
        Err(err) => {
            Response::problem(500, "internal", &format!("the page could not be drawn: {err}"))
        }
    }
}

fn problem(refusal: &Refusal) -> Response {
    let kind = match refusal.status {
        400 => "bad-request",
        403 => "forbidden",
        404 => "not-found",
        409 => "conflict",
        _ => "unavailable",
    };
    Response::problem(refusal.status, kind, &refusal.detail)
}

pub async fn handle(backend: &Backend, request: &Request, route: &[&str]) -> Response {
    let writes = backend.writes();
    let post = request.method == "POST";
    match (post, route) {
        (false, [] | [""]) if writes => list(backend, Flash::default()).await,
        (false, [] | [""] | ["password"]) => password(backend, Flash::default()),
        (true, ["password"]) => change_password(backend, request).await,
        (_, ["accounts", ..]) if !writes => {
            Response::problem(403, "forbidden", "needs plugin:local:user:rw to manage accounts")
        }
        (false, ["accounts", "new"]) => html(&NewAccountPage {
            writes,
            flash: Flash::default(),
            username: String::new(),
            first_name: String::new(),
            surname: String::new(),
            email: String::new(),
        }),
        (true, ["accounts"]) => create(backend, request).await,
        (false, ["accounts", id]) => shown(backend, id, Flash::default(), None).await,
        (false, ["accounts", id, "edit"]) => {
            showing(backend, id, Flash::default(), None, Some(Form::new())).await
        }
        (true, ["accounts", id]) => change(backend, request, id).await,
        (true, ["accounts", id, "one-time"]) => one_time(backend, id).await,
        (true, ["accounts", id, "disabled"]) => disable(backend, request, id).await,
        _ => Response::not_found(),
    }
}

async fn list(backend: &Backend, flash: Flash) -> Response {
    let accounts = match Store(backend).all().await {
        Ok(accounts) => accounts,
        Err(refusal) => return problem(&refusal),
    };
    let rows = accounts
        .into_iter()
        .map(|account| Row {
            state: account.state(),
            name: account.name().unwrap_or_default(),
            signed_in: account.signed_in_at.as_ref().map_or_else(|| "Never".into(), when),
            id: account.id,
            username: account.username,
            email: account.email,
        })
        .collect();
    html(&AccountsPage { writes: true, flash, rows })
}

/// The caller's own DOC account: the login of the `local` account linked to them.
fn own(backend: &Backend) -> Option<String> {
    backend.caller().and_then(|caller| caller.linked.get(backend.id()).cloned())
}

fn password(backend: &Backend, flash: Flash) -> Response {
    html(&PasswordPage { writes: backend.writes(), flash, username: own(backend) })
}

async fn change_password(backend: &Backend, request: &Request) -> Response {
    let Some(username) = own(backend) else {
        return password(backend, Flash::refused("You have no DOC account to change."));
    };
    let form = form(request);
    let (current, new) = (field(&form, "current"), field(&form, "password"));
    if new != field(&form, "again") {
        return password(backend, Flash::refused("The two new passwords are not the same."));
    }
    let store = Store(backend);
    let flash = match store.check(&username, &current).await {
        Ok(accounts::Checked::Password(_)) => match store.choose(&username, &new).await {
            Ok(Some(_)) => {
                let _ = backend.audit("password.changed", Some(&username), json!({})).await;
                Flash::done("Your password is changed.")
            }
            Ok(None) => Flash::refused("Your account is gone."),
            Err(refusal) => Flash::refused(refusal.detail),
        },
        Ok(accounts::Checked::Resting) => {
            Flash::refused("Too many wrong passwords: try again in 15 minutes.")
        }
        Ok(_) => Flash::refused("Your current password is not right."),
        Err(refusal) => Flash::refused(refusal.detail),
    };
    password(backend, flash)
}

/// Makes or updates the DOC user an account belongs to, so they can be given access before they
/// first sign in, and their name is right everywhere.
async fn describe(backend: &Backend, account: &Account) -> Option<String> {
    let text = |value: &str| (!value.is_empty()).then(|| value.to_string());
    let described = UserRequest {
        provider: backend.id().to_string(),
        external_id: account.username.clone(),
        login: account.username.clone(),
        name: account.name(),
        email: text(&account.email),
        first_name: text(&account.first_name),
        surname: text(&account.surname),
        organisation: None,
    };
    match backend.provide_user(described).await {
        Ok(_) => None,
        Err(err) => Some(err.problem().map_or_else(|| err.to_string(), |(_, detail)| detail)),
    }
}

async fn create(backend: &Backend, request: &Request) -> Response {
    let form = form(request);
    let entered = NewAccountPage {
        writes: true,
        flash: Flash::default(),
        username: field(&form, "username").trim().to_string(),
        first_name: field(&form, "first_name"),
        surname: field(&form, "surname"),
        email: field(&form, "email"),
    };
    let made = async {
        let username = accounts::username(&entered.username)?;
        let first_name = accounts::detail(&entered.first_name, "a first name", false)?;
        let surname = accounts::detail(&entered.surname, "a surname", false)?;
        let email = accounts::detail(&entered.email, "an email address", true)?;
        let password = accounts::one_time_password()?;
        let expires_at = Utc::now() + Duration::days(ONE_TIME_DAYS);
        let profile = [first_name.as_str(), surname.as_str(), email.as_str()];
        let account =
            Store(backend).create(&username, profile, Some((&password, Some(expires_at)))).await?;
        Ok::<_, Refusal>((account, password))
    };
    match made.await {
        Ok((account, password)) => {
            let _ = backend.audit("account.created", Some(&account.username), json!({})).await;
            let flash = match describe(backend, &account).await {
                None => Flash::done(format!(
                    "{} can be given access now, before they sign in.",
                    account.username
                )),
                Some(problem) => Flash::refused(format!(
                    "The account is made, but DOC has no user for it until they sign in: {problem}"
                )),
            };
            let id = account.id.clone();
            shown(backend, &id, flash, Some(password)).await
        }
        Err(refusal) => html(&NewAccountPage { flash: Flash::refused(refusal.detail), ..entered }),
    }
}

async fn shown(backend: &Backend, id: &str, flash: Flash, one_time: Option<String>) -> Response {
    showing(backend, id, flash, one_time, None).await
}

/// An account's page, or, with `editing`, the page its details are changed on.
async fn showing(
    backend: &Backend,
    id: &str,
    flash: Flash,
    one_time: Option<String>,
    editing: Option<Form>,
) -> Response {
    let account = match Store(backend).get(id).await {
        Ok(Some(account)) => account,
        Ok(None) => return Response::problem(404, "not-found", "there is no such account"),
        Err(refusal) => return problem(&refusal),
    };
    let expires = account
        .one_time_expires_at
        .as_ref()
        .map_or_else(|| "until it is used".into(), |at| format!("until {}", when(at)));
    html(&AccountPage {
        writes: true,
        flash,
        editing,
        state: account.state(),
        signed_in: account.signed_in_at.as_ref().map_or_else(|| "Never".into(), when),
        account,
        one_time,
        expires,
    })
}

async fn change(backend: &Backend, request: &Request, id: &str) -> Response {
    let form = form(request);
    let changed = async {
        let first_name = accounts::detail(&field(&form, "first_name"), "a first name", false)?;
        let surname = accounts::detail(&field(&form, "surname"), "a surname", false)?;
        let email = accounts::detail(&field(&form, "email"), "an email address", true)?;
        let set = json!({ "first_name": first_name, "surname": surname, "email": email });
        Store(backend).set(id, set).await?.ok_or_else(|| Refusal::new(404, "that account is gone"))
    };
    let flash = match changed.await {
        Ok(account) => {
            let _ = backend.audit("account.changed", Some(&account.username), json!({})).await;
            match describe(backend, &account).await {
                None => Flash::done("Saved, and DOC shows it."),
                Some(problem) => Flash::refused(format!("Saved, but DOC was not told: {problem}")),
            }
        }
        Err(refusal) => {
            let flash = Flash::refused(refusal.detail);
            return showing(backend, id, flash, None, Some(form)).await;
        }
    };
    shown(backend, id, flash, None)
        .await
        .with_header("hx-push-url", &format!("/p/local/accounts/{id}"))
}

async fn one_time(backend: &Backend, id: &str) -> Response {
    let given = async {
        let password = accounts::one_time_password()?;
        let expires_at = Utc::now() + Duration::days(ONE_TIME_DAYS);
        let account = Store(backend)
            .give_one_time(id, &password, Some(expires_at))
            .await?
            .ok_or_else(|| Refusal::new(404, "that account is gone"))?;
        Ok::<_, Refusal>((account, password))
    };
    match given.await {
        Ok((account, password)) => {
            let _ = backend.audit("account.one-time", Some(&account.username), json!({})).await;
            let flash = Flash::done("Their old password no longer works.");
            shown(backend, id, flash, Some(password)).await
        }
        Err(refusal) => shown(backend, id, Flash::refused(refusal.detail), None).await,
    }
}

async fn disable(backend: &Backend, request: &Request, id: &str) -> Response {
    let disabled = field(&form(request), "disabled") == "true";
    let flash = match Store(backend).set(id, json!({ "disabled": disabled })).await {
        Ok(Some(account)) => {
            let action = if disabled { "account.disabled" } else { "account.enabled" };
            let _ = backend.audit(action, Some(&account.username), json!({})).await;
            match disabled {
                true => Flash::done("They can no longer sign in with this account."),
                false => Flash::done("They can sign in with this account again."),
            }
        }
        Ok(None) => Flash::refused("That account is gone."),
        Err(refusal) => Flash::refused(refusal.detail),
    };
    shown(backend, id, flash, None).await
}
