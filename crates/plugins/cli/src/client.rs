//! The backend API over HTTP, with RFC 9457 problems turned into errors a person can read.

use std::process::ExitCode;
use std::time::Duration;

use doc_secret::Secret;
use reqwest::Method;
use serde_json::Value;
use url::Url;

const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub enum Failure {
    /// The backend answered and refused, or a task being waited for did not succeed.
    Refused(String),
    TimedOut(String),
    Unreachable(String),
    Unauthorised(String),
}

impl Failure {
    pub fn exit_code(&self) -> ExitCode {
        match self {
            Self::Refused(_) => ExitCode::from(1),
            Self::TimedOut(_) => ExitCode::from(3),
            Self::Unreachable(_) | Self::Unauthorised(_) => ExitCode::from(4),
        }
    }

    pub fn message(&self) -> &str {
        match self {
            Self::Refused(message)
            | Self::TimedOut(message)
            | Self::Unreachable(message)
            | Self::Unauthorised(message) => message,
        }
    }
}

pub struct Client {
    base: Url,
    token: Option<Secret<String>>,
    http: reqwest::Client,
}

impl Client {
    pub fn new(base: &str, token: Option<Secret<String>>) -> Result<Self, Failure> {
        let base = Url::parse(&format!("{}/", base.trim_end_matches('/')))
            .map_err(|err| Failure::Refused(format!("`{base}` is not a URL: {err}")))?;
        let http = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .user_agent(concat!("doc-cli/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|err| Failure::Unreachable(err.to_string()))?;
        Ok(Self { base, token, http })
    }

    pub fn base(&self) -> &Url {
        &self.base
    }

    pub async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, Failure> {
        let mut request = self.request(method, path)?;
        if let Some(body) = body {
            request = request.json(&body);
        }
        self.send(request).await
    }

    /// A body as it is, such as the YAML `resources apply` sends or the archive `kb import` does.
    pub async fn post_body(
        &self,
        path: &str,
        content_type: &str,
        body: impl Into<reqwest::Body>,
    ) -> Result<Value, Failure> {
        let request =
            self.request(Method::POST, path)?.header("content-type", content_type).body(body);
        self.send(request).await
    }

    fn request(&self, method: Method, path: &str) -> Result<reqwest::RequestBuilder, Failure> {
        let url = self
            .base
            .join(&format!("api/v1/{}", path.trim_start_matches('/')))
            .map_err(|err| Failure::Refused(err.to_string()))?;
        let mut request = self.http.request(method, url);
        if let Some(token) = &self.token {
            request = request.bearer_auth(token.expose());
        }
        Ok(request)
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<Value, Failure> {
        let answer = request.send().await.map_err(|err| {
            Failure::Unreachable(format!("{} could not be reached: {err}", self.base))
        })?;
        let status = answer.status();
        let text = answer.text().await.map_err(|err| Failure::Unreachable(err.to_string()))?;
        let value = match text.is_empty() {
            true => Value::Null,
            false => serde_json::from_str(&text).unwrap_or(Value::String(text)),
        };
        if status.is_success() {
            return Ok(value);
        }
        let detail = value
            .get("detail")
            .or_else(|| value.get("title"))
            .and_then(Value::as_str)
            .map_or_else(|| value.to_string(), str::to_string);
        match status.as_u16() {
            401 => Err(Failure::Unauthorised(format!(
                "the backend refused the token ({detail}); pass --token, set DOC_TOKEN or run `cli login`"
            ))),
            code => Err(Failure::Refused(format!("{code}: {detail}"))),
        }
    }

    pub async fn get(&self, path: &str) -> Result<Value, Failure> {
        self.call(Method::GET, path, None).await
    }

    pub async fn post(&self, path: &str, body: Value) -> Result<Value, Failure> {
        self.call(Method::POST, path, Some(body)).await
    }

    pub async fn patch(&self, path: &str, body: Value) -> Result<Value, Failure> {
        self.call(Method::PATCH, path, Some(body)).await
    }

    pub async fn delete(&self, path: &str) -> Result<Value, Failure> {
        self.call(Method::DELETE, path, None).await
    }
}
