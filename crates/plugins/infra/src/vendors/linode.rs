//! Linode, through API v4 with a personal access token: Object Storage buckets and Linode
//! instances. Linode's buckets take no labels, so each one's name carries its DOC request instead;
//! an instance takes DOC's labels as tags. `DOC_INFRA_LINODE_API` points it at a sandbox.

use std::net::Ipv4Addr;

use async_trait::async_trait;
use doc_plugin_sdk::protocol::Secret;
use serde_json::{Value, json};

use super::{
    Adapter, Made, Wanted, client, credential, offered, refused, setting, sized, unknown_password,
};
use crate::catalog::VM;

/// What a machine runs when the settings do not say.
const IMAGE: &str = "linode/debian12";

pub struct Linode {
    token: Secret<String>,
    api: String,
    http: reqwest::Client,
}

/// A Linode tag: 3 to 50 characters, and nothing exotic in them.
fn tag(key: &str, value: &str) -> String {
    let kept = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':');
    format!("{key}:{value}").chars().map(|c| if kept(c) { c } else { '-' }).take(50).collect()
}

/// The first address that is not one only the private network can reach.
fn public(addresses: &Value) -> Option<String> {
    addresses.as_array()?.iter().filter_map(Value::as_str).find_map(|address| {
        match address.parse::<Ipv4Addr>() {
            Ok(v4) if v4.is_private() || v4.is_link_local() => None,
            Ok(_) => Some(address.to_string()),
            Err(_) => None,
        }
    })
}

impl Linode {
    pub fn from_settings() -> Result<Self, String> {
        let api =
            setting("DOC_INFRA_LINODE_API").unwrap_or_else(|| "https://api.linode.com".into());
        Ok(Self {
            token: credential("DOC_INFRA_LINODE_TOKEN", "Linode")?,
            api: api.trim_end_matches('/').to_string(),
            http: client()?,
        })
    }

    fn made(bucket: &Value) -> Made {
        Made {
            id: bucket["label"].as_str().unwrap_or_default().to_string(),
            detail: json!({
                "bucket": bucket["label"],
                "region": bucket["region"],
                "hostname": bucket["hostname"],
                "created": bucket["created"],
            }),
            ready: true,
        }
    }

    /// An instance as DOC shows it. Its ID is a number, which is what every later call names it
    /// by, so it is kept as the resource's ID rather than the label.
    fn instance(node: &Value) -> Made {
        let status = node["status"].as_str().unwrap_or("provisioning");
        let address = public(&node["ipv4"]);
        Made {
            id: node["id"].as_i64().map(|id| id.to_string()).unwrap_or_default(),
            detail: json!({
                "instance": node["label"],
                "region": node["region"],
                "plan": node["type"],
                "image": node["image"],
                "state": status,
                "address": address,
                "ipv6": node["ipv6"],
                "tags": node["tags"],
            }),
            // Running is the first moment it answers for itself, and so the first moment a name
            // pointing at it is worth having.
            ready: status == "running",
        }
    }

    async fn send(&self, call: reqwest::RequestBuilder) -> Result<reqwest::Response, String> {
        call.bearer_auth(self.token.expose())
            .send()
            .await
            .map_err(|err| format!("Linode could not be reached: {err}"))
    }

    async fn create_instance(&self, wanted: &Wanted) -> Result<Made, String> {
        let image = setting("DOC_INFRA_LINODE_IMAGE").unwrap_or_else(|| IMAGE.into());
        let tags: Vec<String> = wanted.labels.iter().map(|(key, value)| tag(key, value)).collect();
        let mut body = json!({
            "region": wanted.region,
            "type": sized("Linode", wanted)?,
            "label": wanted.name,
            "image": image,
            "root_pass": unknown_password(),
            "tags": tags,
            "booted": true,
        });
        if let Some(key) = setting("DOC_INFRA_LINODE_SSH_KEY") {
            body["authorized_keys"] = json!([key.trim()]);
        }
        let url = format!("{}/v4/linode/instances", self.api);
        let answer = self.send(self.http.post(url).json(&body)).await?;
        if !answer.status().is_success() {
            return Err(refused("Linode", answer).await);
        }
        let node: Value = answer.json().await.map_err(|err| format!("Linode's answer: {err}"))?;
        Ok(Self::instance(&node))
    }
}

#[async_trait]
impl Adapter for Linode {
    async fn create(&self, wanted: &Wanted) -> Result<Made, String> {
        offered("Linode", wanted, &["bucket", VM])?;
        if wanted.kind == VM {
            return self.create_instance(wanted).await;
        }
        let url = format!("{}/v4/object-storage/buckets", self.api);
        let body = json!({ "region": wanted.region, "label": wanted.name });
        let answer = self.send(self.http.post(url).json(&body)).await?;
        if !answer.status().is_success() {
            return Err(refused("Linode", answer).await);
        }
        let bucket: Value = answer.json().await.map_err(|err| format!("Linode's answer: {err}"))?;
        Ok(Self::made(&bucket))
    }

    async fn inspect(&self, wanted: &Wanted, id: &str) -> Result<Option<Made>, String> {
        let url = match wanted.kind == VM {
            true => format!("{}/v4/linode/instances/{id}", self.api),
            false => format!("{}/v4/object-storage/buckets/{}/{id}", self.api, wanted.region),
        };
        let answer = self.send(self.http.get(url)).await?;
        match answer.status().as_u16() {
            404 => Ok(None),
            200 => {
                let body: Value =
                    answer.json().await.map_err(|err| format!("Linode's answer: {err}"))?;
                match wanted.kind == VM {
                    true => Ok(Some(Self::instance(&body))),
                    false => Ok(Some(Self::made(&body))),
                }
            }
            _ => Err(refused("Linode", answer).await),
        }
    }

    async fn delete(&self, wanted: &Wanted, id: &str) -> Result<(), String> {
        let url = match wanted.kind == VM {
            true => format!("{}/v4/linode/instances/{id}", self.api),
            false => format!("{}/v4/object-storage/buckets/{}/{id}", self.api, wanted.region),
        };
        let answer = self.send(self.http.delete(url)).await?;
        match answer.status().as_u16() {
            200 | 204 | 404 => Ok(()),
            _ => Err(refused("Linode", answer).await),
        }
    }
}
