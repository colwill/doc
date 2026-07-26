//! Publishing what a template made: a repository, and one commit holding every rendered file.
//! Only GitHub is written here; another host is another module with the same two functions.

use std::time::Duration;

use base64::Engine;
use doc_plugin_sdk::Settings;
use doc_plugin_sdk::telemetry::sent;
use serde_json::{Value, json};

use crate::files::Rendered;

pub const API: &str = "github-api";
pub const TOKEN: &str = "github-token";
pub const OWNER: &str = "github-owner";

const TIMEOUT: Duration = Duration::from_secs(20);
const DEFAULT_API: &str = "https://api.github.com";

/// What a publish step asks for, with everything already rendered.
#[derive(Debug, Clone)]
pub struct Spec {
    pub owner: String,
    pub repository: String,
    pub description: String,
    pub private: bool,
    pub branch: Option<String>,
    pub message: String,
}

pub struct GitHub {
    client: reqwest::Client,
    api: String,
    token: String,
}

impl GitHub {
    /// The client the settings describe, or what is missing before one can be made.
    pub fn new(settings: &Settings) -> Result<Self, String> {
        let token = settings.secret(TOKEN).ok_or(
            "this plugin has no GitHub token: an administrator sets one on its Settings page",
        )?;
        let api = settings.some_text(API).unwrap_or_else(|| DEFAULT_API.to_string());
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .user_agent(concat!("doc-templates/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|err| format!("the GitHub client could not be made: {err}"))?;
        Ok(Self {
            client,
            api: api.trim_end_matches('/').to_string(),
            token: token.expose().to_string(),
        })
    }

    async fn call(
        &self,
        operation: &str,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<(u16, Value), String> {
        let url = format!("{}/{}", self.api, path.trim_start_matches('/'));
        let mut asking = self
            .client
            .request(method, &url)
            .bearer_auth(&self.token)
            .header("accept", "application/vnd.github+json")
            .header("x-github-api-version", "2022-11-28");
        if let Some(body) = body {
            asking = asking.json(&body);
        }
        let answer = asking.send().await;
        sent("github", operation, &answer);
        let answer = answer.map_err(|err| format!("GitHub could not be reached: {err}"))?;
        let status = answer.status().as_u16();
        let value: Value = answer.json().await.unwrap_or(Value::Null);
        Ok((status, value))
    }

    /// What GitHub said was wrong, as a person should read it.
    fn refused(status: u16, answer: &Value, doing: &str) -> String {
        let said = answer["message"].as_str().unwrap_or("it gave no reason");
        let errors = answer["errors"]
            .as_array()
            .map(|errors| {
                errors
                    .iter()
                    .filter_map(|error| error["message"].as_str().or(error["code"].as_str()))
                    .collect::<Vec<_>>()
                    .join("; ")
            })
            .filter(|errors| !errors.is_empty());
        match errors {
            Some(errors) => format!("GitHub answered {status} to {doing}: {said} ({errors})"),
            None => format!("GitHub answered {status} to {doing}: {said}"),
        }
    }

    /// Creates the repository, under an organisation if `owner` is one and under the token's own
    /// account if it is not.
    async fn create(&self, spec: &Spec) -> Result<Value, String> {
        let body = json!({
            "name": spec.repository,
            "description": spec.description,
            "private": spec.private,
            "auto_init": true,
        });
        let (status, answer) = self
            .call(
                "create-repository",
                reqwest::Method::POST,
                &format!("orgs/{}/repos", spec.owner),
                Some(body.clone()),
            )
            .await?;
        let (status, answer) = match status {
            404 => {
                self.call("create-repository", reqwest::Method::POST, "user/repos", Some(body))
                    .await?
            }
            _ => (status, answer),
        };
        match status {
            201 => Ok(answer),
            422 => Err(format!(
                "GitHub would not create {}/{}: it may already exist, or the name is not allowed",
                spec.owner, spec.repository
            )),
            401 | 403 => Err(format!(
                "GitHub refused the plugin's token for {}/{}: it needs to be allowed to create repositories there",
                spec.owner, spec.repository
            )),
            status => Err(Self::refused(status, &answer, "creating the repository")),
        }
    }

    /// One commit holding every file, on top of the commit `auto_init` made.
    async fn commit(
        &self,
        full_name: &str,
        branch: &str,
        message: &str,
        files: &[Rendered],
    ) -> Result<String, String> {
        let (status, head) = self
            .call(
                "read-ref",
                reqwest::Method::GET,
                &format!("repos/{full_name}/git/ref/heads/{branch}"),
                None,
            )
            .await?;
        if status != 200 {
            return Err(Self::refused(status, &head, "reading the branch"));
        }
        let parent = head["object"]["sha"].as_str().unwrap_or_default().to_string();
        let (status, commit) = self
            .call(
                "read-commit",
                reqwest::Method::GET,
                &format!("repos/{full_name}/git/commits/{parent}"),
                None,
            )
            .await?;
        if status != 200 {
            return Err(Self::refused(status, &commit, "reading the first commit"));
        }
        let base = commit["tree"]["sha"].as_str().unwrap_or_default().to_string();

        let mut entries = Vec::new();
        for file in files {
            let entry = match &file.text {
                Some(text) => json!({
                    "path": file.path,
                    "mode": "100644",
                    "type": "blob",
                    "content": text,
                }),
                None => {
                    let encoded = base64::engine::general_purpose::STANDARD.encode(&file.bytes);
                    let (status, blob) = self
                        .call(
                            "create-blob",
                            reqwest::Method::POST,
                            &format!("repos/{full_name}/git/blobs"),
                            Some(json!({ "content": encoded, "encoding": "base64" })),
                        )
                        .await?;
                    if status != 201 {
                        return Err(Self::refused(
                            status,
                            &blob,
                            &format!("uploading {}", file.path),
                        ));
                    }
                    json!({
                        "path": file.path,
                        "mode": "100644",
                        "type": "blob",
                        "sha": blob["sha"],
                    })
                }
            };
            entries.push(entry);
        }

        let (status, tree) = self
            .call(
                "create-tree",
                reqwest::Method::POST,
                &format!("repos/{full_name}/git/trees"),
                Some(json!({ "base_tree": base, "tree": entries })),
            )
            .await?;
        if status != 201 {
            return Err(Self::refused(status, &tree, "writing the files"));
        }
        let (status, made) = self
            .call(
                "create-commit",
                reqwest::Method::POST,
                &format!("repos/{full_name}/git/commits"),
                Some(json!({ "message": message, "tree": tree["sha"], "parents": [parent] })),
            )
            .await?;
        if status != 201 {
            return Err(Self::refused(status, &made, "making the commit"));
        }
        let sha = made["sha"].as_str().unwrap_or_default().to_string();
        let (status, moved) = self
            .call(
                "move-ref",
                reqwest::Method::PATCH,
                &format!("repos/{full_name}/git/refs/heads/{branch}"),
                Some(json!({ "sha": sha })),
            )
            .await?;
        if status != 200 {
            return Err(Self::refused(status, &moved, "moving the branch"));
        }
        Ok(sha)
    }

    /// Whether the token works and who it is, for the Settings page's **Test connection**.
    pub async fn whoami(&self) -> Result<String, String> {
        let (status, answer) = self.call("whoami", reqwest::Method::GET, "user", None).await?;
        match status {
            200 => Ok(answer["login"].as_str().unwrap_or("someone").to_string()),
            401 => Err("GitHub refused this token".into()),
            status => Err(Self::refused(status, &answer, "checking the token")),
        }
    }
}

/// Creates the repository and commits the files, answering what the step outputs.
pub async fn publish(github: &GitHub, spec: &Spec, files: &[Rendered]) -> Result<Value, String> {
    let repository = github.create(spec).await?;
    let full_name = repository["full_name"].as_str().unwrap_or_default().to_string();
    let branch = spec
        .branch
        .clone()
        .filter(|branch| !branch.is_empty())
        .or_else(|| repository["default_branch"].as_str().map(str::to_string))
        .unwrap_or_else(|| "main".to_string());
    let commit = match files.is_empty() {
        true => None,
        false => Some(github.commit(&full_name, &branch, &spec.message, files).await?),
    };
    Ok(json!({
        "repository": full_name,
        "name": repository["name"],
        "owner": repository["owner"]["login"],
        "url": repository["html_url"],
        "clone_url": repository["clone_url"],
        "ssh_url": repository["ssh_url"],
        "branch": branch,
        "commit": commit,
        "files": files.len(),
        "private": spec.private,
    }))
}
