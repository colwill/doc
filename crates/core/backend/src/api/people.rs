//! Adding a person by email address (FEAT-PEOPLE): an identity manager anywhere, a team's lead into
//! the teams they arrange, and only by an address in a domain the organisation approved. The person
//! is given a DOC password login by the `local` plugin, so they can sign in before their
//! organisation's SSO is set up.

use axum::Json;
use axum::extract::State;
use axum::response::IntoResponse;
use http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::AppState;
use super::iam::{announce, manages_identity};
use super::problem::Problem;
use crate::auth::Auth;
use crate::identity::{Account, AuditEntry, NewUser, Principal, User};
use crate::plugins::routes::ask_internal;
use crate::teams::{BY_HAND, Team};

/// The identity plugin that keeps DOC passwords.
pub const PASSWORDS: &str = "local";
const MAX_EMAIL: usize = 254;
const MAX_LOGIN: usize = 64;
const MAX_NAME: usize = 256;

#[derive(Debug, Deserialize)]
pub struct NewPerson {
    pub email: String,
    #[serde(default)]
    pub name: Option<String>,
    /// The team to put them in, which a lead must arrange.
    #[serde(default)]
    pub team: Option<Uuid>,
    /// Their organisation, when there is no team; else the one that approved the domain.
    #[serde(default)]
    pub organisation: Option<Uuid>,
    /// The name they go by, when not the part of the address before the `@`.
    #[serde(default)]
    pub login: Option<String>,
}

/// An email address, lower case, and its domain.
fn address(text: &str) -> Result<(String, String), Problem> {
    let email = text.trim().to_lowercase();
    let fine = email.chars().count() <= MAX_EMAIL
        && !email.chars().any(|c| c.is_whitespace() || c.is_control())
        && email
            .split_once('@')
            .is_some_and(|(user, domain)| !user.is_empty() && domain.contains('.'));
    match (fine, email.rsplit_once('@')) {
        (true, Some((_, domain))) => {
            let domain = domain.to_string();
            Ok((email, domain))
        }
        _ => Err(Problem::bad_request("an email address is written like ada@acme.com")),
    }
}

/// An email domain as an organisation approves it: lower case, with a dot, and no `@`.
pub fn domain(text: &str) -> Result<String, Problem> {
    let domain = text.trim().trim_start_matches('@').to_lowercase();
    let fine = (3..=253).contains(&domain.len())
        && domain.contains('.')
        && !domain.starts_with(['.', '-'])
        && !domain.ends_with(['.', '-'])
        && !domain.contains("..")
        && domain.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    match fine {
        true => Ok(domain),
        false => Err(Problem::bad_request(format!(
            "`{}` is not an email domain, written like acme.com",
            text.trim()
        ))),
    }
}

/// Whether an address in `domain` falls under the approved `approved`: it or anything inside it.
pub fn covers(approved: &str, domain: &str) -> bool {
    domain == approved || domain.ends_with(&format!(".{approved}"))
}

/// A login from what was asked, or from the address: letters, digits, `.`, `_` and `-`, made
/// unique among everyone's with `-2`, `-3` and so on.
fn login_for(asked: Option<&str>, email: &str, taken: &[String]) -> Result<String, Problem> {
    let base = match asked.map(str::trim).filter(|asked| !asked.is_empty()) {
        Some(asked) => {
            let fine = asked.chars().count() <= MAX_LOGIN
                && asked.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
                && asked.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
            if !fine {
                return Err(Problem::bad_request(
                    "a login is letters, digits, `.`, `_` and `-`, starting with a letter or digit",
                ));
            }
            return match taken.iter().any(|login| login.eq_ignore_ascii_case(asked)) {
                true => Err(Problem::conflict(format!("somebody goes by {asked} already"))),
                false => Ok(asked.to_string()),
            };
        }
        None => {
            let before = email.split('@').next().unwrap_or_default();
            let kept: String = before
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '-' })
                .skip_while(|c| !c.is_ascii_alphanumeric())
                .take(MAX_LOGIN - 4)
                .collect();
            if kept.is_empty() { "person".to_string() } else { kept }
        }
    };
    let free = |login: &str| !taken.iter().any(|held| held.eq_ignore_ascii_case(login));
    if free(&base) {
        return Ok(base);
    }
    (2..1000)
        .map(|n| format!("{base}-{n}"))
        .find(|login| free(login))
        .ok_or_else(|| Problem::conflict(format!("every login like {base} is taken")))
}

/// Asks `local` for a DOC password login, and links it to the person. What it answers: whether a
/// sign-in link was emailed, the one-time password to hand over otherwise, or why there is none.
async fn password_login(state: &AppState, user: &User, email: &str) -> Value {
    let asked = json!({ "username": user.login, "email": email, "name": user.name });
    let answer = match ask_internal(state, PASSWORDS, "logins", &asked).await {
        Ok(answer) => answer,
        Err(reason) => {
            return json!({ "problem": format!("they have no DOC password yet: {reason}") });
        }
    };
    let account = Account {
        provider: PASSWORDS.into(),
        external_id: user.login.clone(),
        login: user.login.clone(),
    };
    if let Err(err) = state.repos.identity.attach_identity(user.id, &account, BY_HAND).await {
        tracing::warn!(%err, login = %user.login, "a DOC password was made but not linked");
    }
    json!({
        "emailed": answer["emailed"].as_bool().unwrap_or(false),
        "password": answer["password"],
        "why": answer["why"],
    })
}

async fn record(state: &AppState, by: &Principal, action: &str, subject: Uuid, detail: Value) {
    let entry = AuditEntry::new(action).by(by).subject(subject.to_string()).detail(detail);
    let _ = state.repos.identity.record_audit(entry).await;
}

/// Adds somebody by email address, into a team or an organisation.
pub async fn create(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<NewPerson>,
) -> Result<impl IntoResponse, Problem> {
    let (status, answer) = add(&state, &auth.0, body).await?;
    Ok((status, Json(answer)))
}

/// Adds somebody for `by`, whether they asked here or through a plugin acting for them.
pub async fn add(
    state: &AppState,
    by: &Principal,
    body: NewPerson,
) -> Result<(StatusCode, Value), Problem> {
    let (email, their_domain) = address(&body.email)?;
    let team: Option<Team> = match body.team {
        Some(id) => Some(super::teams::team(state, id).await?),
        None => None,
    };
    match &team {
        Some(team) if !super::positions::arranges(state, by, team).await => {
            return Err(Problem::forbidden(
                "people are added to a team by its lead, a lead above it, or an identity manager",
            ));
        }
        None if !manages_identity(state, by).await => {
            return Err(Problem::forbidden(
                "adding somebody to no team needs plugin:rbac:user:rw; a lead adds them to a team",
            ));
        }
        _ => {}
    }
    let domains = state.repos.teams.domains().await?;
    let organisation = match (&team, body.organisation) {
        (Some(team), _) => team.organisation_id,
        (None, Some(id)) => super::teams::organisation(state, id).await?.id,
        (None, None) => domains
            .iter()
            .find(|(approved, _)| covers(approved, &their_domain))
            .map(|(_, organisation)| *organisation)
            .ok_or_else(|| {
                Problem::bad_request(format!("no organisation approves {their_domain}"))
            })?,
    };
    let approved = domains
        .iter()
        .any(|(approved, owner)| *owner == organisation && covers(approved, &their_domain));
    if !approved {
        let title = super::teams::organisation(state, organisation).await?.title;
        return Err(Problem::bad_request(format!(
            "{title} has not approved {their_domain}, so nobody can be added by an address in it"
        )));
    }
    let everyone: Vec<User> =
        state.repos.identity.list_users().await?.into_iter().map(|listed| listed.user).collect();
    let existing = everyone
        .iter()
        .find(|user| user.email.as_deref().is_some_and(|held| held.eq_ignore_ascii_case(&email)));
    let (user, created) = match existing {
        Some(user) if user.organisation_id != organisation => {
            return Err(Problem::conflict(format!(
                "{} has that address, in another organisation",
                user.login
            )));
        }
        Some(user) => (user.clone(), false),
        None => {
            let taken: Vec<String> = everyone.iter().map(|user| user.login.clone()).collect();
            let name = body.name.as_deref().map(str::trim).filter(|name| !name.is_empty());
            if name.is_some_and(|name| name.chars().count() > MAX_NAME) {
                return Err(Problem::bad_request(format!(
                    "a name is at most {MAX_NAME} characters"
                )));
            }
            let new = NewUser {
                login: login_for(body.login.as_deref(), &email, &taken)?,
                organisation_id: organisation,
                name: name.map(str::to_string),
                email: Some(email.clone()),
            };
            let user = state.repos.identity.create_user(&new, None).await?;
            let detail = json!({ "login": user.login, "email": email, "team": body.team });
            record(state, by, "iam.user.created", user.id, detail).await;
            (user, true)
        }
    };
    let added = match &team {
        Some(team) => state.repos.teams.add_member(team.id, user.id, BY_HAND, None).await?,
        None => false,
    };
    if let (Some(team), true) = (&team, added) {
        let detail = json!({ "user": user.id, "login": user.login });
        record(state, by, "team.member.added", team.id, detail).await;
        crate::permissions::forget_all(state).await;
        let change = json!({ "id": team.id, "members": { "added": [user.id] } });
        announce(state, "team.changed", change).await;
    }
    let login = match created {
        true => password_login(state, &user, &email).await,
        false => Value::Null,
    };
    let status = if created { StatusCode::CREATED } else { StatusCode::OK };
    Ok((
        status,
        json!({ "user": user, "created": created, "added_to_team": added, "login": login }),
    ))
}

#[cfg(test)]
mod tests {
    use doc_plugin_protocol::{Capability, Manifest, PluginState, RegisterRequest};

    use super::*;
    use crate::config::Config;
    use crate::db::repositories::IdentityRepository;
    use crate::identity::TokenOwner;
    use crate::plugins::client::Answer;
    use crate::secrets::TokenKind;
    use crate::testing::{ADMIN, Host, plugin_host_with, post_json, put_json};

    const ADA: &str = "doc_ses_ada";
    const BOB: &str = "doc_ses_bob";

    /// `tester` administers everything; `ada` leads Platform in Acme, which approves acme.com, and
    /// `bob` is anyone else. `local` keeps DOC passwords and hands out `one-time`.
    async fn acme() -> (Host, String, String) {
        let mut config = Config::default();
        config.bootstrap.admins = vec!["tester".into()];
        config.plugins.ids = vec!["local".into(), "agent".into()];
        config.plugins.capabilities.insert("local".into(), vec![Capability::IdentityProvider]);
        config.plugins.capabilities.insert("agent".into(), vec![Capability::TeamWriter]);
        let host = plugin_host_with(config);
        for (login, token) in [("ada", ADA), ("bob", BOB)] {
            let user = host.identity.add_user(login);
            host.identity.give(token, TokenKind::Session, TokenOwner::User(user.id), None, false);
        }
        let made = |path: &'static str, body: Value| {
            let app = host.app.clone();
            async move {
                let (status, made, _) = post_json(&app, path, Some(ADMIN), body).await;
                assert_eq!(status, StatusCode::CREATED, "{path}: {made}");
                made["id"].as_str().expect("an id").to_string()
            }
        };
        let acme = made("/api/v1/organisations", json!({ "name": "acme", "title": "Acme" })).await;
        let platform = made(
            "/api/v1/teams",
            json!({ "organisation": acme, "name": "platform", "title": "Platform" }),
        )
        .await;
        let path = format!("/api/v1/organisations/{acme}/domains");
        let (status, body, _) =
            put_json(&host.app, &path, ADMIN, json!({ "domains": ["Acme.com"] })).await;
        assert_eq!((status, &body["domains"]), (StatusCode::OK, &json!(["acme.com"])));
        let ada = host.identity.list_users().await.unwrap();
        let ada = ada.iter().find(|listed| listed.user.login == "ada").unwrap().user.id;
        let path = format!("/api/v1/users/{ada}");
        let moved =
            crate::testing::patch_json(&host.app, &path, ADMIN, json!({ "organisation": acme }));
        assert_eq!(moved.await.0, StatusCode::OK);
        let path = format!("/api/v1/teams/{platform}/members");
        let (status, _, _) = post_json(&host.app, &path, Some(ADMIN), json!({ "user": ada })).await;
        assert_eq!(status, StatusCode::CREATED);
        let path = format!("/api/v1/teams/{platform}/lead");
        assert_eq!(
            put_json(&host.app, &path, ADMIN, json!({ "user": ada })).await.0,
            StatusCode::OK
        );
        let manifest = Manifest {
            id: "local".into(),
            version: "1.0.0".into(),
            capabilities: vec![Capability::IdentityProvider],
            ..Manifest::default()
        };
        let request = RegisterRequest {
            manifest,
            address: "plugin-local:4440".into(),
            binary_sha256: "a".repeat(64),
            started_at: None,
        };
        let principal = Principal::Plugin { id: "local".into() };
        crate::plugins::register(&host.state, &principal, request).await.expect("registered");
        assert_eq!(host.settle("local").await, Some(PluginState::Running));
        (host, acme, platform)
    }

    #[tokio::test]
    async fn a_lead_adds_somebody_to_their_team_by_an_approved_address_and_nobody_else_does() {
        let (host, _, platform) = acme().await;
        host.plugin.answer_with(Answer::json(StatusCode::OK, &json!({ "password": "one-time" })));
        let asked =
            json!({ "email": "Grace@eu.acme.com", "name": "Grace Hopper", "team": platform });

        let (status, _, _) = post_json(&host.app, "/api/v1/people", Some(BOB), asked.clone()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "bob leads nothing");

        let (status, body, _) =
            post_json(&host.app, "/api/v1/people", Some(ADA), asked.clone()).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["user"]["login"], "grace");
        assert_eq!(body["user"]["email"], "grace@eu.acme.com");
        assert_eq!(
            (body["added_to_team"].as_bool(), body["login"]["password"].as_str()),
            (Some(true), Some("one-time"))
        );
        let (asked_local, caller) = host.plugin.requests().pop().expect("local was asked");
        assert_eq!(
            (asked_local.path.as_str(), caller.kind.as_str()),
            ("internal/logins", "platform")
        );
        let grace: Uuid = serde_json::from_value(body["user"]["id"].clone()).unwrap();
        let held = host.state.repos.identity.identities(Some(grace)).await.unwrap();
        assert_eq!(
            held.iter().map(|identity| identity.provider.as_str()).collect::<Vec<_>>(),
            ["local"]
        );

        let (status, body, _) = post_json(&host.app, "/api/v1/people", Some(ADA), asked).await;
        assert_eq!((status, body["created"].as_bool()), (StatusCode::OK, Some(false)), "{body}");

        let outside = json!({ "email": "mallory@evil.io", "team": platform });
        let (status, body, _) = post_json(&host.app, "/api/v1/people", Some(ADA), outside).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body["detail"].as_str().unwrap().contains("has not approved evil.io"));

        let (status, _, _) =
            post_json(&host.app, "/api/v1/people", Some(ADA), json!({ "email": "x@acme.com" }))
                .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "a lead adds people to a team, not to nowhere");
    }

    /// A plugin that writes teams adds somebody only for whoever it is acting for, held to what
    /// they may do themselves, and never for itself (FEAT-AGENT).
    #[tokio::test]
    async fn a_plugin_adds_somebody_only_for_whoever_it_is_acting_for() {
        use doc_plugin_protocol::calls::PersonRequest;

        let (host, _, platform) = acme().await;
        let mut agent = crate::testing::manifest("agent", "1.0.0");
        agent.capabilities = vec![Capability::TeamWriter];
        host.register(agent).await;
        host.plugin.answer_with(Answer::json(StatusCode::OK, &json!({ "emailed": true })));
        let grace = || PersonRequest {
            email: "grace@acme.com".into(),
            team: platform.parse().ok(),
            ..PersonRequest::default()
        };
        let people = host.identity.list_users().await.unwrap();
        let as_who = |login: &str| {
            let user = people.iter().find(|listed| listed.user.login == login).unwrap();
            let principal = Principal::User(user.user.clone());
            let ttl = std::time::Duration::from_secs(30);
            host.state.plugins.contexts.issue("agent", principal, ttl).expect("a context")
        };
        let add = |plugin: &'static str, context: Option<String>| {
            let state = host.state.clone();
            async move {
                crate::plugins::api::add_person(&state, plugin, context.as_deref(), grace()).await
            }
        };

        let alone = add("agent", None).await;
        assert_eq!(alone.unwrap_err().status, 403, "never for the plugin itself");
        let bob = as_who("bob");
        let refused = add("agent", Some(bob.token().to_string())).await;
        assert_eq!(refused.unwrap_err().status, 403, "bob does not arrange Platform");
        let ada = as_who("ada");
        let added = add("agent", Some(ada.token().to_string())).await.expect("added for ada");
        assert_eq!(
            (added["created"].clone(), added["added_to_team"].clone()),
            (json!(true), json!(true))
        );
        let unable = add("local", Some(ada.token().to_string())).await;
        assert_eq!(unable.unwrap_err().status, 403, "local does not write teams");
    }

    #[tokio::test]
    async fn an_identity_manager_adds_somebody_to_the_organisation_that_approved_their_domain() {
        let (host, acme, _) = acme().await;
        host.plugin.answer_with(Answer::json(StatusCode::OK, &json!({ "emailed": true })));
        let asked = json!({ "email": "alan@acme.com" });
        let (status, body, _) = post_json(&host.app, "/api/v1/people", Some(ADMIN), asked).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["user"]["organisation_id"], json!(acme));
        assert_eq!(
            (body["login"]["emailed"].as_bool(), body["added_to_team"].as_bool()),
            (Some(true), Some(false))
        );
        let (status, _, _) =
            post_json(&host.app, "/api/v1/people", Some(ADMIN), json!({ "email": "a@nowhere.io" }))
                .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "no organisation approves it");
    }

    #[test]
    fn an_approved_domain_covers_the_domains_inside_it_and_nothing_else() {
        assert!(covers("acme.com", "acme.com"));
        assert!(covers("acme.com", "eu.acme.com"));
        assert!(!covers("acme.com", "notacme.com"));
        assert!(!covers("acme.com", "acme.com.evil.io"));
        assert_eq!(domain(" @Acme.COM ").unwrap(), "acme.com");
        assert!(
            domain("acme").is_err() && domain("a..b.com").is_err() && domain("ac me.com").is_err()
        );
    }

    #[test]
    fn a_login_comes_from_the_address_and_is_made_unique() {
        let taken = vec!["ada".to_string(), "ada-2".to_string()];
        assert_eq!(login_for(None, "ada@acme.com", &taken).unwrap(), "ada-3");
        assert_eq!(login_for(None, "grace.hopper+x@acme.com", &[]).unwrap(), "grace.hopper-x");
        assert_eq!(login_for(Some("alan"), "ada@acme.com", &taken).unwrap(), "alan");
        assert!(login_for(Some("Ada"), "x@acme.com", &taken).is_err(), "taken, whatever the case");
    }
}
