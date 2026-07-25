//! An automation's actions, carried out as its owner. Slack and SMTP settings come from the plugin's
//! environment; a message with no address of its own goes to the resource's team.

use std::time::Duration;

use doc_plugin_sdk::{Backend, telemetry};
use hmac::{KeyInit, Mac};
use serde_json::{Value, json};
use url::form_urlencoded::byte_serialize;

use crate::conditions::{render, render_value};
use crate::model::{Action, published_topic};

const WEBHOOK_TRIES: u32 = 3;
const EXCERPT: usize = 2_000;

/// What core last told the plugin it is configured with (ADR-0007). An action reaches for a
/// credential while it runs, with no `Backend` in scope, so the settings are kept here and
/// refreshed at every `load` — which the SDK also does after a settings change.
static CONFIGURED: std::sync::RwLock<Option<std::sync::Arc<doc_plugin_sdk::Settings>>> =
    std::sync::RwLock::new(None);

pub fn remember(backend: &Backend) {
    if let Ok(mut held) = CONFIGURED.write() {
        *held = Some(backend.settings());
    }
}

/// A setting by the variable that names it: what an administrator set on the Settings page, or
/// what the deployment gives in `DOC_AUTOMATION_*` (or the file `<NAME>_FILE` names).
pub fn setting(name: &str) -> Option<String> {
    let configured = CONFIGURED.read().ok().and_then(|held| held.clone());
    let set = configured.and_then(|settings| settings.by_variable("automation", name));
    set.or_else(|| {
        let direct = std::env::var(name).ok().filter(|value| !value.trim().is_empty());
        direct.or_else(|| {
            let path = std::env::var(format!("{name}_FILE")).ok()?;
            std::fs::read_to_string(path).ok().map(|value| value.trim().to_string())
        })
    })
}

fn excerpt(text: &str) -> String {
    text.chars().take(EXCERPT).collect()
}

/// `sha256=<hex>` of HMAC-SHA256 over `body`, as GitHub signs its webhooks.
pub fn signature(secret: &str, body: &[u8]) -> String {
    let mut mac = <hmac::Hmac<sha2::Sha256> as KeyInit>::new_from_slice(secret.as_bytes())
        .unwrap_or_else(|_| unreachable!("HMAC takes a key of any length"));
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent(concat!("doc-automation/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|err| err.to_string())
}

/// What the resource's team is, read as the owner once and only if an action needs it.
pub struct Team<'a> {
    backend: &'a Backend,
    resource: String,
    found: Option<Result<Value, String>>,
}

fn path_of(kind: &str, name: &str) -> String {
    let name: Vec<String> = name
        .split('/')
        .map(|part| byte_serialize(part.as_bytes()).collect::<String>().replace('+', "%20"))
        .collect();
    format!("resources/{kind}/{}", name.join("/"))
}

impl<'a> Team<'a> {
    pub fn of(backend: &'a Backend, resource: &str) -> Self {
        Self { backend, resource: resource.to_string(), found: None }
    }

    async fn read(&self, kind: &str, name: &str) -> Result<Value, String> {
        match self.backend.ask("resources", "GET", &path_of(kind, name), None, None).await {
            Ok((200, mut body)) => Ok(body["resource"].take()),
            Ok((status, body)) => Err(format!(
                "Resource Definitions answered {status} for {kind}:{name}: {}",
                body["detail"].as_str().unwrap_or("no reason given")
            )),
            Err(err) => Err(format!("Resource Definitions could not be asked: {err}")),
        }
    }

    pub async fn get(&mut self) -> Result<Value, String> {
        if self.found.is_none() {
            let (kind, name) = self.resource.split_once(':').unwrap_or(("", &self.resource));
            let found = match kind {
                "team" => self.read("team", name).await,
                _ => match self.read(kind, name).await {
                    Ok(resource) => match resource["owner"].as_str() {
                        Some(team) => self.read("team", team).await,
                        None => Err(format!("{} has no owning team", self.resource)),
                    },
                    Err(err) => Err(err),
                },
            };
            self.found = Some(found);
        }
        self.found.clone().unwrap_or_else(|| Err("no team".into()))
    }

    async fn field(&mut self, key: &str, what: &str) -> Result<String, String> {
        let team = self.get().await?;
        match team[key].as_str().filter(|value| !value.trim().is_empty()) {
            Some(value) => Ok(value.to_string()),
            None => Err(format!(
                "{}'s team {} has no {what} in Resource Definitions, so name one in the action",
                self.resource,
                team["name"].as_str().unwrap_or("?")
            )),
        }
    }
}

/// Does one action, answering what it did or why it could not.
pub async fn perform(
    backend: &Backend,
    action: &Action,
    context: &Value,
    team: &mut Team<'_>,
) -> Result<Value, String> {
    match action {
        Action::Webhook { url, headers, body, secret } => {
            let url = render(url, context);
            let body = match body {
                Some(body) => render_value(body, context),
                None => context.clone(),
            };
            let headers: Vec<(String, String)> = headers
                .iter()
                .map(|(name, value)| (name.clone(), render(value, context)))
                .collect();
            webhook(&url, &headers, &body, secret.as_ref().map(|secret| secret.expose().as_str()))
                .await
        }
        Action::Slack { channel, text } => {
            // A team's Slack channel was kept in the catalogue until T69; the Slack plugin (T70)
            // matches teams to Slack, so until then an action names its own channel.
            let Some(channel) = channel else {
                return Err("name the Slack channel in the action: a team no longer carries one"
                    .to_string());
            };
            let channel = render(channel, context);
            slack(&channel, &render(text, context)).await
        }
        Action::Email { to, subject, body } => {
            let to = match to {
                Some(to) => render(to, context),
                None => team.field("email", "email address").await?,
            };
            email(&to, &render(subject, context), &render(body, context)).await
        }
        Action::Event { topic, payload } => {
            let topic = published_topic(topic);
            let payload = match payload {
                Some(payload) => render_value(payload, context),
                None => context.clone(),
            };
            let id = backend.publish(&topic, payload).await.map_err(|err| err.to_string())?;
            Ok(json!({ "topic": topic, "event": id }))
        }
        Action::Notify { user, title, body, url } => {
            let user = render(user, context);
            let url = url.as_deref().map(|url| render(url, context));
            backend
                .notify(&user, &render(title, context), &render(body, context), url.as_deref())
                .await
                .map_err(|err| err.to_string())?;
            Ok(json!({ "user": user }))
        }
        Action::Operation { plugin, operation, params } => {
            let declared = crate::operations::find(backend, plugin, operation)
                .await
                .map_err(|refusal| refusal.detail)?;
            let call = crate::operations::call(&declared, params, context);
            let asked = backend
                .ask(plugin, &call.method, &call.route, call.query.as_deref(), call.body)
                .await;
            match asked {
                Ok((status, answer)) if (200..300).contains(&status) => {
                    Ok(json!({ "status": status, "body": answer }))
                }
                Ok((status, answer)) => Err(format!(
                    "{} ({plugin}) answered {status}: {}",
                    declared.label,
                    answer["detail"]
                        .as_str()
                        .map_or_else(|| excerpt(&answer.to_string()), str::to_string)
                )),
                Err(err) => Err(format!("{plugin} could not be asked: {err}")),
            }
        }
        Action::Request { plugin, method, route, query, body } => {
            let route = render(route, context);
            let query = query.as_ref().map(|query| render(query, context));
            let body = body.as_ref().map(|body| render_value(body, context));
            match backend.ask(plugin, method, &route, query.as_deref(), body).await {
                Ok((status, answer)) if (200..300).contains(&status) => {
                    Ok(json!({ "status": status, "body": answer }))
                }
                Ok((status, answer)) => Err(format!(
                    "{plugin} answered {status} to {method} api/{route}: {}",
                    answer["detail"]
                        .as_str()
                        .map_or_else(|| excerpt(&answer.to_string()), str::to_string)
                )),
                Err(err) => Err(format!("{plugin} could not be asked: {err}")),
            }
        }
    }
}

async fn webhook(
    url: &str,
    headers: &[(String, String)],
    body: &Value,
    secret: Option<&str>,
) -> Result<Value, String> {
    let client = client()?;
    let bytes = serde_json::to_vec(body).map_err(|err| err.to_string())?;
    let mut last = String::new();
    for attempt in 1..=WEBHOOK_TRIES {
        let mut request =
            client.post(url).header("content-type", "application/json").body(bytes.clone());
        for (name, value) in headers {
            request = request.header(name.as_str(), value.as_str());
        }
        if let Some(secret) = secret {
            request = request.header("x-doc-signature", signature(secret, &bytes));
        }
        let answer = request.send().await;
        telemetry::sent("webhook", "post", &answer);
        let retry = match answer {
            Ok(answer) => {
                let status = answer.status();
                let text = answer.text().await.unwrap_or_default();
                if status.is_success() {
                    return Ok(
                        json!({ "status": status.as_u16(), "body": excerpt(&text), "attempts": attempt }),
                    );
                }
                last = format!("{url} answered {}: {}", status.as_u16(), excerpt(&text));
                status.is_server_error() || status.as_u16() == 429
            }
            Err(err) => {
                last = format!("{url} could not be reached: {err}");
                true
            }
        };
        if !retry || attempt == WEBHOOK_TRIES {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500 * 3u64.pow(attempt - 1))).await;
    }
    Err(last)
}

async fn slack(channel: &str, text: &str) -> Result<Value, String> {
    let token = setting("DOC_AUTOMATION_SLACK_TOKEN")
        .ok_or("Slack is not set up: set DOC_AUTOMATION_SLACK_TOKEN for the automation plugin")?;
    let api = setting("DOC_AUTOMATION_SLACK_API").unwrap_or_else(|| "https://slack.com/api".into());
    let answer = client()?
        .post(format!("{}/chat.postMessage", api.trim_end_matches('/')))
        .bearer_auth(token)
        .json(&json!({ "channel": channel, "text": text }))
        .send()
        .await;
    if answer.is_err() {
        telemetry::sent("slack", "post-message", &answer);
    }
    let answer = answer.map_err(|err| format!("Slack could not be reached: {err}"))?;
    let status = answer.status();
    let body: Value = answer.json().await.unwrap_or_default();
    // Slack answers 200 to a message it refuses, and says so in the body.
    telemetry::external("slack", "post-message", Some(status.as_u16()), body["ok"] == true);
    match body["ok"].as_bool() {
        Some(true) => Ok(json!({ "channel": body["channel"], "ts": body["ts"] })),
        _ => Err(format!(
            "Slack refused the message to {channel}: {}",
            body["error"].as_str().unwrap_or("no reason given")
        )),
    }
}

async fn email(to: &str, subject: &str, body: &str) -> Result<Value, String> {
    use lettre::message::header::ContentType;
    use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
    let url = setting("DOC_AUTOMATION_SMTP_URL")
        .ok_or("email is not set up: set DOC_AUTOMATION_SMTP_URL for the automation plugin")?;
    let from = setting("DOC_AUTOMATION_SMTP_FROM")
        .ok_or("email is not set up: set DOC_AUTOMATION_SMTP_FROM for the automation plugin")?;
    let message = Message::builder()
        .from(
            from.parse()
                .map_err(|err| format!("DOC_AUTOMATION_SMTP_FROM is not an address: {err}"))?,
        )
        .to(to.parse().map_err(|err| format!("`{to}` is not an address: {err}"))?)
        .subject(subject)
        .header(ContentType::TEXT_PLAIN)
        .body(body.to_string())
        .map_err(|err| format!("the email could not be made: {err}"))?;
    let transport = AsyncSmtpTransport::<Tokio1Executor>::from_url(&url)
        .map_err(|err| format!("DOC_AUTOMATION_SMTP_URL is not an SMTP URL: {err}"))?
        .timeout(Some(Duration::from_secs(30)))
        .build();
    let sent = transport.send(message).await;
    telemetry::external("smtp", "send", None, sent.is_ok());
    let sent = sent.map_err(|err| format!("the email to {to} was not sent: {err}"))?;
    Ok(json!({ "to": to, "code": sent.code().to_string() }))
}
