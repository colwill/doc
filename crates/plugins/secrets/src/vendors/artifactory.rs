//! JFrog Artifactory's access tokens, made by an account token allowed to create them, each
//! limited to some groups and ended early by its ID.

use chrono::{Duration, Utc};
use doc_plugin_sdk::protocol::Secret;
use serde_json::{Value, json};

use super::{Issued, Vendor, client, configured, refused, strings};

pub async fn test(config: &Value, credential: &Secret<String>) -> Result<String, String> {
    let url = configured(config, "url")?;
    let answer = client()?
        .get(format!("{url}/access/api/v1/tokens"))
        .bearer_auth(credential.expose())
        .send()
        .await
        .map_err(|err| format!("Artifactory could not be reached: {err}"))?;
    let status = answer.status().as_u16();
    let body = answer.text().await.unwrap_or_default();
    match status {
        200 => {
            let held: Value = serde_json::from_str(&body).unwrap_or_default();
            let count = held["tokens"].as_array().map_or(0, Vec::len);
            Ok(format!("Artifactory took the token, which can see {count} tokens there."))
        }
        401 | 403 => Err(format!(
            "{}. The token must be allowed to create tokens.",
            refused(Vendor::Artifactory, status, &body)
        )),
        _ => Err(refused(Vendor::Artifactory, status, &body)),
    }
}

pub async fn issue(
    config: &Value,
    credential: &Secret<String>,
    restrictions: &Value,
    minutes: i64,
    subject: &str,
    purpose: &str,
) -> Result<Issued, String> {
    let url = configured(config, "url")?;
    let scope =
        format!("applied-permissions/groups:{}", strings(&restrictions["groups"]).join(","));
    let body = json!({
        "username": subject,
        "scope": scope,
        "expires_in": minutes * 60,
        "refreshable": false,
        "description": purpose.chars().take(200).collect::<String>(),
    });
    let answer = client()?
        .post(format!("{url}/access/api/v1/tokens"))
        .bearer_auth(credential.expose())
        .json(&body)
        .send()
        .await
        .map_err(|err| format!("Artifactory could not be reached: {err}"))?;
    let status = answer.status().as_u16();
    let text = answer.text().await.unwrap_or_default();
    if !(200..300).contains(&status) {
        return Err(refused(Vendor::Artifactory, status, &text));
    }
    let made: Value =
        serde_json::from_str(&text).map_err(|_| "Artifactory's answer was not JSON".to_string())?;
    let token = made["access_token"].as_str().ok_or("Artifactory gave no token")?;
    let lasts = made["expires_in"].as_i64().unwrap_or(minutes * 60);
    Ok(Issued {
        value: Secret::new(token.to_string()),
        vendor_id: made["token_id"].as_str().map(str::to_string),
        expires_at: Utc::now() + Duration::seconds(lasts),
    })
}

pub async fn revoke(config: &Value, credential: &Secret<String>, id: &str) -> Result<(), String> {
    let url = configured(config, "url")?;
    let answer = client()?
        .delete(format!("{url}/access/api/v1/tokens/{id}"))
        .bearer_auth(credential.expose())
        .send()
        .await
        .map_err(|err| format!("Artifactory could not be reached: {err}"))?;
    match answer.status().as_u16() {
        200..300 | 404 => Ok(()),
        status => {
            Err(refused(Vendor::Artifactory, status, &answer.text().await.unwrap_or_default()))
        }
    }
}
