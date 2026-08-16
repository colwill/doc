//! A name for each service, following whatever Infra stood up for it.
//!
//! Infra says a resource is standing, and what address it answers at, as it happens: a machine is
//! asked for, provisioned in the background, and only some minutes later does it have an address.
//! Nothing that asked for it is still waiting by then, so the name follows the event rather than
//! the request — which also means a machine somebody stood up on the Infra page gets its name the
//! same way one a software template asked for does.
//!
//! The records are ordinary ones. DOC writes a note on the ones it keeps and touches only those,
//! so a name somebody wrote by hand is left where it is.

use doc_plugin_sdk::{Backend, Event};
use serde_json::Value;

use crate::api;
use crate::server::Server;
use crate::settings::Serving;
use crate::store::{Kind, Record, Store, Wanted};

/// A resource of Infra's is standing and answering, or has been torn down.
pub const ACTIVE: &str = "plugin.infra.request.active";
pub const DELETED: &str = "plugin.infra.request.deleted";

/// What the note of a record DOC keeps for a service starts with, and how it knows its own from
/// one somebody wrote at the page.
const NOTE: &str = "Stood up for";
/// Who the record says changed it, since an event has no caller.
const WHO: &str = "Infra";

fn text<'a>(payload: &'a Value, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(Value::as_str).map(str::trim).filter(|found| !found.is_empty())
}

/// Whether DOC wrote this record for a service, or somebody did at the page.
fn ours(record: &Record) -> bool {
    record.note.starts_with(NOTE)
}

/// The record DOC would keep for a service at this address.
fn wanted(name: &str, service: &str, resource: &str, address: &str) -> Option<Wanted> {
    let kind = match address.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(_)) => Kind::A,
        Ok(std::net::IpAddr::V6(_)) => Kind::AAAA,
        // Infra gives an address; anything else is not something to point a service at.
        Err(_) => return None,
    };
    Some(Wanted {
        name: name.to_string(),
        kind: kind.id().to_string(),
        value: address.to_string(),
        ttl: None,
        note: format!("{NOTE} {service} by Infra: {resource}"),
    })
}

/// One of Infra's events, which is only ever about a service DOC can name.
pub async fn heard(backend: &Backend, server: &Server, event: &Event) {
    let serving = Serving::read(&backend.settings());
    if serving.service_domain.is_none() {
        return;
    }
    let Some(service) = text(&event.payload, "service") else { return };
    let Some(name) = serving.service_name(service) else {
        tracing::debug!(%service, "a service was stood up under a domain DOC does not answer for");
        return;
    };
    let resource = text(&event.payload, "name").unwrap_or("a resource");
    let held = match Store(backend).at(&name).await {
        Ok(held) => held,
        Err(refusal) => {
            tracing::warn!(detail = refusal.detail, %name, "the records at a name could not be read");
            return;
        }
    };
    let addresses: Vec<&Record> =
        held.iter().filter(|record| matches!(record.kind, Kind::A | Kind::AAAA)).collect();
    let mine = addresses.iter().find(|record| ours(record)).map(|record| record.id.to_string());
    if !addresses.is_empty() && mine.is_none() {
        tracing::info!(%name, "a record written at the page is already here, so it is left alone");
        return;
    }
    let done = match (event.topic.as_str(), text(&event.payload, "address")) {
        (ACTIVE, Some(address)) => {
            let Some(asked) = wanted(&name, service, resource, address) else { return };
            match mine {
                Some(id) => written(backend, server, &serving, Some(&id), asked, "changed").await,
                None => written(backend, server, &serving, None, asked, "added").await,
            }
        }
        // A resource that has gone takes its name with it, so nothing is left pointing at an
        // address somebody else will be given next.
        (DELETED, _) => match mine {
            Some(id) => match api::remove(backend, server, &id).await {
                Ok(_) => Ok(()),
                Err(refusal) => Err(refusal.detail),
            },
            None => return,
        },
        _ => return,
    };
    match done {
        Ok(()) => {
            tracing::info!(%name, %service, topic = %event.topic, "a service's name followed its infrastructure")
        }
        Err(detail) => {
            tracing::warn!(%name, %service, %detail, "a service's name could not be kept")
        }
    }
}

async fn written(
    backend: &Backend,
    server: &Server,
    serving: &Serving,
    replacing: Option<&str>,
    asked: Wanted,
    what: &str,
) -> Result<(), String> {
    let store = Store(backend);
    let written = match replacing {
        Some(id) => store.replace_as(serving, id, asked, WHO).await,
        None => store.create_as(serving, asked, WHO).await,
    };
    match written {
        Ok(record) => {
            api::announce(backend, server, what, &record).await;
            Ok(())
        }
        Err(refusal) => Err(refusal.detail),
    }
}
