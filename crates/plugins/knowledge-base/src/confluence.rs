//! Reading a Confluence space: Cloud through REST v2 with an email and API token, Data Center
//! through REST v1 with a personal access token. Pages come back in page-tree order, and bodies,
//! labels and attachments are fetched only for pages whose version changed.

use std::collections::BTreeMap;
use std::time::Duration;

use doc_plugin_sdk::protocol::Secret;
use doc_plugin_sdk::telemetry::sent;
use serde_json::Value;
use url::Url;

const PAGE_LIMIT: usize = 5_000;
const ATTACHMENT_LIMIT: u64 = 10 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavour {
    Cloud,
    DataCenter,
}

impl Flavour {
    pub fn named(name: &str) -> Option<Self> {
        match name {
            "cloud" => Some(Self::Cloud),
            "datacenter" | "data-center" | "server" => Some(Self::DataCenter),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Page {
    pub id: String,
    pub title: String,
    pub version: i64,
    parent: Option<String>,
    position: i64,
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct Attachment {
    pub name: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
}

pub struct Confluence {
    http: reqwest::Client,
    /// Everything ends in `/`, so paths join onto it: `…/wiki/` on Cloud.
    base: Url,
    flavour: Flavour,
    /// `email:token` for Cloud's basic authentication, the token alone for Data Center.
    credential: Secret<String>,
}

fn text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        _ => String::new(),
    }
}

impl Confluence {
    pub fn new(site: &str, flavour: Flavour, credential: Secret<String>) -> Result<Self, String> {
        let site = site.trim_end_matches('/');
        let base = match flavour {
            Flavour::Cloud if !site.ends_with("/wiki") => format!("{site}/wiki/"),
            _ => format!("{site}/"),
        };
        let base = Url::parse(&base).map_err(|err| format!("`{site}` is not a URL: {err}"))?;
        if flavour == Flavour::Cloud && !credential.expose().contains(':') {
            return Err("a Confluence Cloud credential is written email:api-token".into());
        }
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("doc-kb/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|err| err.to_string())?;
        Ok(Self { http, base, flavour, credential })
    }

    fn url(&self, path: &str) -> Result<Url, String> {
        self.base.join(path.trim_start_matches('/')).map_err(|err| err.to_string())
    }

    async fn fetch(&self, url: Url) -> Result<reqwest::Response, String> {
        let request = match (self.flavour, self.credential.expose().split_once(':')) {
            (Flavour::Cloud, Some((email, token))) => {
                self.http.get(url.clone()).basic_auth(email, Some(token))
            }
            _ => self.http.get(url.clone()).bearer_auth(self.credential.expose()),
        };
        let answer = request.send().await;
        sent("confluence", "get", &answer);
        let answer = answer.map_err(|err| format!("Confluence could not be reached: {err}"))?;
        match answer.status().as_u16() {
            200..=299 => Ok(answer),
            401 | 403 => Err(format!("Confluence refused the credential for {}", url.path())),
            status => Err(format!("Confluence answered {status} to {}", url.path())),
        }
    }

    async fn get(&self, path: &str) -> Result<Value, String> {
        let answer = self.fetch(self.url(path)?).await?;
        answer.json().await.map_err(|err| format!("Confluence's answer could not be read: {err}"))
    }

    /// Every page of the space, each with its version, in page-tree order.
    pub async fn pages(&self, space_key: &str) -> Result<Vec<Page>, String> {
        let pages = match self.flavour {
            Flavour::Cloud => self.cloud_pages(space_key).await?,
            Flavour::DataCenter => self.datacenter_pages(space_key).await?,
        };
        Ok(tree_order(pages))
    }

    async fn cloud_pages(&self, space_key: &str) -> Result<Vec<Page>, String> {
        let spaces = self.get(&format!("api/v2/spaces?keys={space_key}")).await?;
        let space = spaces["results"][0]["id"].clone();
        if space.is_null() {
            return Err(format!(
                "Confluence has no space {space_key}, or the credential cannot see it"
            ));
        }
        let mut next = Some(format!("api/v2/spaces/{}/pages?limit=250", text(&space)));
        let mut pages = Vec::new();
        while let Some(path) = next.take() {
            let answer = self.get(&path).await?;
            for page in answer["results"].as_array().into_iter().flatten() {
                pages.push(Page {
                    id: text(&page["id"]),
                    title: text(&page["title"]),
                    version: page["version"]["number"].as_i64().unwrap_or_default(),
                    parent: Some(text(&page["parentId"])).filter(|parent| !parent.is_empty()),
                    position: page["position"].as_i64().unwrap_or_default(),
                    url: text(&page["_links"]["webui"]),
                });
            }
            next = answer["_links"]["next"]
                .as_str()
                .map(|link| link.trim_start_matches("/wiki/").to_string());
            if pages.len() > PAGE_LIMIT {
                return Err(format!("a space of more than {PAGE_LIMIT} pages is not synced"));
            }
        }
        Ok(pages)
    }

    async fn datacenter_pages(&self, space_key: &str) -> Result<Vec<Page>, String> {
        let mut pages = Vec::new();
        let mut start = 0;
        loop {
            let path = format!(
                "rest/api/content?spaceKey={space_key}&type=page&expand=version,ancestors,extensions.position&limit=100&start={start}"
            );
            let answer = self.get(&path).await?;
            let found = answer["results"].as_array().cloned().unwrap_or_default();
            for page in &found {
                pages.push(Page {
                    id: text(&page["id"]),
                    title: text(&page["title"]),
                    version: page["version"]["number"].as_i64().unwrap_or_default(),
                    parent: page["ancestors"]
                        .as_array()
                        .and_then(|above| above.last())
                        .map(|parent| text(&parent["id"])),
                    position: page["extensions"]["position"].as_i64().unwrap_or_default(),
                    url: text(&page["_links"]["webui"]),
                });
            }
            start += found.len();
            if found.is_empty() || answer["_links"]["next"].is_null() {
                break;
            }
            if pages.len() > PAGE_LIMIT {
                return Err(format!("a space of more than {PAGE_LIMIT} pages is not synced"));
            }
        }
        Ok(pages)
    }

    /// A page's storage-format body and its labels.
    pub async fn body(&self, page: &Page) -> Result<(String, Vec<String>), String> {
        match self.flavour {
            Flavour::Cloud => {
                let answer =
                    self.get(&format!("api/v2/pages/{}?body-format=storage", page.id)).await?;
                let labels = self.get(&format!("api/v2/pages/{}/labels", page.id)).await?;
                let names = labels["results"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|label| text(&label["name"]))
                    .collect();
                Ok((text(&answer["body"]["storage"]["value"]), names))
            }
            Flavour::DataCenter => {
                let answer = self
                    .get(&format!(
                        "rest/api/content/{}?expand=body.storage,metadata.labels",
                        page.id
                    ))
                    .await?;
                let names = answer["metadata"]["labels"]["results"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|label| text(&label["name"]))
                    .collect();
                Ok((text(&answer["body"]["storage"]["value"]), names))
            }
        }
    }

    /// A page's attachments, each fetched in full; any over the limit is left out.
    pub async fn attachments(&self, page: &Page) -> Result<Vec<Attachment>, String> {
        let links: Vec<(String, String, String)> = match self.flavour {
            Flavour::Cloud => {
                let listed = self.get(&format!("api/v2/pages/{}/attachments", page.id)).await?;
                listed["results"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|found| {
                        (
                            text(&found["title"]),
                            text(&found["mediaType"]),
                            text(&found["downloadLink"]),
                        )
                    })
                    .collect()
            }
            Flavour::DataCenter => {
                let listed =
                    self.get(&format!("rest/api/content/{}/child/attachment", page.id)).await?;
                listed["results"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|found| {
                        let kind = text(&found["metadata"]["mediaType"]);
                        (text(&found["title"]), kind, text(&found["_links"]["download"]))
                    })
                    .collect()
            }
        };
        let mut attachments = Vec::new();
        for (name, content_type, link) in links {
            if name.is_empty() || link.is_empty() {
                continue;
            }
            let answer = self.fetch(self.url(&link)?).await?;
            if answer.content_length().is_some_and(|length| length > ATTACHMENT_LIMIT) {
                continue;
            }
            let bytes = answer
                .bytes()
                .await
                .map_err(|err| format!("an attachment could not be read: {err}"))?;
            let content_type = if content_type.is_empty() {
                "application/octet-stream".into()
            } else {
                content_type
            };
            attachments.push(Attachment { name, content_type, bytes: bytes.to_vec() });
        }
        Ok(attachments)
    }

    /// Where a page is on the site, from the path Confluence gives for it.
    pub fn page_url(&self, page: &Page) -> String {
        self.url(&page.url).map(|url| url.to_string()).unwrap_or_default()
    }
}

/// Depth first from the top, each level by its position and then its title.
fn tree_order(pages: Vec<Page>) -> Vec<Page> {
    let ids: std::collections::BTreeSet<String> =
        pages.iter().map(|page| page.id.clone()).collect();
    let mut children: BTreeMap<Option<String>, Vec<Page>> = BTreeMap::new();
    for page in pages {
        let parent = page.parent.clone().filter(|parent| ids.contains(parent));
        children.entry(parent).or_default().push(page);
    }
    for level in children.values_mut() {
        level.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| a.title.cmp(&b.title)));
    }
    let mut ordered = Vec::new();
    let mut stack: Vec<Page> =
        children.remove(&None).unwrap_or_default().into_iter().rev().collect();
    while let Some(page) = stack.pop() {
        if let Some(below) = children.remove(&Some(page.id.clone())) {
            stack.extend(below.into_iter().rev());
        }
        ordered.push(page);
    }
    ordered
}
