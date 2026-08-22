//! Talking to a cluster's API server with the service account a kubeconfig names: the server, its
//! certificate authority and a bearer token. Only reads are ever made; the settings check refuses a
//! service account that could change anything. Client certificates and `exec` plugins are not
//! supported, since a service account's token is all a read-only connection needs.

use std::collections::BTreeMap;
use std::time::Duration;

use base64::Engine;
use chrono::{DateTime, Utc};
use doc_plugin_sdk::telemetry::sent;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;

const TIMEOUT: Duration = Duration::from_secs(20);
/// Items asked for in one page of a list.
const PAGE: usize = 500;
/// Pages read of one list at most, so a vast cluster cannot keep a read going for ever.
const MAX_PAGES: usize = 40;

/// Where a cluster is and how to prove who is asking, from a kubeconfig.
pub struct Connection {
    pub server: Url,
    ca: Option<Vec<u8>>,
    insecure: bool,
    token: String,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct Kubeconfig {
    clusters: Vec<Named<ClusterEntry>>,
    users: Vec<Named<UserEntry>>,
    contexts: Vec<Named<ContextEntry>>,
    current_context: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Named<T: Default> {
    name: String,
    #[serde(alias = "cluster", alias = "user", alias = "context")]
    value: T,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct ClusterEntry {
    server: String,
    certificate_authority_data: Option<String>,
    certificate_authority: Option<String>,
    insecure_skip_tls_verify: bool,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct UserEntry {
    token: Option<String>,
    client_certificate_data: Option<String>,
    exec: Option<Value>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct ContextEntry {
    cluster: String,
    user: String,
}

impl Connection {
    /// The connection a kubeconfig describes: its current context, or its only one.
    pub fn parse(text: &str) -> Result<Self, String> {
        let config: Kubeconfig = serde_yaml_ng::from_str(text)
            .map_err(|err| format!("it is not a kubeconfig: {err}"))?;
        let context = match &config.current_context {
            Some(current) => config.contexts.iter().find(|context| &context.name == current),
            None if config.contexts.len() == 1 => config.contexts.first(),
            None => None,
        };
        let (cluster, user) = match context {
            Some(context) => (
                config.clusters.iter().find(|cluster| cluster.name == context.value.cluster),
                config.users.iter().find(|user| user.name == context.value.user),
            ),
            None if config.clusters.len() == 1 && config.users.len() == 1 => {
                (config.clusters.first(), config.users.first())
            }
            None if config.contexts.len() > 1 => {
                return Err("it has more than one context and names no current-context".into());
            }
            None => return Err("it names no cluster and user to connect as".into()),
        };
        let cluster = cluster.ok_or("its context names a cluster it does not have")?;
        let user = user.ok_or("its context names a user it does not have")?;
        let server = Url::parse(cluster.value.server.trim())
            .map_err(|_| format!("{} is not the address of an API server", cluster.value.server))?;
        if server.scheme() != "https" {
            return Err(
                "the API server is reached over https; a token is never sent in the clear".into()
            );
        }
        if cluster.value.certificate_authority.is_some()
            && cluster.value.certificate_authority_data.is_none()
        {
            return Err("it names its certificate authority by a file; put it in as \
                 certificate-authority-data instead"
                .into());
        }
        let ca = match &cluster.value.certificate_authority_data {
            Some(data) => Some(
                base64::engine::general_purpose::STANDARD
                    .decode(data.trim())
                    .map_err(|_| "its certificate-authority-data is not base64")?,
            ),
            None => None,
        };
        let token = match (&user.value.token, &user.value.client_certificate_data, &user.value.exec)
        {
            (Some(token), _, _) if !token.trim().is_empty() => token.trim().to_string(),
            (_, Some(_), _) => {
                return Err(
                    "it signs in with a client certificate; use a service account's token".into()
                );
            }
            (_, _, Some(_)) => {
                return Err(
                    "it signs in by running a program; use a service account's token".into()
                );
            }
            _ => return Err("its user has no token; use a service account's token".into()),
        };
        Ok(Self { server, ca, insecure: cluster.value.insecure_skip_tls_verify, token })
    }

    /// Whether it trusts any certificate the server shows, as its kubeconfig asks.
    pub fn insecure(&self) -> bool {
        self.insecure
    }

    pub fn client(&self) -> Result<Api, String> {
        let mut headers = HeaderMap::new();
        let mut bearer = HeaderValue::from_str(&format!("Bearer {}", self.token))
            .map_err(|_| "the token holds characters a header cannot")?;
        bearer.set_sensitive(true);
        headers.insert(AUTHORIZATION, bearer);
        let mut builder = reqwest::Client::builder()
            .user_agent(concat!("doc-kubernetes/", env!("CARGO_PKG_VERSION")))
            .default_headers(headers)
            .timeout(TIMEOUT)
            .redirect(reqwest::redirect::Policy::none());
        if let Some(ca) = &self.ca {
            let certificates = reqwest::Certificate::from_pem_bundle(ca)
                .map_err(|err| format!("its certificate authority cannot be read: {err}"))?;
            builder = builder.tls_certs_only(certificates);
        }
        if self.insecure {
            builder = builder.tls_danger_accept_invalid_certs(true);
        }
        let http = builder.build().map_err(|err| format!("no client could be made: {err}"))?;
        Ok(Api { http, server: self.server.clone() })
    }
}

/// A refusal or failure from the API server, worded for the page.
#[derive(Debug)]
pub struct Failed {
    pub detail: String,
}

pub struct Api {
    http: reqwest::Client,
    server: Url,
}

impl Api {
    fn url(&self, path: &str) -> Result<Url, Failed> {
        self.server
            .join(path.trim_start_matches('/'))
            .map_err(|err| Failed { detail: format!("{path} is not a path on the server: {err}") })
    }

    async fn answer(
        &self,
        path: &str,
        sent_as: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, Failed> {
        let answered = sent_as.send().await;
        sent("kubernetes", path, &answered);
        let answer = answered.map_err(|err| Failed {
            detail: format!("the API server could not be reached: {}", plain(&err)),
        })?;
        let status = answer.status().as_u16();
        if answer.status().is_success() {
            return Ok(answer);
        }
        let said: Value = answer.json().await.unwrap_or(Value::Null);
        let message = said["message"].as_str().unwrap_or("").to_string();
        let detail = match status {
            401 => "the API server refused the token: it has expired or been removed".to_string(),
            403 => format!(
                "the service account may not read {path}; bind it to the doc-read-only \
                 ClusterRole the Kubernetes page shows"
            ),
            404 => format!("the API server has no {path}"),
            _ => format!("the API server answered {status}: {message}"),
        };
        Err(Failed { detail })
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, Failed> {
        let url = self.url(path)?;
        let answer = self.answer(path, self.http.get(url)).await?;
        answer.json().await.map_err(|err| Failed {
            detail: format!("the API server's answer to {path} could not be read: {err}"),
        })
    }

    /// Every item of a list, a page at a time.
    pub async fn list<T: DeserializeOwned>(&self, path: &str) -> Result<Vec<T>, Failed> {
        let mut items = Vec::new();
        let mut next: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut url = self.url(path)?;
            url.query_pairs_mut().append_pair("limit", &PAGE.to_string());
            if let Some(next) = &next {
                url.query_pairs_mut().append_pair("continue", next);
            }
            let answer = self.answer(path, self.http.get(url)).await?;
            let page: List<T> = answer.json().await.map_err(|err| Failed {
                detail: format!("the API server's list of {path} could not be read: {err}"),
            })?;
            items.extend(page.items);
            match page.metadata.next.filter(|next| !next.is_empty()) {
                Some(more) => next = Some(more),
                None => return Ok(items),
            }
        }
        Ok(items)
    }

    /// Whether this service account may do `verb` to `resource` in the `apps` or core group,
    /// which is how the settings check makes sure it cannot change the cluster.
    pub async fn may(&self, verb: &str, group: &str, resource: &str) -> Result<bool, Failed> {
        let path = "apis/authorization.k8s.io/v1/selfsubjectaccessreviews";
        let review = json!({
            "apiVersion": "authorization.k8s.io/v1",
            "kind": "SelfSubjectAccessReview",
            "spec": { "resourceAttributes": { "verb": verb, "group": group, "resource": resource } },
        });
        let url = self.url(path)?;
        let answer = self.answer(path, self.http.post(url).json(&review)).await?;
        let said: Value = answer.json().await.map_err(|err| Failed {
            detail: format!("the access review could not be read: {err}"),
        })?;
        Ok(said["status"]["allowed"].as_bool().unwrap_or(false))
    }
}

/// An error and its causes, which is where reqwest says what actually went wrong.
fn plain(err: &reqwest::Error) -> String {
    let mut text = err.to_string();
    let mut cause = std::error::Error::source(err);
    while let Some(inner) = cause {
        text = format!("{text}: {inner}");
        cause = inner.source();
    }
    text
}

#[derive(Deserialize)]
struct List<T> {
    #[serde(default = "Vec::new")]
    items: Vec<T>,
    #[serde(default)]
    metadata: ListMeta,
}

#[derive(Default, Deserialize)]
struct ListMeta {
    #[serde(default, rename = "continue")]
    next: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Meta {
    pub name: String,
    pub namespace: String,
    pub uid: String,
    pub generation: i64,
    pub creation_timestamp: Option<DateTime<Utc>>,
    pub labels: BTreeMap<String, String>,
    pub annotations: BTreeMap<String, String>,
    pub owner_references: Vec<Owner>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Owner {
    pub kind: String,
    pub name: String,
    pub uid: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Condition {
    #[serde(rename = "type")]
    pub kind: String,
    pub status: String,
    pub reason: String,
    pub message: String,
    pub last_update_time: Option<DateTime<Utc>>,
    pub last_transition_time: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Template {
    pub metadata: Meta,
    pub spec: PodSpec,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct PodSpec {
    pub containers: Vec<Container>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Container {
    pub name: String,
    pub image: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Deployment {
    pub metadata: Meta,
    pub spec: DeploymentSpec,
    pub status: DeploymentStatus,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct DeploymentSpec {
    pub replicas: Option<i64>,
    pub paused: bool,
    pub template: Template,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DeploymentStatus {
    pub observed_generation: i64,
    pub replicas: i64,
    pub updated_replicas: i64,
    pub ready_replicas: i64,
    pub available_replicas: i64,
    pub unavailable_replicas: i64,
    pub conditions: Vec<Condition>,
}

impl Deployment {
    pub fn condition(&self, kind: &str) -> Option<&Condition> {
        self.status.conditions.iter().find(|condition| condition.kind == kind)
    }

    pub fn desired(&self) -> i64 {
        self.spec.replicas.unwrap_or(1)
    }

    pub fn revision(&self) -> Option<i64> {
        revision(&self.metadata)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ReplicaSet {
    pub metadata: Meta,
    pub spec: ReplicaSetSpec,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ReplicaSetSpec {
    pub template: Template,
}

impl ReplicaSet {
    pub fn revision(&self) -> Option<i64> {
        revision(&self.metadata)
    }

    /// The deployment it belongs to, by its UID.
    pub fn deployment(&self) -> Option<&str> {
        self.metadata
            .owner_references
            .iter()
            .find(|owner| owner.kind == "Deployment")
            .map(|owner| owner.uid.as_str())
    }

    /// The earlier revisions it served, when its template was rolled out again: Kubernetes takes
    /// a template's replica set up again, under a new revision, whenever the template comes back.
    pub fn history(&self) -> Vec<i64> {
        self.metadata
            .annotations
            .get("deployment.kubernetes.io/revision-history")
            .map(|history| history.split(',').filter_map(|r| r.trim().parse().ok()).collect())
            .unwrap_or_default()
    }

    pub fn reused(&self) -> bool {
        !self.history().is_empty()
    }

    /// The revision its template was first rolled out as.
    pub fn first(&self) -> Option<i64> {
        self.history().into_iter().chain(self.revision()).min()
    }

    /// Whether it served `revision`, now or before.
    pub fn served(&self, revision: i64) -> bool {
        self.revision() == Some(revision) || self.history().contains(&revision)
    }
}

fn revision(meta: &Meta) -> Option<i64> {
    meta.annotations.get("deployment.kubernetes.io/revision")?.trim().parse().ok()
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Pod {
    pub metadata: Meta,
    pub status: PodStatus,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PodStatus {
    pub phase: String,
    pub container_statuses: Vec<ContainerStatus>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ContainerStatus {
    pub restart_count: i64,
    pub ready: bool,
}

impl Pod {
    pub fn restarts(&self) -> i64 {
        self.status.container_statuses.iter().map(|status| status.restart_count).sum()
    }

    /// The replica set it belongs to, by name.
    pub fn replica_set(&self) -> Option<&str> {
        self.metadata
            .owner_references
            .iter()
            .find(|owner| owner.kind == "ReplicaSet")
            .map(|owner| owner.name.as_str())
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Node {
    pub metadata: Meta,
    pub status: NodeStatus,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct NodeStatus {
    pub allocatable: BTreeMap<String, String>,
    pub conditions: Vec<Condition>,
}

impl Node {
    pub fn ready(&self) -> bool {
        self.status
            .conditions
            .iter()
            .any(|condition| condition.kind == "Ready" && condition.status == "True")
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Namespace {
    pub metadata: Meta,
    pub status: NamespaceStatus,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct NamespaceStatus {
    pub phase: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Version {
    pub git_version: String,
    pub platform: String,
}

/// What metrics-server says a pod uses, where it is installed.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct PodMetrics {
    pub metadata: Meta,
    pub containers: Vec<ContainerMetrics>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ContainerMetrics {
    pub usage: BTreeMap<String, String>,
}

/// A Kubernetes quantity as a number: CPU in cores (`250m` is 0.25), memory in bytes (`1Gi`).
pub fn quantity(text: &str) -> Option<f64> {
    let text = text.trim();
    let split = text.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(text.len());
    let (number, suffix) = text.split_at(split);
    let number: f64 = number.parse().ok()?;
    let scale = match suffix {
        "" => 1.0,
        "n" => 1e-9,
        "u" => 1e-6,
        "m" => 1e-3,
        "k" => 1e3,
        "M" => 1e6,
        "G" => 1e9,
        "T" => 1e12,
        "P" => 1e15,
        "E" => 1e18,
        "Ki" => 1024.0,
        "Mi" => 1024.0_f64.powi(2),
        "Gi" => 1024.0_f64.powi(3),
        "Ti" => 1024.0_f64.powi(4),
        "Pi" => 1024.0_f64.powi(5),
        "Ei" => 1024.0_f64.powi(6),
        _ => return None,
    };
    Some(number * scale)
}
