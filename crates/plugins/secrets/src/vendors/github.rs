//! A GitHub App's installation tokens: the app signs a JWT with its private key and exchanges it
//! for a token limited to some repositories and permissions, which GitHub ends after an hour.

use base64::Engine;
use chrono::{DateTime, Utc};
use doc_plugin_sdk::protocol::Secret;
use ring::{rand, signature};
use serde_json::{Value, json};

use super::{Issued, Vendor, client, configured, refused, strings};

const VERSION: &str = "2022-11-28";

fn encoded(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// The key GitHub gave, in either form it gives one: PKCS#1 or PKCS#8, as PEM.
fn key(pem: &str) -> Result<signature::RsaKeyPair, String> {
    let body: String = pem
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("-----"))
        .collect();
    let der = base64::engine::general_purpose::STANDARD
        .decode(body.as_bytes())
        .map_err(|_| "the private key is not PEM".to_string())?;
    let parsed = match pem.contains("BEGIN RSA PRIVATE KEY") {
        true => signature::RsaKeyPair::from_der(&der),
        false => signature::RsaKeyPair::from_pkcs8(&der),
    };
    parsed.map_err(|err| format!("the private key is not an RSA key GitHub would give: {err}"))
}

/// A JWT naming the app, good for nine minutes, signed with its key (RS256).
fn jwt(app_id: &str, pem: &Secret<String>) -> Result<String, String> {
    let pair = key(pem.expose())?;
    let now = Utc::now().timestamp();
    let header = encoded(json!({ "alg": "RS256", "typ": "JWT" }).to_string().as_bytes());
    let claims =
        encoded(json!({ "iat": now - 60, "exp": now + 540, "iss": app_id }).to_string().as_bytes());
    let signed = format!("{header}.{claims}");
    let mut sealed = vec![0; pair.public().modulus_len()];
    pair.sign(
        &signature::RSA_PKCS1_SHA256,
        &rand::SystemRandom::new(),
        signed.as_bytes(),
        &mut sealed,
    )
    .map_err(|_| "the JWT could not be signed".to_string())?;
    Ok(format!("{signed}.{}", encoded(&sealed)))
}

fn asked(request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    request.header("accept", "application/vnd.github+json").header("x-github-api-version", VERSION)
}

pub async fn test(config: &Value, credential: &Secret<String>) -> Result<String, String> {
    let api = configured(config, "api")?;
    let installation = configured(config, "installation_id")?;
    let token = jwt(&configured(config, "app_id")?, credential)?;
    let answer = asked(client()?.get(format!("{api}/app/installations/{installation}")))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|err| format!("GitHub could not be reached: {err}"))?;
    let status = answer.status().as_u16();
    let body = answer.text().await.unwrap_or_default();
    match status {
        200 => {
            let installed: Value = serde_json::from_str(&body).unwrap_or_default();
            let on = installed["account"]["login"].as_str().unwrap_or("an account");
            Ok(format!("GitHub took the key: the app is installed on {on}."))
        }
        _ => Err(refused(Vendor::GitHubApp, status, &body)),
    }
}

pub async fn issue(
    config: &Value,
    credential: &Secret<String>,
    restrictions: &Value,
) -> Result<Issued, String> {
    let api = configured(config, "api")?;
    let installation = configured(config, "installation_id")?;
    let token = jwt(&configured(config, "app_id")?, credential)?;
    // GitHub takes a repository by its name alone, the installation's owner being implied.
    let repositories: Vec<String> = strings(&restrictions["repositories"])
        .into_iter()
        .map(|repository| repository.rsplit('/').next().unwrap_or_default().to_string())
        .collect();
    let mut body = json!({ "permissions": restrictions["permissions"] });
    if !repositories.is_empty() {
        body["repositories"] = json!(repositories);
    }
    let answer =
        asked(client()?.post(format!("{api}/app/installations/{installation}/access_tokens")))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .map_err(|err| format!("GitHub could not be reached: {err}"))?;
    let status = answer.status().as_u16();
    let text = answer.text().await.unwrap_or_default();
    if !(200..300).contains(&status) {
        return Err(refused(Vendor::GitHubApp, status, &text));
    }
    let made: Value =
        serde_json::from_str(&text).map_err(|_| "GitHub's answer was not JSON".to_string())?;
    let value = made["token"].as_str().ok_or("GitHub gave no token")?;
    let expires_at = made["expires_at"]
        .as_str()
        .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
        .map(|at| at.with_timezone(&Utc))
        .unwrap_or_else(|| Utc::now() + chrono::Duration::hours(1));
    Ok(Issued { value: Secret::new(value.to_string()), vendor_id: None, expires_at })
}

/// Ends an installation token early, which takes the token itself.
pub async fn revoke(config: &Value, token: &Secret<String>) -> Result<(), String> {
    let api = configured(config, "api")?;
    let answer = asked(client()?.delete(format!("{api}/installation/token")))
        .header("authorization", format!("token {}", token.expose()))
        .send()
        .await
        .map_err(|err| format!("GitHub could not be reached: {err}"))?;
    match answer.status().as_u16() {
        200..300 | 401 => Ok(()),
        status => Err(refused(Vendor::GitHubApp, status, &answer.text().await.unwrap_or_default())),
    }
}
