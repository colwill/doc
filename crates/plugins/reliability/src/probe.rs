//! Checking each service's health URL: a GET that has to answer below 400 in the time the settings
//! give. A service is down after the settings' number of failed checks in a row, from the first of
//! them, and up again at the next check that passes. The answer's body is never read or kept.

use std::net::IpAddr;
use std::time::{Duration as Wait, Instant};

use chrono::{DateTime, Duration, Utc};
use doc_plugin_sdk::telemetry::sent;
use doc_plugin_sdk::{Backend, DataRequest, PluginError, Query};
use futures::StreamExt;
use serde_json::{Value, json};
use url::Url;

use crate::record::{self, Watching};
use crate::settings::Definitions;
use crate::store::Subject;

/// Services checked at once.
const AT_ONCE: usize = 16;
/// More than this since the last check is a gap in watching, not time watched.
const GAP_SECONDS: i64 = 300;
const BATCH: usize = 100;

pub fn client() -> Result<reqwest::Client, PluginError> {
    reqwest::Client::builder()
        .user_agent(concat!("doc-reliability/", env!("CARGO_PKG_VERSION")))
        // A redirect could lead anywhere, including where a check may not go: 3xx is an answer.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|err| PluginError::Message(format!("the HTTP client could not be built: {err}")))
}

/// Why a check may never reach an address unless its host is listed: it is this host, a link-local
/// address — which is where cloud metadata lives — or not one machine.
fn forbidden(address: IpAddr) -> Option<&'static str> {
    match address {
        IpAddr::V4(v4) => match () {
            () if v4.is_loopback() => Some("a loopback address"),
            () if v4.is_link_local() => Some("a link-local address, where cloud metadata is"),
            () if v4.is_unspecified() => Some("the unspecified address"),
            () if v4.is_broadcast() || v4.is_multicast() => Some("not one machine"),
            () => None,
        },
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => forbidden(IpAddr::V4(v4)),
            None if v6.is_loopback() => Some("a loopback address"),
            None if v6.segments()[0] & 0xffc0 == 0xfe80 => Some("a link-local address"),
            None if v6.is_unspecified() => Some("the unspecified address"),
            None if v6.is_multicast() => Some("not one machine"),
            None => None,
        },
    }
}

/// Whether a URL may be checked: `Ok` with the URL, or why not.
pub async fn allowed(text: &str, definitions: &Definitions) -> Result<Url, String> {
    let url = Url::parse(text.trim()).map_err(|err| format!("it is not a URL: {err}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("a health URL is http or https".into());
    }
    let host = url.host_str().ok_or("a health URL names a host")?.to_ascii_lowercase();
    if definitions.allowed.contains(&host) {
        return Ok(url);
    }
    let port = url.port_or_known_default().unwrap_or(443);
    let bare = host.trim_start_matches('[').trim_end_matches(']').to_string();
    let found = tokio::net::lookup_host((bare.as_str(), port))
        .await
        .map_err(|err| format!("{host} could not be looked up: {err}"))?;
    for address in found {
        if let Some(why) = forbidden(address.ip()) {
            let named = match bare == address.ip().to_string() {
                true => format!("{host} is {why}"),
                false => format!("{host} is {}, {why}", address.ip()),
            };
            return Err(format!("{named}, which checks may not reach unless the settings list it"));
        }
    }
    Ok(url)
}

/// What one check found.
struct Checked {
    subject: Subject,
    at: DateTime<Utc>,
    passed: bool,
    status: Option<u16>,
    latency_ms: i64,
    problem: Option<String>,
}

async fn check(http: &reqwest::Client, subject: Subject, definitions: &Definitions) -> Checked {
    let at = Utc::now();
    let started = Instant::now();
    let url = match allowed(subject.url.as_deref().unwrap_or_default(), definitions).await {
        Ok(url) => url,
        Err(problem) => {
            return Checked {
                subject,
                at,
                passed: false,
                status: None,
                latency_ms: 0,
                problem: Some(problem),
            };
        }
    };
    let answer = http.get(url).timeout(Wait::from_secs_f64(definitions.timeout)).send().await;
    sent("health", "check", &answer);
    let latency_ms = started.elapsed().as_millis() as i64;
    let (passed, status, problem) = match answer {
        Ok(answer) => {
            let status = answer.status().as_u16();
            match status < 400 {
                true => (true, Some(status), None),
                false => (false, Some(status), Some(format!("it answered {status}"))),
            }
        }
        Err(err) if err.is_timeout() => {
            (false, None, Some(format!("no answer within {}s", definitions.timeout)))
        }
        Err(err) if err.is_connect() => (false, None, Some("it could not be connected to".into())),
        Err(_) => (false, None, Some("the request failed".into())),
    };
    Checked { subject, at, passed, status, latency_ms, problem }
}

/// Checks every service with a health URL, once.
pub async fn run(
    backend: &Backend,
    http: &reqwest::Client,
    definitions: &Definitions,
) -> Result<Value, PluginError> {
    let subjects: Vec<Subject> = backend
        .query_all(
            Query::new("subjects")
                .filter(json!({ "kind": "service", "url": { "is_null": false } })),
        )
        .await?;
    let total = subjects.len();
    let checked: Vec<Checked> = futures::stream::iter(subjects)
        .map(|subject| check(http, subject, definitions))
        .buffer_unordered(AT_ONCE)
        .collect()
        .await;
    let mut watching = Watching::default();
    let mut writes = Vec::new();
    let mut down = 0;
    for Checked { mut subject, at, passed, status, latency_ms, problem } in checked {
        let watched_from = subject
            .checked_at
            .filter(|last| at - *last <= Duration::seconds(GAP_SECONDS))
            .unwrap_or(at);
        watching.add(&subject.subject, watched_from, at, !passed);
        subject.since.get_or_insert(at);
        if passed {
            if subject.state == "down" {
                record::close(backend, &subject.subject, at, Some("check")).await?;
            }
            subject.state = "up".into();
            subject.failing = 0;
            subject.failing_since = None;
        } else {
            subject.failing += 1;
            let since = *subject.failing_since.get_or_insert(at);
            if subject.failing >= definitions.failures {
                record::open(backend, &subject.subject, since, "check", problem.clone(), None)
                    .await?;
                subject.state = "down".into();
            }
        }
        down += usize::from(subject.state == "down");
        subject.checked_at = Some(at);
        subject.status = status.map(i64::from);
        subject.latency_ms = Some(latency_ms);
        subject.problem = problem;
        // Only what checking changes, so objectives changed meanwhile are kept.
        let set = json!({
            "state": subject.state, "failing": subject.failing,
            "failing_since": subject.failing_since, "since": subject.since,
            "checked_at": subject.checked_at, "status": subject.status,
            "latency_ms": subject.latency_ms, "problem": subject.problem,
        });
        writes.push(DataRequest::update("subjects", subject.subject.as_str(), set));
    }
    for chunk in writes.chunks(BATCH) {
        backend.batch(chunk.to_vec()).await?;
    }
    watching.flush(backend).await?;
    Ok(json!({ "checked": total, "down": down }))
}
