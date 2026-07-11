//! The GitHub calls the plugin makes: for sign-in, exchanging a code and reading the user, their
//! email addresses, organisations and teams; for the sync, an organisation's teams, members and
//! repositories; for delivery data, pages read conditionally so an unchanged one costs nothing;
//! archive links, which GitHub answers with a redirect to a short-lived URL; and pull requests.

use std::time::Duration;

use doc_plugin_sdk::protocol::Secret;
use doc_plugin_sdk::telemetry::sent;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::settings::{OAuth, Settings};

const TIMEOUT: Duration = Duration::from_secs(10);
const PER_PAGE: usize = 100;
const MAX_PAGES: usize = 100;
/// A code GitHub cannot know, so exchanging it tests only the client ID, secret and redirect URL.
const PROBE_CODE: &str = "doc-configuration-check";

#[derive(Debug, Deserialize)]
pub struct User {
    pub id: u64,
    pub login: String,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Email {
    email: String,
    #[serde(default)]
    primary: bool,
    #[serde(default)]
    verified: bool,
}

#[derive(Debug, Deserialize)]
struct Organisation {
    login: String,
}

#[derive(Debug, Deserialize)]
struct Team {
    slug: String,
    organization: Organisation,
}

/// The team a team sits inside, which GitHub gives with each team it lists.
#[derive(Debug, Deserialize)]
pub struct ParentTeam {
    pub slug: String,
}

/// An organisation's team, as the sync reads it.
#[derive(Debug, Deserialize)]
pub struct OrgTeam {
    pub slug: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// `None` for a team at the top of the organisation.
    #[serde(default)]
    pub parent: Option<ParentTeam>,
}

#[derive(Debug, Deserialize)]
pub struct Member {
    /// GitHub's own number for them, which never changes and is never reused, so it is the
    /// account's ID here. A login can be renamed and then taken by somebody else.
    pub id: u64,
    pub login: String,
}

#[derive(Debug, Deserialize)]
pub struct Repository {
    pub full_name: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub html_url: Option<String>,
    #[serde(default)]
    pub default_branch: Option<String>,
    #[serde(default)]
    pub visibility: Option<String>,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub language: Option<String>,
    /// When something was last pushed to it, which is how another plugin tells whether what it
    /// read from the repository is still what is there.
    #[serde(default)]
    pub pushed_at: Option<String>,
    /// Which is how delivery data can be asked for by topic rather than by name.
    #[serde(default)]
    pub topics: Vec<String>,
}

/// What a conditional read came back with: a page and the ETag to ask with next time, or word
/// that nothing changed, which GitHub does not count against the rate limit.
pub enum Read {
    Fresh(Value, Option<String>),
    Unchanged,
}

/// A read GitHub refused or could not answer, keeping the status so a rate limit can be told from
/// a repository that is not there.
#[derive(Debug)]
pub struct Refused {
    pub status: Option<u16>,
    pub detail: String,
}

impl Refused {
    pub fn limited(&self) -> bool {
        matches!(self.status, Some(403 | 429))
    }
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

#[derive(Debug, Deserialize)]
struct Installation {
    id: u64,
}

#[derive(Debug, Deserialize)]
pub struct InstallationToken {
    pub token: Secret<String>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

/// Everything sign-in learns about a person, read with their own access token.
#[derive(Debug)]
pub struct Profile {
    pub user: User,
    pub email: Option<String>,
    pub organisations: Vec<String>,
    pub teams: Vec<String>,
}

#[derive(Clone)]
pub struct GitHub {
    http: reqwest::Client,
    /// Stops at a redirect, since an archive link is where GitHub points rather than what it sends.
    pointing: reqwest::Client,
}

impl GitHub {
    pub fn new() -> Result<Self, String> {
        let built = |redirects: reqwest::redirect::Policy| {
            reqwest::Client::builder()
                .timeout(TIMEOUT)
                .user_agent(concat!("doc-github/", env!("CARGO_PKG_VERSION")))
                .redirect(redirects)
                .build()
                .map_err(|err| format!("the HTTP client could not be built: {err}"))
        };
        Ok(Self {
            http: built(reqwest::redirect::Policy::default())?,
            pointing: built(reqwest::redirect::Policy::none())?,
        })
    }

    async fn token_answer(
        &self,
        settings: &Settings,
        oauth: &OAuth,
        code: &str,
    ) -> Result<Value, String> {
        let endpoint = settings.web.join("login/oauth/access_token").map_err(|e| e.to_string())?;
        let form = [
            ("client_id", oauth.client_id.as_str()),
            ("client_secret", oauth.client_secret.expose().as_str()),
            ("code", code),
            ("redirect_uri", oauth.redirect.as_str()),
        ];
        let answer =
            self.http.post(endpoint).header("accept", "application/json").form(&form).send().await;
        sent("github", "oauth-token", &answer);
        let answer = answer.map_err(|err| format!("GitHub could not be reached: {err}"))?;
        answer.json().await.map_err(|err| format!("GitHub's answer could not be read: {err}"))
    }

    pub async fn exchange(
        &self,
        settings: &Settings,
        oauth: &OAuth,
        code: &str,
    ) -> Result<String, String> {
        let answer = self.token_answer(settings, oauth, code).await?;
        match answer.get("access_token").and_then(Value::as_str) {
            Some(token) => Ok(token.to_string()),
            None => Err(format!(
                "GitHub refused the sign-in: {}",
                answer.get("error_description").or(answer.get("error")).unwrap_or(&Value::Null)
            )),
        }
    }

    /// GitHub names what is wrong with an OAuth app when handed a code it has never issued.
    pub async fn check_app(&self, settings: &Settings, oauth: &OAuth) -> Result<(), String> {
        let answer = self.token_answer(settings, oauth, PROBE_CODE).await?;
        match answer.get("error").and_then(Value::as_str) {
            Some("bad_verification_code") => Ok(()),
            Some("incorrect_client_credentials") => {
                Err("GitHub does not accept the OAuth app's client ID and secret".into())
            }
            Some("redirect_uri_mismatch") => {
                Err(format!("the OAuth app does not allow {} as its callback URL", oauth.redirect))
            }
            _ => Err(format!("GitHub's answer to a configuration check was unexpected: {answer}")),
        }
    }

    pub async fn reachable(&self, settings: &Settings) -> Result<(), String> {
        let meta = settings.api.join("meta").map_err(|err| err.to_string())?;
        let answer = self.http.get(meta).send().await;
        sent("github", "meta", &answer);
        let answer = answer.map_err(|err| err.to_string())?;
        match answer.status().is_success() {
            true => Ok(()),
            false => Err(format!("{} answered {}", settings.api, answer.status())),
        }
    }

    async fn get<T: DeserializeOwned>(
        &self,
        settings: &Settings,
        token: &str,
        path: &str,
    ) -> Result<T, String> {
        let endpoint = settings.api.join(path).map_err(|err| err.to_string())?;
        let answer = self
            .http
            .get(endpoint)
            .bearer_auth(token)
            .header("accept", "application/vnd.github+json")
            .header("x-github-api-version", "2022-11-28")
            .send()
            .await;
        sent("github", "get", &answer);
        let answer = answer.map_err(|err| format!("GitHub could not be reached: {err}"))?;
        if !answer.status().is_success() {
            return Err(format!("GitHub answered {} to {path}", answer.status()));
        }
        answer.json().await.map_err(|err| format!("GitHub's {path} could not be read: {err}"))
    }

    async fn all<T: DeserializeOwned>(
        &self,
        settings: &Settings,
        token: &str,
        path: &str,
    ) -> Result<Vec<T>, String> {
        let mut items = Vec::new();
        let joiner = if path.contains('?') { '&' } else { '?' };
        for page in 1..=MAX_PAGES {
            let batch: Vec<T> = self
                .get(settings, token, &format!("{path}{joiner}per_page={PER_PAGE}&page={page}"))
                .await?;
            let last = batch.len() < PER_PAGE;
            items.extend(batch);
            if last {
                break;
            }
        }
        Ok(items)
    }

    /// One page of `path`, sent with `etag` when there is one; the ETag comes back to be kept.
    pub async fn read(
        &self,
        settings: &Settings,
        token: &str,
        path: &str,
        etag: Option<&str>,
    ) -> Result<Read, Refused> {
        let refused = |status: Option<u16>, detail: String| Refused { status, detail };
        let endpoint = settings.api.join(path).map_err(|err| refused(None, err.to_string()))?;
        let mut asking = self
            .http
            .get(endpoint)
            .bearer_auth(token)
            .header("accept", "application/vnd.github+json")
            .header("x-github-api-version", "2022-11-28");
        if let Some(etag) = etag {
            asking = asking.header("if-none-match", etag);
        }
        let answer = asking.send().await;
        sent("github", "delivery", &answer);
        let answer =
            answer.map_err(|err| refused(None, format!("GitHub could not be reached: {err}")))?;
        let status = answer.status().as_u16();
        if status == 304 {
            return Ok(Read::Unchanged);
        }
        if !answer.status().is_success() {
            return Err(refused(Some(status), format!("GitHub answered {status} to {path}")));
        }
        let etag =
            answer.headers().get("etag").and_then(|etag| etag.to_str().ok()).map(str::to_string);
        let page = answer.json().await.map_err(|err| {
            refused(Some(status), format!("GitHub's {path} could not be read: {err}"))
        })?;
        Ok(Read::Fresh(page, etag))
    }

    pub async fn profile(&self, settings: &Settings, token: &str) -> Result<Profile, String> {
        let user: User = self.get(settings, token, "user").await?;
        let emails: Vec<Email> = self.all(settings, token, "user/emails").await?;
        let organisations: Vec<Organisation> = self.all(settings, token, "user/orgs").await?;
        let teams: Vec<Team> = self.all(settings, token, "user/teams").await?;
        Ok(Profile {
            user,
            email: emails.into_iter().find(|e| e.primary && e.verified).map(|e| e.email),
            organisations: organisations.into_iter().map(|org| org.login).collect(),
            teams: teams
                .into_iter()
                .map(|team| format!("{}/{}", team.organization.login, team.slug))
                .collect(),
        })
    }

    pub async fn teams(
        &self,
        settings: &Settings,
        token: &str,
        org: &str,
    ) -> Result<Vec<OrgTeam>, String> {
        self.all(settings, token, &format!("orgs/{org}/teams")).await
    }

    pub async fn members(
        &self,
        settings: &Settings,
        token: &str,
        org: &str,
        team: &str,
    ) -> Result<Vec<Member>, String> {
        self.all(settings, token, &format!("orgs/{org}/teams/{team}/members")).await
    }

    /// Everybody in the organisation, which is who its teams can hold. Somebody who was here at
    /// the last sync and is not now has left it.
    pub async fn organisation_members(
        &self,
        settings: &Settings,
        token: &str,
        org: &str,
    ) -> Result<Vec<Member>, String> {
        self.all(settings, token, &format!("orgs/{org}/members")).await
    }

    pub async fn team_repositories(
        &self,
        settings: &Settings,
        token: &str,
        org: &str,
        team: &str,
    ) -> Result<Vec<Repository>, String> {
        self.all(settings, token, &format!("orgs/{org}/teams/{team}/repos")).await
    }

    pub async fn repositories(
        &self,
        settings: &Settings,
        token: &str,
        org: &str,
    ) -> Result<Vec<Repository>, String> {
        self.all(settings, token, &format!("orgs/{org}/repos?type=all")).await
    }

    /// A GitHub App's token for one organisation, asked for with the App's own signed JWT.
    pub async fn installation_token(
        &self,
        settings: &Settings,
        jwt: &str,
        org: &str,
    ) -> Result<InstallationToken, String> {
        let installation: Installation =
            self.get(settings, jwt, &format!("orgs/{org}/installation")).await.map_err(|err| {
                format!("the GitHub App is not installed in {org}, or cannot see it: {err}")
            })?;
        let path = format!("app/installations/{}/access_tokens", installation.id);
        let endpoint = settings.api.join(&path).map_err(|err| err.to_string())?;
        let answer = self
            .http
            .post(endpoint)
            .bearer_auth(jwt)
            .header("accept", "application/vnd.github+json")
            .header("x-github-api-version", "2022-11-28")
            .send()
            .await;
        sent("github", "installation-token", &answer);
        let answer = answer.map_err(|err| format!("GitHub could not be reached: {err}"))?;
        if !answer.status().is_success() {
            return Err(format!("GitHub answered {} to {path}", answer.status()));
        }
        answer.json().await.map_err(|err| format!("GitHub's {path} could not be read: {err}"))
    }

    /// One call to the API with a JSON body or none: its status, and its body when it has one.
    pub async fn call(
        &self,
        settings: &Settings,
        token: &str,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<(u16, Value), String> {
        let endpoint = settings.api.join(path).map_err(|err| err.to_string())?;
        let mut asking = self
            .http
            .request(method, endpoint)
            .bearer_auth(token)
            .header("accept", "application/vnd.github+json")
            .header("x-github-api-version", "2022-11-28");
        if let Some(body) = body {
            asking = asking.json(body);
        }
        let answer = asking.send().await;
        sent("github", "proposal", &answer);
        let answer = answer.map_err(|err| format!("GitHub could not be reached: {err}"))?;
        let status = answer.status().as_u16();
        let body = answer.json().await.unwrap_or(Value::Null);
        Ok((status, body))
    }

    /// Where GitHub points for a tarball of `repository` at `reference`: minutes long, and tokenless.
    /// Without a token only a public repository can be read, and GitHub allows 60 of these an hour.
    pub async fn archive(
        &self,
        settings: &Settings,
        token: Option<&str>,
        repository: &str,
        reference: &str,
    ) -> Result<String, (u16, String)> {
        let path = format!("repos/{repository}/tarball/{reference}");
        let endpoint = settings.api.join(&path).map_err(|err| (400, err.to_string()))?;
        let mut asking = self.pointing.get(endpoint);
        if let Some(token) = token {
            asking = asking.bearer_auth(token);
        }
        let answer = asking
            .header("accept", "application/vnd.github+json")
            .header("x-github-api-version", "2022-11-28")
            .send()
            .await;
        sent("github", "archive", &answer);
        let answer = answer.map_err(|err| (502, format!("GitHub could not be reached: {err}")))?;
        match answer.status().as_u16() {
            301 | 302 | 303 | 307 | 308 => answer
                .headers()
                .get("location")
                .and_then(|location| location.to_str().ok())
                .map(str::to_string)
                .ok_or_else(|| (502, "GitHub's redirect named nowhere".into())),
            404 if token.is_none() => Err((
                404,
                format!(
                    "{repository} has no {reference}, or is not public: a private repository needs \
                     its organisation in the plugin's organisations, with a token or App to read it"
                ),
            )),
            404 => {
                Err((404, format!("{repository} has no {reference}, or the plugin cannot read it")))
            }
            403 | 429 if token.is_none() => Err((
                429,
                "GitHub is limiting archive links fetched without a token; try again later, or give \
                 the plugin a token"
                    .into(),
            )),
            status => Err((502, format!("GitHub answered {status} to {path}"))),
        }
    }
}
