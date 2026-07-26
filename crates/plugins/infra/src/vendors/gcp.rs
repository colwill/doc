//! GCP, as a service account: Cloud Storage buckets with a storage class, and Compute Engine
//! instances in a zone, both labelled with their DOC request, team and expiry. The service account
//! earns a token for only the API it is about to call. `DOC_INFRA_GCP_API` and
//! `DOC_INFRA_GCP_COMPUTE_API` point each at a sandbox.

use async_trait::async_trait;
use base64::Engine;
use doc_plugin_sdk::protocol::Secret;
use serde_json::{Value, json};
use url::form_urlencoded::byte_serialize;

use super::{Adapter, Made, Wanted, client, needed, offered, refused, setting, sized};
use crate::catalog::VM;

/// Reading and changing buckets and objects, and nothing else: no IAM, no other service.
const STORAGE_SCOPE: &str = "https://www.googleapis.com/auth/devstorage.read_write";
/// Making and ending instances, asked for only when one is; a deployment that never stands up a
/// machine never mints a token that could.
const COMPUTE_SCOPE: &str = "https://www.googleapis.com/auth/compute";

/// What a machine runs when the settings do not say.
const IMAGE: &str = "projects/debian-cloud/global/images/family/debian-12";
/// The network a machine joins when the settings do not say.
const NETWORK: &str = "global/networks/default";

pub struct Gcp {
    key: Secret<Value>,
    project: String,
    api: String,
    compute: String,
    http: reqwest::Client,
}

fn url_safe(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn encoded(text: &str) -> String {
    byte_serialize(text.as_bytes()).collect::<String>().replace('+', "%20")
}

/// A label as Cloud Storage takes it: lowercase letters, digits, `_` and `-`, up to 63 of them.
fn label(text: &str) -> String {
    text.to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '-' })
        .take(63)
        .collect()
}

impl Gcp {
    pub fn from_settings() -> Result<Self, String> {
        let key: Value = serde_json::from_str(&needed("DOC_INFRA_GCP_KEY", "GCP")?)
            .map_err(|err| format!("DOC_INFRA_GCP_KEY is not a service account key: {err}"))?;
        let project = setting("DOC_INFRA_GCP_PROJECT")
            .or_else(|| key["project_id"].as_str().map(str::to_string))
            .ok_or("GCP has no DOC_INFRA_GCP_PROJECT in the plugin's secrets")?;
        let api =
            setting("DOC_INFRA_GCP_API").unwrap_or_else(|| "https://storage.googleapis.com".into());
        let compute = setting("DOC_INFRA_GCP_COMPUTE_API")
            .unwrap_or_else(|| "https://compute.googleapis.com".into());
        let key = Secret::new(key);
        Ok(Self {
            key,
            project,
            api: api.trim_end_matches('/').to_string(),
            compute: compute.trim_end_matches('/').to_string(),
            http: client()?,
        })
    }

    /// The access token the service account's key earns for one scope, by signing its own
    /// assertion.
    async fn token(&self, scope: &str) -> Result<Secret<String>, String> {
        use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
        let key = self.key.expose();
        let email = key["client_email"].as_str().ok_or("the GCP key names no client_email")?;
        let token_uri = key["token_uri"].as_str().unwrap_or("https://oauth2.googleapis.com/token");
        let pem = key["private_key"].as_str().ok_or("the GCP key has no private_key")?;
        let body: String = pem.lines().filter(|line| !line.starts_with("-----")).collect();
        let der = base64::engine::general_purpose::STANDARD
            .decode(body.trim())
            .map_err(|err| format!("the GCP private key is not PEM: {err}"))?;
        let pair = RsaKeyPair::from_pkcs8(&der)
            .map_err(|err| format!("the GCP private key is not an RSA key: {err}"))?;
        let now = chrono::Utc::now().timestamp();
        let claims = json!({ "iss": email, "scope": scope, "aud": token_uri, "iat": now, "exp": now + 3600 });
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
        .map_err(|_| "the GCP assertion could not be signed".to_string())?;
        let assertion = format!("{signed}.{}", url_safe(&signature));
        let form = [
            ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
            ("assertion", assertion.as_str()),
        ];
        let answer = self
            .http
            .post(token_uri)
            .form(&form)
            .send()
            .await
            .map_err(|err| format!("Google could not be reached: {err}"))?;
        if !answer.status().is_success() {
            return Err(refused("Google's token service", answer).await);
        }
        let body: Value =
            answer.json().await.map_err(|err| format!("Google's token answer: {err}"))?;
        body["access_token"]
            .as_str()
            .map(|token| Secret::new(token.to_string()))
            .ok_or_else(|| "Google gave no token".into())
    }

    fn made(bucket: &Value) -> Made {
        Made {
            id: bucket["name"].as_str().unwrap_or_default().to_string(),
            detail: json!({
                "bucket": bucket["name"],
                "location": bucket["location"],
                "storage_class": bucket["storageClass"],
                "labels": bucket["labels"],
                "link": bucket["selfLink"],
            }),
            ready: true,
        }
    }

    fn instances(&self, zone: &str) -> String {
        format!(
            "{}/compute/v1/projects/{}/zones/{}/instances",
            self.compute,
            encoded(&self.project),
            encoded(zone)
        )
    }

    /// An instance as DOC shows it. Compute Engine names a machine rather than numbering it for
    /// later calls, so the name it was made under is its ID here.
    fn instance(node: &Value, zone: &str) -> Made {
        let state = node["status"].as_str().unwrap_or("PROVISIONING");
        let address = node["networkInterfaces"][0]["accessConfigs"][0]["natIP"].as_str();
        Made {
            id: node["name"].as_str().unwrap_or_default().to_string(),
            detail: json!({
                "instance": node["name"],
                "zone": zone,
                "machine_type": node["machineType"],
                "state": state,
                "address": address,
                "private_address": node["networkInterfaces"][0]["networkIP"],
                "labels": node["labels"],
                "link": node["selfLink"],
            }),
            // Running with an address is the first moment it answers for itself, and so the
            // first moment a name pointing at it is worth having.
            ready: state == "RUNNING" && address.is_some(),
        }
    }

    async fn create_instance(&self, wanted: &Wanted) -> Result<Made, String> {
        let token = self.token(COMPUTE_SCOPE).await?;
        let zone = &wanted.region;
        let image = setting("DOC_INFRA_GCP_IMAGE").unwrap_or_else(|| IMAGE.into());
        let network = setting("DOC_INFRA_GCP_NETWORK").unwrap_or_else(|| NETWORK.into());
        let labels: serde_json::Map<String, Value> =
            wanted.labels.iter().map(|(key, value)| (label(key), json!(label(value)))).collect();
        let mut instance = json!({
            "name": wanted.name,
            "machineType": format!("zones/{zone}/machineTypes/{}", sized("GCP", wanted)?),
            "disks": [{
                "boot": true,
                "autoDelete": true,
                "initializeParams": { "sourceImage": image },
            }],
            // One external address, so the machine is reachable at the name DOC gives it.
            "networkInterfaces": [{
                "network": network,
                "accessConfigs": [{ "type": "ONE_TO_ONE_NAT", "name": "External NAT" }],
            }],
            "labels": labels,
        });
        if let Some(key) = setting("DOC_INFRA_GCP_SSH_KEY") {
            instance["metadata"] = json!({ "items": [{ "key": "ssh-keys", "value": key.trim() }] });
        }
        let answer = self
            .http
            .post(self.instances(zone))
            .bearer_auth(token.expose())
            .json(&instance)
            .send()
            .await
            .map_err(|err| format!("GCP could not be reached: {err}"))?;
        if !answer.status().is_success() {
            return Err(refused("GCP", answer).await);
        }
        let operation: Value = answer.json().await.map_err(|err| format!("GCP's answer: {err}"))?;
        // Compute Engine answers with the operation, not the machine, so DOC looks again on its
        // next sweep for the address and the moment it started running.
        Ok(Made {
            id: wanted.name.clone(),
            detail: json!({
                "instance": wanted.name,
                "zone": zone,
                "state": "PROVISIONING",
                "operation": operation["name"],
            }),
            ready: false,
        })
    }

    async fn inspect_instance(&self, zone: &str, id: &str) -> Result<Option<Made>, String> {
        let token = self.token(COMPUTE_SCOPE).await?;
        let answer = self
            .http
            .get(format!("{}/{}", self.instances(zone), encoded(id)))
            .bearer_auth(token.expose())
            .send()
            .await
            .map_err(|err| format!("GCP could not be reached: {err}"))?;
        match answer.status().as_u16() {
            404 => Ok(None),
            200 => {
                let node: Value =
                    answer.json().await.map_err(|err| format!("GCP's answer: {err}"))?;
                Ok(Some(Self::instance(&node, zone)))
            }
            _ => Err(refused("GCP", answer).await),
        }
    }

    async fn delete_instance(&self, zone: &str, id: &str) -> Result<(), String> {
        let token = self.token(COMPUTE_SCOPE).await?;
        let answer = self
            .http
            .delete(format!("{}/{}", self.instances(zone), encoded(id)))
            .bearer_auth(token.expose())
            .send()
            .await
            .map_err(|err| format!("GCP could not be reached: {err}"))?;
        match answer.status().as_u16() {
            200 | 204 | 404 => Ok(()),
            _ => Err(refused("GCP", answer).await),
        }
    }
}

#[async_trait]
impl Adapter for Gcp {
    async fn create(&self, wanted: &Wanted) -> Result<Made, String> {
        offered("GCP", wanted, &["bucket", VM])?;
        if wanted.kind == VM {
            return self.create_instance(wanted).await;
        }
        let token = self.token(STORAGE_SCOPE).await?;
        let labels: serde_json::Map<String, Value> =
            wanted.labels.iter().map(|(key, value)| (label(key), json!(label(value)))).collect();
        let bucket = json!({
            "name": wanted.name,
            "location": wanted.region,
            "storageClass": wanted.size.clone().unwrap_or_else(|| "STANDARD".into()),
            "labels": labels,
        });
        let url = format!("{}/storage/v1/b?project={}", self.api, encoded(&self.project));
        let answer = self
            .http
            .post(url)
            .bearer_auth(token.expose())
            .json(&bucket)
            .send()
            .await
            .map_err(|err| format!("GCP could not be reached: {err}"))?;
        if !answer.status().is_success() {
            return Err(refused("GCP", answer).await);
        }
        let made: Value = answer.json().await.map_err(|err| format!("GCP's answer: {err}"))?;
        Ok(Self::made(&made))
    }

    async fn inspect(&self, wanted: &Wanted, id: &str) -> Result<Option<Made>, String> {
        if wanted.kind == VM {
            return self.inspect_instance(&wanted.region, id).await;
        }
        let token = self.token(STORAGE_SCOPE).await?;
        let url = format!("{}/storage/v1/b/{}", self.api, encoded(id));
        let answer = self
            .http
            .get(url)
            .bearer_auth(token.expose())
            .send()
            .await
            .map_err(|err| format!("GCP could not be reached: {err}"))?;
        match answer.status().as_u16() {
            404 => Ok(None),
            200 => {
                let bucket: Value =
                    answer.json().await.map_err(|err| format!("GCP's answer: {err}"))?;
                Ok(Some(Self::made(&bucket)))
            }
            _ => Err(refused("GCP", answer).await),
        }
    }

    async fn delete(&self, wanted: &Wanted, id: &str) -> Result<(), String> {
        if wanted.kind == VM {
            return self.delete_instance(&wanted.region, id).await;
        }
        let token = self.token(STORAGE_SCOPE).await?;
        let bucket = format!("{}/storage/v1/b/{}", self.api, encoded(id));
        let listed = self
            .http
            .get(format!("{bucket}/o"))
            .bearer_auth(token.expose())
            .send()
            .await
            .map_err(|err| format!("GCP could not be reached: {err}"))?;
        if listed.status().as_u16() == 404 {
            return Ok(());
        }
        if !listed.status().is_success() {
            return Err(refused("GCP", listed).await);
        }
        let objects: Value = listed.json().await.map_err(|err| format!("GCP's answer: {err}"))?;
        for object in objects["items"].as_array().into_iter().flatten() {
            let name = object["name"].as_str().unwrap_or_default();
            let gone = self
                .http
                .delete(format!("{bucket}/o/{}", encoded(name)))
                .bearer_auth(token.expose())
                .send()
                .await
                .map_err(|err| format!("GCP could not be reached: {err}"))?;
            if !gone.status().is_success() && gone.status().as_u16() != 404 {
                return Err(refused("GCP", gone).await);
            }
        }
        let answer = self
            .http
            .delete(bucket)
            .bearer_auth(token.expose())
            .send()
            .await
            .map_err(|err| format!("GCP could not be reached: {err}"))?;
        match answer.status().as_u16() {
            200 | 204 | 404 => Ok(()),
            _ => Err(refused("GCP", answer).await),
        }
    }
}
