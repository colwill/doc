//! The plugin's JSON routes, behind core's read or write check on `resources`, and the few other
//! plugins may read as themselves (`discovery`).

use doc_plugin_sdk::{Backend, Request, Response};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::apply::{self, Document, Mode};
use crate::definitions::{Definition, unconnectable};
use crate::graph::Graph;
use crate::kinds::{self, ALL, Kind, Ref};
use crate::model::{Node, Refusal};
use crate::ops;
use crate::rbac::Rbac;
use crate::store::Store;

type Answer = Result<(u16, Value), Refusal>;

const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 500;

pub async fn handle(backend: &Backend, request: Request) -> Response {
    let path = request.path.trim_end_matches('/').to_string();
    let segments: Vec<&str> = path.split('/').collect();
    match segments.as_slice() {
        ["ui", route @ ..] => crate::ui::handle(backend, &request, route).await,
        ["discovery", route @ ..] => crate::discovery::handle(backend, &request, route).await,
        ["api", route @ ..] => match api(backend, &request, route).await {
            Ok((204, _)) => Response::new(204, "application/json", Vec::new()),
            Ok((status, value)) => Response::new(
                status,
                "application/json",
                serde_json::to_vec(&value).unwrap_or_default(),
            ),
            Err(refusal) => refusal.response(),
        },
        _ => Refusal::missing("no such route").response(),
    }
}

fn ok(value: Value) -> Answer {
    Ok((200, value))
}

fn body<T: DeserializeOwned>(request: &Request) -> Result<T, Refusal> {
    serde_json::from_slice(&request.body)
        .map_err(|err| Refusal::bad(format!("the body is not what this route takes: {err}")))
}

pub fn query(request: &Request, key: &str) -> Option<String> {
    queries(request, key).into_iter().next()
}

pub fn queries(request: &Request, key: &str) -> Vec<String> {
    url::form_urlencoded::parse(request.query.as_bytes())
        .filter(|(name, _)| name == key)
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect()
}

/// A route's name as it was before the browser percent-encoded it.
pub fn decoded(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let hex = bytes.get(index + 1..index + 3).and_then(|pair| std::str::from_utf8(pair).ok());
        match (bytes[index], hex.and_then(|hex| u8::from_str_radix(hex, 16).ok())) {
            (b'%', Some(byte)) => {
                out.push(byte);
                index += 3;
            }
            (byte, _) => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Where a listing carries on from, for reading more than one page of it.
fn offset(request: &Request) -> Result<usize, Refusal> {
    match query(request, "offset") {
        None => Ok(0),
        Some(offset) => match offset.parse::<usize>() {
            Ok(offset) => Ok(offset),
            Err(_) => Err(Refusal::bad("`offset` is a number from 0")),
        },
    }
}

fn limit(request: &Request) -> Result<i64, Refusal> {
    match query(request, "limit") {
        None => Ok(DEFAULT_LIMIT),
        Some(text) => match text.parse::<i64>() {
            Ok(limit) if (1..=MAX_LIMIT).contains(&limit) => Ok(limit),
            _ => Err(Refusal::bad(format!("`limit` is a number from 1 to {MAX_LIMIT}"))),
        },
    }
}

fn flag(request: &Request, key: &str) -> bool {
    query(request, key).is_some_and(|value| matches!(value.as_str(), "true" | "1" | "yes"))
}

fn shown(node: &Node) -> Value {
    json!({ "kind": node.kind, "name": node.name, "key": node.key, "title": node.title })
}

fn definition_json(definition: &Definition) -> Value {
    json!({ "name": definition.name, "kinds": definition.kinds, "text": definition.to_string() })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Pair {
    from: String,
    to: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Assignment {
    service_account: String,
    to: String,
}

/// One or more `Connection[name](Kind, …)`, separated by commas or new lines.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewDefinitions {
    definitions: String,
}

fn definition_document(definition: &Definition) -> Document {
    Document {
        kind: "ConnectionDefinition".into(),
        name: definition.name.clone(),
        title: None,
        description: None,
        metadata: None,
        owner: None,
        organisation: None,
        _slack_channel: None,
        email: None,
        connections: None,
        kinds: Some(definition.kinds.iter().map(|kind| kind.name().to_string()).collect()),
    }
}

pub async fn api(backend: &Backend, request: &Request, path: &[&str]) -> Answer {
    let store = Store(backend);
    match (request.method.as_str(), path) {
        ("GET", ["kinds"]) => kinds_overview(&store).await,
        ("GET", ["version"]) => ok(json!({ "version": store.version().await? })),
        ("GET", ["definitions"]) => {
            let definitions = store.definitions().await?;
            ok(json!(definitions.iter().map(definition_json).collect::<Vec<_>>()))
        }
        ("POST", ["definitions"]) => {
            let asked: NewDefinitions = body(request)?;
            let definitions = Definition::parse_all(&asked.definitions)?;
            if definitions.is_empty() {
                return Err(Refusal::bad("write at least one Connection[name](Kind, …)"));
            }
            let documents = definitions.iter().map(definition_document).collect();
            let report = applied(backend, &store, documents, flag(request, "dry_run")).await?;
            let status = if report["changed"] == 0 { 200 } else { 201 };
            let shown: Vec<Value> = definitions.iter().map(definition_json).collect();
            Ok((status, json!({ "definitions": shown, "report": report })))
        }
        ("DELETE", ["definitions", name]) => {
            if !store.delete_definition(name).await? {
                return Err(Refusal::missing(format!("there is no connection definition {name}")));
            }
            ops::record(
                backend,
                &store,
                "definition.deleted",
                name,
                json!({ "by": ops::actor(backend) }),
            )
            .await;
            Ok((204, Value::Null))
        }
        ("POST", ["apply"]) => {
            let text = std::str::from_utf8(&request.body)
                .map_err(|_| Refusal::bad("the documents are not UTF-8 text"))?;
            let documents = apply::documents(text)?;
            ok(applied(backend, &store, documents, flag(request, "dry_run")).await?)
        }
        ("GET", ["resources"]) => {
            let kind = query(request, "kind").ok_or_else(|| Refusal::bad("name a `kind`"))?;
            list(
                backend,
                &store,
                kinds::kind(&kind)?,
                query(request, "q").as_deref(),
                limit(request)?,
                offset(request)?,
            )
            .await
        }
        ("GET", ["resources", kind, name @ ..]) if !name.is_empty() => {
            let at = Ref::new(kinds::kind(kind)?, decoded(&name.join("/")));
            detail(&store, &at).await
        }
        ("DELETE", ["resources", kind, name @ ..]) if !name.is_empty() => {
            let at = Ref::new(kinds::kind(kind)?, decoded(&name.join("/")));
            if !at.kind.stored() {
                return Err(Refusal::bad(format!("{} are not kept here", at.kind.plural())));
            }
            let resource = store
                .resource(at.kind, &at.name)
                .await?
                .ok_or_else(|| Refusal::missing(format!("there is no {at}")))?;
            store.delete(&resource).await?;
            let detail = json!({ "by": ops::actor(backend), "id": resource.id });
            ops::record(backend, &store, "deleted", &at.to_string(), detail).await;
            Ok((204, Value::Null))
        }
        ("GET", ["search"]) => {
            // Without `q`, everything of the kinds asked for, to choose from before typing.
            let text = query(request, "q").unwrap_or_default();
            let wanted = queries(request, "kinds");
            let kinds = match wanted.is_empty() {
                true => ALL.into_iter().filter(|kind| kind.written()).collect(),
                false => wanted
                    .iter()
                    .flat_map(|list| list.split(','))
                    .map(|kind| kinds::kind(kind.trim()))
                    .collect::<Result<Vec<_>, _>>()?,
            };
            let found = store.search(&text, &kinds, limit(request)?).await?;
            ok(json!(found.iter().map(shown).collect::<Vec<_>>()))
        }
        ("GET", ["neighbours"]) => {
            let of =
                query(request, "of").ok_or_else(|| Refusal::bad("name a resource with `of`"))?;
            let graph = Graph::new(&store).await?;
            let node = graph.node(&Ref::parse(&of)?).await?;
            let (neighbours, hidden) = graph.neighbours(&node).await?;
            let version = store.version().await?;
            ok(
                json!({ "node": shown(&node), "neighbours": neighbours, "hidden": hidden, "version": version }),
            )
        }
        ("GET", ["expand"]) => expand(&store, request).await,
        ("POST", ["connections"]) => {
            let pair: Pair = body(request)?;
            connect(backend, &store, &Ref::parse(&pair.from)?, &Ref::parse(&pair.to)?).await
        }
        ("POST", ["connections", "remove"]) => {
            let pair: Pair = body(request)?;
            disconnect(backend, &store, &Ref::parse(&pair.from)?, &Ref::parse(&pair.to)?).await
        }
        ("POST", ["assign"]) => {
            let asked: Assignment = body(request)?;
            let account = Ref::new(Kind::ServiceAccount, asked.service_account.trim());
            connect(backend, &store, &Ref::parse(&asked.to)?, &account).await
        }
        ("POST", ["assign", "remove"]) => {
            let asked: Assignment = body(request)?;
            let account = Ref::new(Kind::ServiceAccount, asked.service_account.trim());
            disconnect(backend, &store, &Ref::parse(&asked.to)?, &account).await
        }
        _ => Err(Refusal::missing("no such route")),
    }
}

/// Plans `documents` as whoever asked, and writes them unless this is a dry run.
async fn applied(
    backend: &Backend,
    store: &Store<'_>,
    documents: Vec<Document>,
    dry_run: bool,
) -> Result<Value, Refusal> {
    let rbac = Rbac::new(backend);
    let plan = apply::plan(backend, store, documents, &Mode::Apply { rbac: &rbac }).await?;
    if !dry_run && plan.changed() > 0 {
        let by = ops::actor(backend);
        apply::execute(store, &plan, &by).await?;
        let detail = json!({ "by": by, "changes": plan.changes });
        ops::record(backend, store, "applied", "catalogue", detail).await;
    }
    Ok(plan.report(dry_run))
}

async fn kinds_overview(store: &Store<'_>) -> Answer {
    let counts = store.counts().await?;
    let kinds: Vec<Value> = ALL
        .into_iter()
        .map(|kind| {
            let from = match kind {
                kind if kind.stored() => "resources",
                kind if kind.in_core() => "core",
                _ => "rbac",
            };
            json!({
                "kind": kind,
                "plural": kind.plural(),
                "slug": kind.slug(),
                "from": from,
                "published_by": kind.publishers(),
                "count": counts.get(&kind),
                // Where it sits in the platform's order, so anything drawing kinds side by side
                // draws them the same way round without working it out for itself.
                "rank": kind.rank(),
            })
        })
        .collect();
    let definitions = store.definitions().await?;
    let definitions: Vec<Value> = definitions.iter().map(definition_json).collect();
    ok(json!({ "kinds": kinds, "definitions": definitions, "version": store.version().await? }))
}

async fn list(
    backend: &Backend,
    store: &Store<'_>,
    kind: Kind,
    text: Option<&str>,
    limit: i64,
    offset: usize,
) -> Answer {
    let take = usize::try_from(limit).unwrap_or_default();
    match kind {
        kind if kind.written() => ok(json!(store.resources(kind, text, limit, offset).await?)),
        Kind::User | Kind::ServiceAccount => {
            let found = store.principals(kind, text, limit, offset).await?;
            ok(json!(found.iter().map(shown).collect::<Vec<_>>()))
        }
        Kind::Role => {
            let roles = Rbac::new(backend).roles().await.map_err(|reason| {
                Refusal::forbidden(format!("roles are read from rbac as you: {reason}"))
            })?;
            let text = text.map(str::to_lowercase);
            let found: Vec<Value> = roles
                .iter()
                .filter(|role| text.as_deref().is_none_or(|text| role.name.contains(text)))
                .skip(offset)
                .take(take)
                .map(shown)
                .collect();
            ok(json!(found))
        }
        other => Err(Refusal::bad(format!(
            "{} are listed under the roles and principals that hold them",
            other.plural()
        ))),
    }
}

async fn detail(store: &Store<'_>, at: &Ref) -> Answer {
    let graph = Graph::new(store).await?;
    let node = graph.node(at).await?;
    let resource = match at.kind.written() {
        true => json!(store.resource(at.kind, &at.name).await?),
        false => shown(&node),
    };
    let (connections, hidden) = graph.neighbours(&node).await?;
    ok(json!({ "resource": resource, "connections": connections, "hidden": hidden }))
}

async fn expand(store: &Store<'_>, request: &Request) -> Answer {
    let name = query(request, "definition").ok_or_else(|| Refusal::bad("name a `definition`"))?;
    let definition = store.definition(&name).await?;
    let asked = queries(request, "path");
    if asked.is_empty() {
        return Err(Refusal::bad("say where to start with `path`, such as Organisation:payments"));
    }
    let path: Vec<Ref> = asked.iter().map(|text| Ref::parse(text)).collect::<Result<_, _>>()?;
    let kinds: Vec<Kind> = path.iter().map(|at| at.kind).collect();
    if !definition.follows(&kinds) {
        return Err(Refusal::bad(format!("the path does not follow {definition}")));
    }
    let depth = match query(request, "depth") {
        None => 1,
        Some(text) => text
            .parse::<usize>()
            .ok()
            .filter(|depth| (1..=definition.kinds.len()).contains(depth))
            .ok_or_else(|| {
                Refusal::bad(format!("`depth` is a number from 1 to {}", definition.kinds.len()))
            })?,
    };
    let graph = Graph::new(store).await?;
    let mut nodes = Vec::new();
    for at in &path {
        nodes.push(graph.node(at).await?);
    }
    let level = graph.expand(&definition, &nodes, depth).await?;
    ok(json!({
        "definition": definition_json(&definition),
        "path": nodes.iter().map(shown).collect::<Vec<_>>(),
        "kind": level.kind,
        "children": level.children,
        "hidden": level.hidden,
        "version": store.version().await?,
    }))
}

async fn connect(backend: &Backend, store: &Store<'_>, a: &Ref, b: &Ref) -> Answer {
    let graph = Graph::new(store).await?;
    if let Some(reason) = unconnectable(&graph.definitions, a.kind, b.kind) {
        return Err(Refusal::bad(reason));
    }
    let (from, to) = (graph.node(a).await?, graph.node(b).await?);
    for node in [&from, &to] {
        if node.kind == Kind::ServiceAccount {
            ops::may_assign(backend, store, node).await?;
        }
    }
    let by = ops::actor(backend);
    let added = store.connect(&from, &to, "api", &by).await?;
    if added {
        let detail = json!({ "to": b.to_string(), "by": by });
        ops::record(backend, store, "connected", &a.to_string(), detail).await;
    }
    let answer = json!({ "from": shown(&from), "to": shown(&to), "added": added });
    Ok((if added { 201 } else { 200 }, answer))
}

async fn disconnect(backend: &Backend, store: &Store<'_>, a: &Ref, b: &Ref) -> Answer {
    let graph = Graph::new(store).await?;
    let (from, to) = (graph.node(a).await?, graph.node(b).await?);
    for node in [&from, &to] {
        if node.kind == Kind::ServiceAccount {
            ops::may_assign(backend, store, node).await?;
        }
    }
    if !store.disconnect(&from, &to).await? {
        return Err(Refusal::missing(format!("{a} and {b} are not related")));
    }
    let detail = json!({ "from": b.to_string(), "by": ops::actor(backend) });
    ops::record(backend, store, "disconnected", &a.to_string(), detail).await;
    Ok((204, Value::Null))
}
