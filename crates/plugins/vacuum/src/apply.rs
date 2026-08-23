//! Approval: what an administrator approves is written to DOC as them, through each plugin's own
//! API, so every check those plugins make of a person applies. Pages are imported into the
//! Knowledge Base a space at a time, every page the Data Vacuum has put in that space included,
//! since an import keeps exactly what it is given; resources are applied to the Catalogue together,
//! and one by one when that fails, so a bad one does not hold the rest back; discussions start
//! Watercooler threads.

use std::collections::BTreeMap;

use chrono::Utc;
use doc_plugin_sdk::Backend;
use serde_json::{Value, json};

use crate::Refusal;
use crate::store::{
    APPLIED, APPROVED, CATALOGUE, DONE, Item, KB, NOT_APPLIED, REJECTED, REVIEW, Run, STAGED,
    Store, WATER,
};

/// What approving came to, to say on the page.
#[derive(Debug, Default)]
pub struct Applied {
    pub written: usize,
    pub failed: usize,
    pub rejected: usize,
}

fn detail(status: u16, body: &Value) -> String {
    let said = body["detail"].as_str().or(body["title"].as_str()).unwrap_or("no detail");
    format!("{status}: {said}")
}

async fn mark(store: &Store<'_>, items: &mut [Item], state: &str, error: Option<String>) {
    for item in items.iter_mut() {
        item.state = state.to_string();
        item.error.clone_from(&error);
        if state == APPLIED {
            item.applied_at = Some(Utc::now());
        }
        if let Err(refusal) = store.save_item(item).await {
            tracing::warn!(detail = %refusal.detail, item = %item.id, "an item's state was not saved");
        }
    }
}

async fn pages(backend: &Backend, store: &Store<'_>, approved: Vec<Item>, done: &mut Applied) {
    let mut by_space: BTreeMap<String, Vec<Item>> = BTreeMap::new();
    for item in approved {
        by_space.entry(item.space.clone().unwrap_or_default()).or_default().push(item);
    }
    for (space, mut items) in by_space {
        // The newest version of each page this plugin has put in the space, from every run.
        let mut latest: BTreeMap<String, Item> = BTreeMap::new();
        let held = match store.pages_of(&space).await {
            Ok(held) => held,
            Err(refusal) => {
                done.failed += items.len();
                mark(store, &mut items, NOT_APPLIED, Some(refusal.detail)).await;
                continue;
            }
        };
        for item in held.into_iter().chain(items.iter().cloned()) {
            let path = item.path.clone().unwrap_or_default();
            let newer =
                latest.get(&path).is_none_or(|kept| item.id >= kept.id || item.state == APPROVED);
            if newer {
                latest.insert(path, item);
            }
        }
        let title = items.iter().find_map(|item| item.space_title.clone());
        let body = json!({
            "space": space,
            "title": title,
            "pages": latest.values().map(|item| json!({
                "path": item.path, "content": item.content,
            })).collect::<Vec<_>>(),
        });
        match backend.ask("kb", "POST", "imports/pages", None, Some(body)).await {
            Ok((status, _)) if (200..300).contains(&status) => {
                done.written += items.len();
                mark(store, &mut items, APPLIED, None).await;
            }
            Ok((status, answer)) => {
                done.failed += items.len();
                mark(store, &mut items, NOT_APPLIED, Some(detail(status, &answer))).await;
            }
            Err(err) => {
                done.failed += items.len();
                let why = format!("the Knowledge Base could not be asked: {err}");
                mark(store, &mut items, NOT_APPLIED, Some(why)).await;
            }
        }
    }
}

async fn apply_documents(backend: &Backend, documents: Vec<Value>) -> Result<(), String> {
    match backend.ask("resources", "POST", "apply", None, Some(Value::Array(documents))).await {
        Ok((status, _)) if (200..300).contains(&status) => Ok(()),
        Ok((status, answer)) => Err(detail(status, &answer)),
        Err(err) => Err(format!("the Catalogue could not be asked: {err}")),
    }
}

async fn resources(
    backend: &Backend,
    store: &Store<'_>,
    mut approved: Vec<Item>,
    done: &mut Applied,
) {
    if approved.is_empty() {
        return;
    }
    let documents: Vec<Value> = approved
        .iter()
        .map(|item| serde_json::from_str(&item.content).unwrap_or(Value::Null))
        .collect();
    if apply_documents(backend, documents.clone()).await.is_ok() {
        done.written += approved.len();
        mark(store, &mut approved, APPLIED, None).await;
        return;
    }
    // Together they failed: one at a time, so each says what is wrong with it.
    for (item, document) in approved.iter_mut().zip(documents) {
        let one = std::slice::from_mut(item);
        match apply_documents(backend, vec![document]).await {
            Ok(()) => {
                done.written += 1;
                mark(store, one, APPLIED, None).await;
            }
            Err(why) => {
                done.failed += 1;
                mark(store, one, NOT_APPLIED, Some(why)).await;
            }
        }
    }
}

async fn discussions(
    backend: &Backend,
    store: &Store<'_>,
    approved: Vec<Item>,
    done: &mut Applied,
) {
    for mut item in approved {
        let mut body = item.content.clone();
        if !item.source_url.is_empty() {
            body.push_str(&format!("\n\n*From [{}]({}).*", item.source, item.source_url));
        }
        let thread = json!({ "title": item.title, "body": body, "tags": item.tags });
        let one = std::slice::from_mut(&mut item);
        match backend.ask("water", "POST", "threads", None, Some(thread)).await {
            Ok((status, _)) if (200..300).contains(&status) => {
                done.written += 1;
                mark(store, one, APPLIED, None).await;
            }
            Ok((status, answer)) => {
                done.failed += 1;
                mark(store, one, NOT_APPLIED, Some(detail(status, &answer))).await;
            }
            Err(err) => {
                done.failed += 1;
                mark(
                    store,
                    one,
                    NOT_APPLIED,
                    Some(format!("Watercooler could not be asked: {err}")),
                )
                .await;
            }
        }
    }
}

/// Approves the chosen items, or every one still staged when `chosen` is `None`, and writes
/// them; items that failed before are tried again when chosen.
pub async fn approve(
    backend: &Backend,
    run: &Run,
    chosen: Option<&[String]>,
) -> Result<Applied, Refusal> {
    if run.state != REVIEW {
        return Err(Refusal::conflict(
            "a run's items are approved once it has finished taking them in",
        ));
    }
    let store = Store(backend);
    let picked = |item: &Item| match chosen {
        Some(ids) => ids.iter().any(|id| *id == item.id.to_string()),
        None => item.state == STAGED,
    };
    let mut approved: Vec<Item> = store
        .items(run.id)
        .await?
        .into_iter()
        .filter(|item| picked(item) && [STAGED, NOT_APPLIED].contains(&item.state.as_str()))
        .collect();
    if approved.is_empty() {
        return Err(Refusal::bad("choose items still waiting to be approved"));
    }
    mark(&store, &mut approved, APPROVED, None).await;
    let mut done = Applied::default();
    let of = |destination: &str| -> Vec<Item> {
        approved.iter().filter(|item| item.destination == destination).cloned().collect()
    };
    pages(backend, &store, of(KB), &mut done).await;
    resources(backend, &store, of(CATALOGUE), &mut done).await;
    discussions(backend, &store, of(WATER), &mut done).await;
    settle(backend, run).await?;
    let detail = json!({ "run": run.id, "written": done.written, "failed": done.failed });
    if let Err(err) = backend.audit("items.approved", Some(&run.id.to_string()), detail).await {
        tracing::warn!(%err, "an approval was not audited");
    }
    Ok(done)
}

pub async fn reject(backend: &Backend, run: &Run, chosen: &[String]) -> Result<Applied, Refusal> {
    let store = Store(backend);
    let mut rejected: Vec<Item> = store
        .items(run.id)
        .await?
        .into_iter()
        .filter(|item| chosen.iter().any(|id| *id == item.id.to_string()))
        .filter(|item| [STAGED, NOT_APPLIED].contains(&item.state.as_str()))
        .collect();
    let count = rejected.len();
    mark(&store, &mut rejected, REJECTED, None).await;
    settle(backend, run).await?;
    Ok(Applied { rejected: count, ..Applied::default() })
}

/// A run with nothing left to decide is done.
async fn settle(backend: &Backend, run: &Run) -> Result<(), Refusal> {
    let store = Store(backend);
    let open = store
        .items(run.id)
        .await?
        .iter()
        .any(|item| [STAGED, APPROVED, NOT_APPLIED].contains(&item.state.as_str()));
    if !open && run.state == REVIEW {
        let mut run = run.clone();
        run.state = DONE.to_string();
        store.save_run(&run).await?;
    }
    Ok(())
}
