//! Linode's personal access tokens, made by an account token allowed to create them, each
//! limited to some scopes and ended early by its ID.

use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use doc_plugin_sdk::protocol::Secret;
use serde_json::{Value, json};

use super::{Issued, Vendor, client, refused, strings};

const API: &str = "https://api.linode.com/v4";

fn api(config: &Value) -> String {
    config["api"].as_str().filter(|api| !api.is_empty()).unwrap_or(API).to_string()
}

pub async fn test(config: &Value, credential: &Secret<String>) -> Result<String, String> {
    let answer = client()?
        .get(format!("{}/profile", api(config)))
        .bearer_auth(credential.expose())
        .send()
        .await
        .map_err(|err| format!("Linode could not be reached: {err}"))?;
    let status = answer.status().as_u16();
    let body = answer.text().await.unwrap_or_default();
    match status {
        200 => {
            let profile: Value = serde_json::from_str(&body).unwrap_or_default();
            let user = profile["username"].as_str().unwrap_or("an account");
            Ok(format!("Linode took the token, which belongs to {user}."))
        }
        _ => Err(refused(Vendor::Linode, status, &body)),
    }
}

pub async fn issue(
    config: &Value,
    credential: &Secret<String>,
    restrictions: &Value,
    minutes: i64,
    purpose: &str,
) -> Result<Issued, String> {
    let expires_at = Utc::now() + Duration::minutes(minutes);
    let body = json!({
        "label": purpose.chars().take(100).collect::<String>(),
        "scopes": strings(&restrictions["scopes"]).join(" "),
        "expiry": expires_at.format("%Y-%m-%dT%H:%M:%S").to_string(),
    });
    let answer = client()?
        .post(format!("{}/profile/tokens", api(config)))
        .bearer_auth(credential.expose())
        .json(&body)
        .send()
        .await
        .map_err(|err| format!("Linode could not be reached: {err}"))?;
    let status = answer.status().as_u16();
    let text = answer.text().await.unwrap_or_default();
    if !(200..300).contains(&status) {
        return Err(refused(Vendor::Linode, status, &text));
    }
    let made: Value =
        serde_json::from_str(&text).map_err(|_| "Linode's answer was not JSON".to_string())?;
    let token = made["token"].as_str().ok_or("Linode gave no token")?;
    let ends = made["expiry"]
        .as_str()
        .and_then(|at| NaiveDateTime::parse_from_str(at, "%Y-%m-%dT%H:%M:%S").ok())
        .map(|at| DateTime::<Utc>::from_naive_utc_and_offset(at, Utc))
        .unwrap_or(expires_at);
    Ok(Issued {
        value: Secret::new(token.to_string()),
        vendor_id: made["id"].as_i64().map(|id| id.to_string()),
        expires_at: ends,
    })
}

pub async fn revoke(config: &Value, credential: &Secret<String>, id: &str) -> Result<(), String> {
    let answer = client()?
        .delete(format!("{}/profile/tokens/{id}", api(config)))
        .bearer_auth(credential.expose())
        .send()
        .await
        .map_err(|err| format!("Linode could not be reached: {err}"))?;
    match answer.status().as_u16() {
        200..300 | 404 => Ok(()),
        status => Err(refused(Vendor::Linode, status, &answer.text().await.unwrap_or_default())),
    }
}
