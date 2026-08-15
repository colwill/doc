//! The Jira calls the plugin makes, the same for Cloud and Data Center but for what differs: how it
//! signs in, the REST API's version, and search — Cloud's enhanced search pages by token
//! (`/rest/api/3/search/jql`), Data Center's by offset (`/rest/api/2/search`).

use std::time::Duration;

use doc_plugin_sdk::telemetry::sent;
use serde_json::Value;

use crate::settings::{Auth, Config};

const TIMEOUT: Duration = Duration::from_secs(20);
pub const PAGE: usize = 100;
/// The most projects read when the settings name none.
pub const MAX_PROJECTS: usize = 200;
/// What an issue is read with.
const FIELDS: &str = "summary,status,issuetype,priority,fixVersions,components,labels,created,updated,resolutiondate";

/// A call Jira refused or could not answer, keeping the status so a rate limit or a bad token can
/// be told apart.
#[derive(Debug)]
pub struct Refused {
    pub status: Option<u16>,
    pub detail: String,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

/// Where a search carries on from: Cloud's next page token, or Data Center's offset.
#[derive(Debug, Clone)]
pub enum Cursor {
    Token(String),
    Offset(usize),
}

#[derive(Clone)]
pub struct Jira {
    http: reqwest::Client,
    base: String,
    auth: Auth,
    dc: bool,
}

impl Jira {
    pub fn new(config: &Config, dc: bool) -> Result<Self, String> {
        let base = config.base.clone().ok_or("no Jira URL is set")?;
        let auth = config.auth.clone().ok_or("no credentials for Jira are set")?;
        let http = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .user_agent(concat!("doc-jira/", env!("CARGO_PKG_VERSION")))
            // A credential is only ever sent to the Jira URL the settings name.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|err| format!("the HTTP client could not be built: {err}"))?;
        Ok(Self { http, base, auth, dc })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    fn api(&self) -> &'static str {
        if self.dc { "2" } else { "3" }
    }

    async fn get(
        &self,
        path: &str,
        query: &[(&str, String)],
        operation: &str,
    ) -> Result<Value, Refused> {
        let url = format!("{}/rest/api/{}/{path}", self.base, self.api());
        let request = self.http.get(&url).query(query).header("accept", "application/json");
        let request = match &self.auth {
            Auth::Basic { email, token } => request.basic_auth(email, Some(token.expose())),
            Auth::Bearer(token) => request.bearer_auth(token.expose()),
        };
        let answer = request.send().await;
        sent("jira", operation, &answer);
        let answer = answer.map_err(|err| Refused {
            status: None,
            detail: format!("Jira could not be reached at {}: {err}", self.base),
        })?;
        let status = answer.status().as_u16();
        if status != 200 {
            let body: Value = answer.json().await.unwrap_or_default();
            let said = body["errorMessages"]
                .as_array()
                .and_then(|messages| messages.first())
                .and_then(Value::as_str)
                .or_else(|| body["message"].as_str())
                .map(|said| format!(": {said}"))
                .unwrap_or_default();
            let detail = match status {
                401 => "Jira refused these credentials".to_string(),
                403 => format!("Jira forbade this account from {operation}{said}"),
                404 => format!("Jira has no such {operation} here{said}"),
                429 => "Jira's rate limit was reached; the next read carries on".to_string(),
                300..=399 => format!(
                    "Jira answered {status}, a redirect: is the Jira URL exactly where it is?"
                ),
                status => format!("Jira answered {status}{said}"),
            };
            return Err(Refused { status: Some(status), detail });
        }
        answer.json().await.map_err(|err| Refused {
            status: Some(status),
            detail: format!("Jira's answer was not JSON: {err}"),
        })
    }

    /// Who the credentials belong to: how a token is checked before it is stored.
    pub async fn myself(&self) -> Result<String, Refused> {
        let me = self.get("myself", &[], "myself").await?;
        Ok(me["displayName"]
            .as_str()
            .or_else(|| me["emailAddress"].as_str())
            .or_else(|| me["name"].as_str())
            .unwrap_or("an account with no name")
            .to_string())
    }

    /// The projects named, or every project the account can see, up to [`MAX_PROJECTS`].
    pub async fn projects(&self, keys: &[String]) -> Result<Vec<Value>, Refused> {
        if !keys.is_empty() {
            let mut found = Vec::new();
            for key in keys {
                found.push(self.get(&format!("project/{key}"), &[], "project").await?);
            }
            return Ok(found);
        }
        if self.dc {
            let listed = self.get("project", &[], "projects").await?;
            return Ok(listed
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .take(MAX_PROJECTS)
                .collect());
        }
        let mut found = Vec::new();
        while found.len() < MAX_PROJECTS {
            let query = [("startAt", found.len().to_string()), ("maxResults", "50".to_string())];
            let page = self.get("project/search", &query, "projects").await?;
            let values = page["values"].as_array().cloned().unwrap_or_default();
            let last = page["isLast"].as_bool().unwrap_or(true) || values.is_empty();
            found.extend(values);
            if last {
                break;
            }
        }
        found.truncate(MAX_PROJECTS);
        Ok(found)
    }

    /// Every version of a project, released, unreleased and archived.
    pub async fn versions(&self, key: &str) -> Result<Vec<Value>, Refused> {
        let listed = self.get(&format!("project/{key}/versions"), &[], "versions").await?;
        Ok(listed.as_array().cloned().unwrap_or_default())
    }

    /// One page of the issues a JQL query finds, and where the next page starts.
    pub async fn search(
        &self,
        jql: &str,
        cursor: Option<&Cursor>,
    ) -> Result<(Vec<Value>, Option<Cursor>), Refused> {
        let mut query = vec![
            ("jql", jql.to_string()),
            ("fields", FIELDS.to_string()),
            ("maxResults", PAGE.to_string()),
        ];
        if self.dc {
            let start = match cursor {
                Some(Cursor::Offset(start)) => *start,
                _ => 0,
            };
            query.push(("startAt", start.to_string()));
            let page = self.get("search", &query, "search").await?;
            let issues = page["issues"].as_array().cloned().unwrap_or_default();
            let total = page["total"].as_u64().unwrap_or_default() as usize;
            let next = start + issues.len();
            let more = !issues.is_empty() && next < total;
            return Ok((issues, more.then_some(Cursor::Offset(next))));
        }
        if let Some(Cursor::Token(token)) = cursor {
            query.push(("nextPageToken", token.clone()));
        }
        let page = self.get("search/jql", &query, "search").await?;
        let issues = page["issues"].as_array().cloned().unwrap_or_default();
        let next = match (page["isLast"].as_bool(), page["nextPageToken"].as_str()) {
            (Some(true), _) | (_, None) => None,
            (_, Some(token)) => Some(Cursor::Token(token.to_string())),
        };
        Ok((issues, next))
    }
}
