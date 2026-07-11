//! Changes proposed to a repository from DOC: a file read for another plugin to edit from, and a
//! pull request carrying a changed file, made on a branch of its own so the branch it targets,
//! protected or not, changes only when somebody merges it.

use base64::Engine;
use chrono::Utc;
use reqwest::Method;
use serde::Deserialize;
use serde_json::{Value, json};
use url::form_urlencoded::Serializer;

use crate::github::GitHub;
use crate::settings::Settings;
use crate::sync::Tokens;

/// The permission opening a pull request needs, besides writing to this plugin. It names an
/// ability, so it counts at any scope.
pub const PULL_REQUESTS: &str = "pull-requests";

/// A status and why, as a refusal is answered.
type Refused = (u16, String);

#[derive(Deserialize)]
pub struct FileRequest {
    pub repository: String,
    #[serde(default, rename = "ref")]
    pub reference: Option<String>,
    pub path: String,
}

/// One file's new content, proposed to `base`, or the repository's default branch.
#[derive(Deserialize)]
pub struct Proposal {
    pub repository: String,
    #[serde(default)]
    pub base: Option<String>,
    pub path: String,
    pub content: String,
    /// The branch an earlier proposal of the same change used, to bring its pull request up to
    /// date rather than open another.
    #[serde(default)]
    pub branch: Option<String>,
    pub title: String,
    pub message: String,
    #[serde(default)]
    pub body: String,
}

/// A repository in one of the plugin's organisations, and the token to use with it.
async fn token_for(
    github: &GitHub,
    settings: &Settings,
    tokens: &Tokens,
    repository: &str,
) -> Result<String, Refused> {
    let (org, _) = repository
        .split_once('/')
        .filter(|(org, name)| !org.is_empty() && !name.is_empty() && !name.contains('/'))
        .ok_or_else(|| (400, format!("`{repository}` is not written owner/name")))?;
    if !settings.organisations.iter().any(|allowed| allowed.eq_ignore_ascii_case(org)) {
        return Err((403, format!("{org} is not one of the plugin's organisations")));
    }
    let token = tokens.for_org(github, settings, org).await.map_err(|err| (503, err))?;
    Ok(token.expose().clone())
}

/// A path as it goes into an address, each part escaped and the `/` between them kept.
fn escaped(path: &str) -> String {
    let part = |part: &str| -> String {
        part.bytes()
            .map(|byte| match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    char::from(byte).to_string()
                }
                byte => format!("%{byte:02X}"),
            })
            .collect()
    };
    path.split('/').map(part).collect::<Vec<_>>().join("/")
}

/// A branch named on purpose: `HEAD` and nothing both mean the default one.
fn named(reference: Option<&str>) -> Option<&str> {
    reference.map(str::trim).filter(|reference| !reference.is_empty() && *reference != "HEAD")
}

/// A branch of the plugin's own for a file, readable in a list of branches.
fn branch_for(path: &str) -> String {
    let mut slug = String::new();
    for c in path.chars() {
        match c {
            c if c.is_ascii_alphanumeric() => slug.push(c.to_ascii_lowercase()),
            _ if !slug.ends_with('-') && !slug.is_empty() => slug.push('-'),
            _ => {}
        }
    }
    let slug: String = slug.trim_end_matches('-').chars().take(60).collect();
    format!("doc/{slug}-{}", Utc::now().format("%Y%m%d%H%M%S"))
}

/// What GitHub's refusal of `what` means, in words.
fn refused(status: u16, body: &Value, what: &str) -> Refused {
    let said = body["message"].as_str().map(|said| format!(" ({said})")).unwrap_or_default();
    match status {
        401 | 403 => (
            403,
            format!(
                "GitHub would not {what}{said}: the plugin's token or App needs to be allowed to \
                 write contents and pull requests"
            ),
        ),
        404 => (404, format!("GitHub has nothing to {what}{said}")),
        422 => (422, format!("GitHub would not {what}{said}")),
        _ => (502, format!("GitHub answered {status} when asked to {what}{said}")),
    }
}

fn content_of(file: &Value) -> Result<String, Refused> {
    let encoded: String = file["content"]
        .as_str()
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|err| (502, format!("GitHub's copy of the file could not be read: {err}")))?;
    String::from_utf8(bytes).map_err(|_| (400, "that file is not text".to_string()))
}

/// The calls one proposal makes, all to the one repository with the one token.
struct Repository<'a> {
    github: &'a GitHub,
    settings: &'a Settings,
    token: String,
    name: &'a str,
}

impl Repository<'_> {
    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<(u16, Value), Refused> {
        let path = match path {
            "" => format!("repos/{}", self.name),
            path => format!("repos/{}/{path}", self.name),
        };
        self.github
            .call(self.settings, &self.token, method, &path, body.as_ref())
            .await
            .map_err(|err| (502, err))
    }

    /// The file at `path` on `branch`, or none where the branch has no such file.
    async fn file(&self, path: &str, branch: Option<&str>) -> Result<Option<Value>, Refused> {
        let at = match branch {
            Some(branch) => Serializer::new(String::from("?")).append_pair("ref", branch).finish(),
            None => String::new(),
        };
        match self.call(Method::GET, &format!("contents/{}{at}", escaped(path)), None).await? {
            (200, file) => Ok(Some(file)),
            (404, _) => Ok(None),
            (status, body) => Err(refused(status, &body, &format!("read {path}"))),
        }
    }
}

/// One file as a branch of the repository has it, for a plugin to edit from.
pub async fn file(
    github: &GitHub,
    settings: &Settings,
    tokens: &Tokens,
    asked: &FileRequest,
) -> Result<Value, Refused> {
    let token = token_for(github, settings, tokens, &asked.repository).await?;
    let repository = Repository { github, settings, token, name: &asked.repository };
    let file = repository
        .file(&asked.path, named(asked.reference.as_deref()))
        .await?
        .ok_or_else(|| (404, format!("{} has no {}", asked.repository, asked.path)))?;
    Ok(json!({ "content": content_of(&file)?, "sha": file["sha"] }))
}

/// Puts the file on a branch of its own and opens a pull request from it, or, with the branch of
/// an earlier proposal, brings that pull request up to date. Where the pull request is.
pub async fn propose(
    github: &GitHub,
    settings: &Settings,
    tokens: &Tokens,
    asked: &Proposal,
    by: &str,
) -> Result<Value, Refused> {
    let token = token_for(github, settings, tokens, &asked.repository).await?;
    let repository = Repository { github, settings, token, name: &asked.repository };
    let base = match named(asked.base.as_deref()) {
        Some(base) => base.to_string(),
        None => match repository.call(Method::GET, "", None).await? {
            (200, found) => found["default_branch"].as_str().unwrap_or("main").to_string(),
            (status, body) => return Err(refused(status, &body, "find the repository")),
        },
    };
    let branch = asked
        .branch
        .clone()
        .filter(|branch| {
            branch.starts_with("doc/")
                && branch.chars().all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c))
        })
        .unwrap_or_else(|| branch_for(&asked.path));
    // The branch, made from the base where it is not there yet: a merged pull request's branch
    // is usually deleted, and its change then starts again from where the base is now.
    match repository.call(Method::GET, &format!("git/ref/heads/{}", escaped(&branch)), None).await?
    {
        (200, _) => {}
        (404, _) => {
            let head = match repository
                .call(Method::GET, &format!("git/ref/heads/{}", escaped(&base)), None)
                .await?
            {
                (200, head) => head,
                (status, body) => return Err(refused(status, &body, &format!("find {base}"))),
            };
            let made =
                json!({ "ref": format!("refs/heads/{branch}"), "sha": head["object"]["sha"] });
            match repository.call(Method::POST, "git/refs", Some(made)).await? {
                (201 | 422, _) => {}
                (status, body) => return Err(refused(status, &body, "make a branch")),
            }
        }
        (status, body) => return Err(refused(status, &body, &format!("find {branch}"))),
    }
    // An update names the file it replaces; nothing is committed when nothing would change.
    let current = repository.file(&asked.path, Some(&branch)).await?;
    let same = current
        .as_ref()
        .is_some_and(|current| content_of(current).ok().as_deref() == Some(asked.content.as_str()));
    if !same {
        let mut put = json!({
            "message": asked.message,
            "content": base64::engine::general_purpose::STANDARD.encode(&asked.content),
            "branch": branch,
        });
        if let Some(current) = &current {
            put["sha"] = current["sha"].clone();
        }
        let path = format!("contents/{}", escaped(&asked.path));
        match repository.call(Method::PUT, &path, Some(put)).await? {
            (200 | 201, _) => {}
            (status, body) => {
                return Err(refused(status, &body, &format!("commit {}", asked.path)));
            }
        }
    }
    let owner = asked.repository.split('/').next().unwrap_or_default();
    let open = Serializer::new(String::new())
        .append_pair("head", &format!("{owner}:{branch}"))
        .append_pair("state", "open")
        .finish();
    let pull = match repository.call(Method::GET, &format!("pulls?{open}"), None).await? {
        (200, Value::Array(found)) if !found.is_empty() => found[0].clone(),
        (200, _) => {
            let opened = json!({
                "title": asked.title,
                "head": branch,
                "base": base,
                "body": format!("{}\n\nOpened from DOC by {by}.", asked.body).trim().to_string(),
            });
            match repository.call(Method::POST, "pulls", Some(opened)).await? {
                (201, pull) => pull,
                (status, body) => return Err(refused(status, &body, "open a pull request")),
            }
        }
        (status, body) => return Err(refused(status, &body, "look for an open pull request")),
    };
    Ok(json!({
        "url": pull["html_url"], "number": pull["number"], "branch": branch, "base": base,
    }))
}
