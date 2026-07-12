//! Reading a Google Drive folder or shared drive as a service account: a token from the account's
//! own signed JWT, the folder tree walked for Docs, Markdown and text, and the changes feed saying
//! whether anything moved since the last sync.

use std::time::Duration;

use base64::Engine;
use doc_plugin_sdk::protocol::Secret;
use doc_plugin_sdk::telemetry::sent;
use serde_json::{Value, json};

const FOLDER: &str = "application/vnd.google-apps.folder";
const DOCUMENT: &str = "application/vnd.google-apps.document";
const SCOPE: &str = "https://www.googleapis.com/auth/drive.readonly";
const FILE_LIMIT: usize = 5_000;
const DOWNLOAD_LIMIT: u64 = 10 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct File {
    pub id: String,
    pub name: String,
    pub mime: String,
    pub version: String,
    pub url: String,
    /// Folders from the source's root down to this file, by name.
    pub folders: Vec<String>,
}

impl File {
    pub fn is_document(&self) -> bool {
        self.mime == DOCUMENT
    }

    pub fn is_markdown(&self) -> bool {
        self.mime == "text/markdown"
            || self.name.ends_with(".md")
            || self.name.ends_with(".markdown")
    }

    pub fn is_text(&self) -> bool {
        self.mime == "text/plain" && !self.is_markdown()
    }
}

pub struct Drive {
    http: reqwest::Client,
    api: String,
    token: Secret<String>,
}

fn url_safe(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// The access token a service account's key earns, by signing its own assertion.
async fn token(http: &reqwest::Client, key: &Value) -> Result<String, String> {
    use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
    let email =
        key["client_email"].as_str().ok_or("the service account key names no client_email")?;
    let token_uri = key["token_uri"].as_str().unwrap_or("https://oauth2.googleapis.com/token");
    let pem = key["private_key"].as_str().ok_or("the service account key has no private_key")?;
    let body: String = pem.lines().filter(|line| !line.starts_with("-----")).collect();
    let der = base64::engine::general_purpose::STANDARD
        .decode(body.trim())
        .map_err(|err| format!("the private key is not PEM: {err}"))?;
    let pair = RsaKeyPair::from_pkcs8(&der)
        .map_err(|err| format!("the private key is not an RSA key: {err}"))?;
    let now = chrono::Utc::now().timestamp();
    let claims =
        json!({ "iss": email, "scope": SCOPE, "aud": token_uri, "iat": now, "exp": now + 3600 });
    let signed = format!(
        "{}.{}",
        url_safe(br#"{"alg":"RS256","typ":"JWT"}"#),
        url_safe(claims.to_string().as_bytes())
    );
    let mut signature = vec![0; pair.public().modulus_len()];
    pair.sign(
        &RSA_PKCS1_SHA256,
        &ring::rand::SystemRandom::new(),
        signed.as_bytes(),
        &mut signature,
    )
    .map_err(|_| "the assertion could not be signed".to_string())?;
    let assertion = format!("{signed}.{}", url_safe(&signature));
    let form = [
        ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
        ("assertion", assertion.as_str()),
    ];
    let answer = http.post(token_uri).form(&form).send().await;
    sent("google", "token", &answer);
    let answer = answer.map_err(|err| format!("Google could not be reached: {err}"))?;
    let status = answer.status();
    let body: Value = answer
        .json()
        .await
        .map_err(|err| format!("Google's token answer could not be read: {err}"))?;
    match body["access_token"].as_str() {
        Some(token) => Ok(token.to_string()),
        None => Err(format!(
            "Google refused the service account ({status}): {}",
            body["error_description"]
                .as_str()
                .or(body["error"].as_str())
                .unwrap_or("no reason given")
        )),
    }
}

impl Drive {
    pub async fn new(key: &Secret<String>) -> Result<Self, String> {
        let key: Value = serde_json::from_str(key.expose())
            .map_err(|err| format!("the credential is not a service account key: {err}"))?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("doc-kb/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|err| err.to_string())?;
        let token = Secret::new(token(&http, &key).await?);
        let api = std::env::var("DOC_KB_GOOGLE_API")
            .unwrap_or_else(|_| "https://www.googleapis.com".into());
        Ok(Self { http, api: api.trim_end_matches('/').to_string(), token })
    }

    async fn fetch(&self, path: &str, query: &[(&str, &str)]) -> Result<reqwest::Response, String> {
        let answer = self
            .http
            .get(format!("{}/drive/v3/{path}", self.api))
            .query(query)
            .bearer_auth(self.token.expose())
            .send()
            .await;
        sent("google", "drive", &answer);
        let answer = answer.map_err(|err| format!("Google Drive could not be reached: {err}"))?;
        match answer.status().as_u16() {
            200..=299 => Ok(answer),
            status => Err(format!("Google Drive answered {status} to {path}")),
        }
    }

    async fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value, String> {
        self.fetch(path, query)
            .await?
            .json()
            .await
            .map_err(|err| format!("Google Drive's answer could not be read: {err}"))
    }

    /// Where the changes feed stands now, to ask later what changed since.
    pub async fn start_token(&self) -> Result<String, String> {
        let answer = self.get("changes/startPageToken", &[("supportsAllDrives", "true")]).await?;
        answer["startPageToken"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| "Google Drive gave no start token".into())
    }

    /// Whether anything changed since `since`, and where the feed stands afterwards.
    pub async fn changed_since(&self, since: &str) -> Result<(bool, String), String> {
        let mut token = since.to_string();
        let mut changed = false;
        loop {
            let answer = self
                .get(
                    "changes",
                    &[
                        ("pageToken", token.as_str()),
                        ("supportsAllDrives", "true"),
                        ("includeItemsFromAllDrives", "true"),
                        ("fields", "nextPageToken,newStartPageToken,changes(fileId,removed)"),
                    ],
                )
                .await?;
            changed |= answer["changes"].as_array().is_some_and(|changes| !changes.is_empty());
            match (answer["nextPageToken"].as_str(), answer["newStartPageToken"].as_str()) {
                (Some(next), _) => token = next.to_string(),
                (None, Some(fresh)) => return Ok((changed, fresh.to_string())),
                (None, None) => return Ok((changed, token)),
            }
        }
    }

    /// Every Doc, Markdown and text file under `folder`, depth first, folders by name.
    pub async fn files(&self, folder: &str) -> Result<Vec<File>, String> {
        let mut found = Vec::new();
        let mut pending = vec![(folder.to_string(), Vec::<String>::new())];
        while let Some((parent, path)) = pending.pop() {
            let mut children = Vec::new();
            let mut page: Option<String> = None;
            loop {
                let q = format!("'{parent}' in parents and trashed = false");
                let mut query = vec![
                    ("q", q.as_str()),
                    ("supportsAllDrives", "true"),
                    ("includeItemsFromAllDrives", "true"),
                    ("pageSize", "1000"),
                    ("fields", "nextPageToken,files(id,name,mimeType,version,webViewLink)"),
                ];
                if let Some(page) = &page {
                    query.push(("pageToken", page.as_str()));
                }
                let answer = self.get("files", &query).await?;
                children.extend(answer["files"].as_array().cloned().unwrap_or_default());
                page = answer["nextPageToken"].as_str().map(str::to_string);
                if page.is_none() {
                    break;
                }
            }
            children.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
            let mut folders = Vec::new();
            for child in children {
                let text = |key: &str| child[key].as_str().unwrap_or_default().to_string();
                let file = File {
                    id: text("id"),
                    name: text("name"),
                    mime: text("mimeType"),
                    version: text("version"),
                    url: text("webViewLink"),
                    folders: path.clone(),
                };
                if file.mime == FOLDER {
                    let mut below = path.clone();
                    below.push(file.name.clone());
                    folders.push((file.id, below));
                } else if file.is_document() || file.is_markdown() || file.is_text() {
                    found.push(file);
                }
            }
            pending.extend(folders.into_iter().rev());
            if found.len() > FILE_LIMIT {
                return Err(format!("a folder of more than {FILE_LIMIT} files is not synced"));
            }
        }
        Ok(found)
    }

    /// A Doc exported as HTML, or a file's own content.
    pub async fn content(&self, file: &File) -> Result<String, String> {
        let answer = match file.is_document() {
            true => {
                self.fetch(&format!("files/{}/export", file.id), &[("mimeType", "text/html")])
                    .await?
            }
            false => {
                self.fetch(
                    &format!("files/{}", file.id),
                    &[("alt", "media"), ("supportsAllDrives", "true")],
                )
                .await?
            }
        };
        if answer.content_length().is_some_and(|length| length > DOWNLOAD_LIMIT) {
            return Err(format!("{} is too large to import", file.name));
        }
        answer.text().await.map_err(|err| format!("{} could not be read: {err}", file.name))
    }
}
