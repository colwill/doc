//! A name for each proxied vendor account, following what Secret Storage onboards (ADR-0015 §4).
//!
//! The proxy addresses an account by a host of its own — `github.rundoc.sh` reaching
//! `api.github.com` — and the host has to resolve before any of that is useful. Secret Storage
//! says an account's host should exist, or should not any more; the record is written here,
//! because a name DOC answers for belongs to whatever keeps DOC's names.
//!
//! As in [`crate::services`], the records are ordinary ones. DOC writes a note on the ones it
//! keeps and touches only those, so a name somebody wrote by hand is left where it is.

use doc_plugin_sdk::{Backend, Event};
use serde_json::Value;

use crate::api;
use crate::names;
use crate::server::Server;
use crate::settings::Serving;
use crate::store::{Kind, Record, Store, Wanted};

/// A proxied account's host should exist, or should not any more.
pub const HOSTED: &str = "plugin.secrets.proxy.hosted";
pub const UNHOSTED: &str = "plugin.secrets.proxy.unhosted";

/// What the note of a record DOC keeps for an account starts with, and how it knows its own from
/// one somebody wrote at the page.
const NOTE: &str = "Proxied account";
/// Who the record says changed it, since an event has no caller.
const WHO: &str = "Secret Storage";

fn text<'a>(payload: &'a Value, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(Value::as_str).map(str::trim).filter(|found| !found.is_empty())
}

/// Whether DOC wrote this record for an account, or somebody did at the page.
fn ours(record: &Record) -> bool {
    record.note.starts_with(NOTE)
}

/// The record DOC would keep for an account, which is a `CNAME` at the proxy rather than an
/// address of its own: every account answers at the one listener (ADR-0015 §3), so a name that
/// followed the proxy's address would have to be rewritten each time that moved.
fn wanted(name: &str, account: &str, points_at: &str) -> Option<Wanted> {
    let points_at = names::plain(points_at).ok()?;
    Some(Wanted {
        name: name.to_string(),
        kind: Kind::CNAME.id().to_string(),
        value: points_at,
        ttl: None,
        note: format!("{NOTE} {account}, proxied by Secret Storage"),
    })
}

/// One of Secret Storage's announcements about an account's host.
pub async fn heard(backend: &Backend, server: &Server, event: &Event) {
    if event.topic != HOSTED && event.topic != UNHOSTED {
        return;
    }
    let serving = Serving::read(&backend.settings());
    let Some(host) = text(&event.payload, "host").and_then(|host| names::plain(host).ok()) else {
        return;
    };
    // A deployment may run the proxy under a domain somebody else answers for, which is its
    // business; DOC writes records only inside its own zones.
    if serving.zone_of(&host).is_none() {
        tracing::debug!(%host, "an account's host is under a domain DOC does not answer for");
        return;
    }
    let account = text(&event.payload, "account").unwrap_or("an account");
    let held = match Store(backend).at(&host).await {
        Ok(held) => held,
        Err(refusal) => {
            tracing::warn!(detail = refusal.detail, %host, "the records at a name could not be read");
            return;
        }
    };
    let mine = held.iter().find(|record| ours(record)).map(|record| record.id.to_string());
    if !held.is_empty() && mine.is_none() {
        tracing::info!(%host, "a record written at the page is already here, so it is left alone");
        return;
    }
    let done = match (event.topic.as_str(), text(&event.payload, "points_at")) {
        (HOSTED, Some(points_at)) => {
            let Some(asked) = wanted(&host, account, points_at) else {
                tracing::warn!(%host, %points_at, "an account's host has nowhere to point");
                return;
            };
            match mine {
                Some(id) => written(backend, server, &serving, Some(&id), asked, "changed").await,
                None => written(backend, server, &serving, None, asked, "added").await,
            }
        }
        // Without a public address there is nothing to point the name at, and a `CNAME` to
        // nowhere is worse than no record: it would answer, and answer wrongly.
        (HOSTED, None) => {
            tracing::info!(%host, "the proxy has no public address, so its accounts are not named");
            return;
        }
        // An account that has gone takes its host with it, so nothing is left pointing at a
        // proxy that would refuse the call anyway.
        (UNHOSTED, _) => match mine {
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
            tracing::info!(%host, %account, topic = %event.topic, "a proxied account's name followed its account")
        }
        Err(detail) => {
            tracing::warn!(%host, %account, %detail, "a proxied account's name could not be kept")
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
