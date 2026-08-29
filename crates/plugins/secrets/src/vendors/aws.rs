//! AWS STS: temporary keys for a role the account may assume, narrowed by managed policies, through
//! the Query API signed with Signature Version 4 as Infra signs EC2's.

use chrono::{DateTime, Utc};
use doc_plugin_sdk::protocol::Secret;
use ring::{digest, hmac};
use serde_json::Value;

use super::{Issued, client, configured, strings};

const VERSION: &str = "2011-06-15";
const SERVICE: &str = "sts";
const SIGNED: &str = "content-type;host;x-amz-date";
const FORM: &str = "application/x-www-form-urlencoded";

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(digest::digest(&digest::SHA256, bytes))
}

fn sign(key: &[u8], message: &str) -> hmac::Tag {
    hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), message.as_bytes())
}

/// The text of the first `<name>…</name>` in `xml`; every element read here holds a scalar.
fn tag<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let from = xml.find(&open)? + open.len();
    let to = xml[from..].find(&close)? + from;
    Some(xml[from..to].trim())
}

fn problem(status: u16, body: &str) -> String {
    match (tag(body, "Code"), tag(body, "Message")) {
        (Some(code), Some(message)) => format!("AWS answered {status}: {code}: {message}"),
        _ => format!("AWS answered {status}: {}", body.chars().take(300).collect::<String>()),
    }
}

/// One Query API call to STS, signed, answering its XML.
async fn call(
    config: &Value,
    secret: &Secret<String>,
    form: &[(&str, String)],
) -> Result<String, String> {
    let region = configured(config, "region")?;
    let key = configured(config, "access_key_id")?;
    let url = match config["endpoint"].as_str().filter(|endpoint| !endpoint.is_empty()) {
        Some(endpoint) => endpoint.trim_end_matches('/').to_string(),
        None => format!("https://sts.{region}.amazonaws.com"),
    };
    let body = {
        let mut writing = url::form_urlencoded::Serializer::new(String::new());
        writing.append_pair("Version", VERSION);
        for (name, value) in form {
            writing.append_pair(name, value);
        }
        writing.finish()
    };
    let parsed =
        url::Url::parse(&url).map_err(|err| format!("{url} is not an STS endpoint: {err}"))?;
    let host = match parsed.port() {
        Some(port) => format!("{}:{port}", parsed.host_str().unwrap_or_default()),
        None => parsed.host_str().unwrap_or_default().to_string(),
    };
    let now = Utc::now();
    let moment = now.format("%Y%m%dT%H%M%SZ").to_string();
    let day = now.format("%Y%m%d").to_string();
    let path = match parsed.path() {
        "" => "/",
        path => path,
    };
    let canonical = format!(
        "POST\n{path}\n\ncontent-type:{FORM}\nhost:{host}\nx-amz-date:{moment}\n\n{SIGNED}\n{}",
        sha256_hex(body.as_bytes())
    );
    let scope = format!("{day}/{region}/{SERVICE}/aws4_request");
    let to_sign =
        format!("AWS4-HMAC-SHA256\n{moment}\n{scope}\n{}", sha256_hex(canonical.as_bytes()));
    let day_key = sign(format!("AWS4{}", secret.expose()).as_bytes(), &day);
    let region_key = sign(day_key.as_ref(), &region);
    let service_key = sign(region_key.as_ref(), SERVICE);
    let signing_key = sign(service_key.as_ref(), "aws4_request");
    let signature = hex::encode(sign(signing_key.as_ref(), &to_sign).as_ref());
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={key}/{scope}, SignedHeaders={SIGNED}, Signature={signature}"
    );
    let answer = client()?
        .post(&url)
        .header("content-type", FORM)
        .header("x-amz-date", &moment)
        .header("authorization", authorization)
        .body(body)
        .send()
        .await
        .map_err(|err| format!("AWS could not be reached: {err}"))?;
    let status = answer.status().as_u16();
    let text = answer.text().await.unwrap_or_default();
    match (200..300).contains(&status) {
        true => Ok(text),
        false => Err(problem(status, &text)),
    }
}

pub async fn test(config: &Value, secret: &Secret<String>) -> Result<String, String> {
    let answer = call(config, secret, &[("Action", "GetCallerIdentity".into())]).await?;
    let arn = tag(&answer, "Arn").unwrap_or("an identity");
    Ok(format!("AWS took the key, which is {arn}."))
}

/// A role session's name: what AWS allows, from who it is for.
fn session(subject: &str) -> String {
    let named: String = subject
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "+=,.@-_".contains(c) { c } else { '-' })
        .take(64)
        .collect();
    match named.len() {
        0 | 1 => format!("doc-{named}"),
        _ => named,
    }
}

pub async fn issue(
    config: &Value,
    secret: &Secret<String>,
    restrictions: &Value,
    minutes: i64,
    subject: &str,
) -> Result<Issued, String> {
    let role = restrictions["role"].as_str().ok_or("choose a role")?.to_string();
    let mut form = vec![
        ("Action", "AssumeRole".to_string()),
        ("RoleArn", role),
        ("RoleSessionName", session(subject)),
        ("DurationSeconds", (minutes.clamp(15, 720) * 60).to_string()),
    ];
    if let Some(external) = config["external_id"].as_str().filter(|external| !external.is_empty()) {
        form.push(("ExternalId", external.to_string()));
    }
    let named: Vec<(String, String)> = strings(&restrictions["policies"])
        .into_iter()
        .enumerate()
        .map(|(at, policy)| (format!("PolicyArns.member.{}.arn", at + 1), policy))
        .collect();
    form.extend(named.iter().map(|(name, value)| (name.as_str(), value.clone())));
    let answer = call(config, secret, &form).await?;
    let (Some(key), Some(secret), Some(session)) = (
        tag(&answer, "AccessKeyId"),
        tag(&answer, "SecretAccessKey"),
        tag(&answer, "SessionToken"),
    ) else {
        return Err("AWS gave no credentials".into());
    };
    let expires_at = tag(&answer, "Expiration")
        .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
        .map(|at| at.with_timezone(&Utc))
        .unwrap_or_else(|| Utc::now() + chrono::Duration::minutes(minutes));
    let value = format!(
        "AWS_ACCESS_KEY_ID={key}\nAWS_SECRET_ACCESS_KEY={secret}\nAWS_SESSION_TOKEN={session}"
    );
    Ok(Issued { value: Secret::new(value), vendor_id: None, expires_at })
}
