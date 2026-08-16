//! The JSON routes under `api/`, and the writes both they and the pages make: each is published,
//! audited, since a record decides where people's traffic goes, and read into the server's table
//! at once.

use doc_plugin_sdk::{Backend, Request, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::server::{Server, Snapshot, Status};
use crate::settings::Serving;
use crate::store::{self, Record, Store, Wanted};
use crate::{ID, Refusal, names};

type Answer = Result<(u16, Value), Refusal>;

pub fn query(request: &Request, key: &str) -> Option<String> {
    url::form_urlencoded::parse(request.query.as_bytes())
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn body<T: for<'de> Deserialize<'de>>(request: &Request) -> Result<T, Refusal> {
    let bytes = if request.body.is_empty() { &b"{}"[..] } else { &request.body[..] };
    serde_json::from_slice(bytes)
        .map_err(|err| Refusal::bad(format!("the body is not what this takes: {err}")))
}

/// Tells subscribers and the audit log; the change is already stored, so a failure is logged.
pub(crate) async fn announce(backend: &Backend, server: &Server, what: &str, record: &Record) {
    server.refresh();
    let payload = json!({
        "id": record.id,
        "name": record.name,
        "type": record.kind,
        "value": record.value,
    });
    let topic = format!("plugin.{ID}.record.{what}");
    if let Err(err) = backend.publish(&topic, payload.clone()).await {
        tracing::warn!(%err, topic, "a change was stored but not announced");
    }
    if let Err(err) = backend.audit(&format!("record.{what}"), Some(&record.name), payload).await {
        tracing::warn!(%err, "a change was stored but not audited");
    }
}

pub async fn add(backend: &Backend, server: &Server, wanted: Wanted) -> Result<Record, Refusal> {
    let serving = Serving::read(&backend.settings());
    let record = Store(backend).create(&serving, wanted).await?;
    announce(backend, server, "added", &record).await;
    Ok(record)
}

pub async fn replace(
    backend: &Backend,
    server: &Server,
    id: &str,
    wanted: Wanted,
) -> Result<Record, Refusal> {
    let serving = Serving::read(&backend.settings());
    let record = Store(backend).replace(&serving, id, wanted).await?;
    announce(backend, server, "changed", &record).await;
    Ok(record)
}

pub async fn remove(backend: &Backend, server: &Server, id: &str) -> Result<Record, Refusal> {
    let record = Store(backend).remove(id).await?;
    announce(backend, server, "removed", &record).await;
    Ok(record)
}

/// The records, sorted by name and type, optionally only those in one domain.
pub async fn records(backend: &Backend, zone: Option<&str>) -> Result<Vec<Record>, Refusal> {
    let mut records = Store(backend).records().await?;
    if let Some(zone) = zone {
        records.retain(|record| names::within(&record.name, zone));
    }
    records.sort_by(|a, b| (&a.name, a.kind.id(), &a.value).cmp(&(&b.name, b.kind.id(), &b.value)));
    Ok(records)
}

pub fn status_shown(snapshot: &Snapshot) -> Value {
    let (state, listen, since, problem) = match &snapshot.status {
        Status::Off => ("off", None, None, None),
        Status::OverHttpsOnly => ("over-https-only", None, None, None),
        Status::Waiting { listen, problem } => ("waiting", Some(listen), None, Some(problem)),
        Status::Answering { listen, since } => ("answering", Some(listen), Some(since), None),
    };
    json!({
        "state": state,
        "listen": listen.map(ToString::to_string),
        "since": since,
        "problem": problem.or(snapshot.problem.as_ref()),
        "zones": snapshot.serving.zones,
        "upstreams": snapshot.serving.upstreams.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "records_read": snapshot.ready,
        "names": snapshot.names,
        "unserved": snapshot.unserved,
        "plugin_names": {
            "on": snapshot.serving.plugin_domain.is_some(),
            "domain": snapshot.serving.plugin_domain,
            "addresses": snapshot.serving.plugin_addresses.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "names": snapshot.named,
        },
        "over_https": {
            "on": snapshot.serving.over_https,
            "url": crate::doh::url(&snapshot.serving, false),
            "public_url": crate::doh::public_url(&snapshot.serving),
        },
        "since_start": {
            "answered": snapshot.answered,
            "forwarded": snapshot.forwarded,
            "refused": snapshot.refused,
            "failed": snapshot.failed,
            "dropped": snapshot.dropped,
            "over_https": snapshot.over_https,
        },
    })
}

pub async fn handle(
    backend: &Backend,
    server: &Server,
    request: &Request,
    path: &[&str],
) -> Response {
    match route(backend, server, request, path).await {
        Ok((status, value)) => {
            let mut response = Response::json(&value);
            response.status = status;
            response
        }
        Err(refusal) => refusal.response(),
    }
}

async fn route(backend: &Backend, server: &Server, request: &Request, path: &[&str]) -> Answer {
    match (request.method.as_str(), path) {
        ("GET", ["status"]) => Ok((200, status_shown(&server.snapshot()))),
        ("GET", ["records"]) => {
            let zone = query(request, "zone").map(|zone| names::plain(&zone)).transpose();
            let records = records(backend, zone.map_err(Refusal::bad)?.as_deref()).await?;
            Ok((200, json!({ "records": records.iter().map(store::shown).collect::<Vec<_>>() })))
        }
        ("POST", ["records"]) => {
            let record = add(backend, server, body(request)?).await?;
            Ok((201, store::shown(&record)))
        }
        ("GET", ["records", id]) => Ok((200, store::shown(&Store(backend).found(id).await?))),
        ("PUT", ["records", id]) => {
            let record = replace(backend, server, id, body(request)?).await?;
            Ok((200, store::shown(&record)))
        }
        ("DELETE", ["records", id]) => {
            let record = remove(backend, server, id).await?;
            Ok((200, json!({ "removed": store::shown(&record) })))
        }
        _ => Err(Refusal::missing("no such route")),
    }
}
