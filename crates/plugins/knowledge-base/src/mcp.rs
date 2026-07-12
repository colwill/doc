//! The Knowledge Base as an MCP server at `api/mcp`: Streamable HTTP with JSON responses and no
//! sessions, for people connecting with a personal access token. Core checks `plugin:kb:user` as a
//! read, since the manifest declares the route one; service accounts are refused here.

use std::collections::BTreeMap;

use doc_plugin_sdk::{Backend, Request, Response};
use serde_json::{Value, json};

use crate::Refusal;
use crate::api::decoded;
use crate::imports::{checked_resource, page_url};
use crate::store::Store;
use crate::{sources, text};

/// Newest first; an unknown version asked for is answered with the newest.
const VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const MAX_RESULTS: i64 = 50;

const INSTRUCTIONS: &str = "DOC's Knowledge Base: documentation imported from MkDocs and Markdown, \
GitHub, Confluence and Google Drive, grouped into spaces and linked to the services, teams and \
other resources it documents. Search with search_docs, read a result with get_document, and find \
everything written about a service with find_docs_for_resource (for example service:card-gateway).";

fn rpc_error(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn json_response(status: u16, value: &Value) -> Response {
    Response::new(status, "application/json", serde_json::to_vec(value).unwrap_or_default())
}

pub async fn handle(backend: &Backend, request: &Request) -> Response {
    if request.method != "POST" {
        return Response::new(
            405,
            "application/json",
            b"{\"detail\":\"MCP is spoken over POST here; this server opens no event streams\"}"
                .to_vec(),
        )
        .with_header("allow", "POST");
    }
    match backend.caller() {
        Some(caller) if caller.kind == "user" => {}
        _ => {
            let detail =
                "the Knowledge Base's MCP server is for people, with a personal access token";
            return Refusal::forbidden(detail).response();
        }
    }
    if let Some(version) = request.headers.get("mcp-protocol-version")
        && !VERSIONS.contains(&version.as_str())
    {
        let message = format!("MCP protocol version {version} is not supported");
        return json_response(400, &rpc_error(&Value::Null, -32600, &message));
    }
    let Ok(message) = serde_json::from_slice::<Value>(&request.body) else {
        return json_response(400, &rpc_error(&Value::Null, -32700, "the body is not JSON"));
    };
    let store = Store(backend);
    match message {
        Value::Array(messages) => {
            let mut answers = Vec::new();
            for message in &messages {
                if let Some(answer) = answer(&store, message).await {
                    answers.push(answer);
                }
            }
            match answers.is_empty() {
                true => Response::new(202, "application/json", Vec::new()),
                false => json_response(200, &Value::Array(answers)),
            }
        }
        message => match answer(&store, &message).await {
            Some(answer) => json_response(200, &answer),
            None if message.get("method").is_some()
                || message.get("result").is_some()
                || message.get("error").is_some() =>
            {
                Response::new(202, "application/json", Vec::new())
            }
            None => json_response(400, &rpc_error(&Value::Null, -32600, "not a JSON-RPC message")),
        },
    }
}

/// The answer to a request, or nothing for a notification or a response.
async fn answer(store: &Store<'_>, message: &Value) -> Option<Value> {
    let id = message.get("id").filter(|id| !id.is_null())?;
    let Some(method) = message["method"].as_str() else {
        return Some(rpc_error(id, -32600, "a request names a method"));
    };
    let params = &message["params"];
    let result = match method {
        "initialize" => Ok(initialize(params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => call(store, params).await,
        _ => Err((-32601, format!("there is no method {method}"))),
    };
    Some(match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => rpc_error(id, code, &message),
    })
}

fn initialize(params: &Value) -> Value {
    let asked = params["protocolVersion"].as_str().unwrap_or_default();
    let version = VERSIONS.iter().find(|version| **version == asked).unwrap_or(&VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": {
            "name": "doc-kb",
            "title": "DOC Knowledge Base",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": INSTRUCTIONS,
    })
}

fn tools() -> Value {
    let read_only = json!({ "readOnlyHint": true, "idempotentHint": true, "openWorldHint": false });
    json!([
        {
            "name": "search_docs",
            "title": "Search the docs",
            "description": "Full-text search across the Knowledge Base, best matches first. Each result has its title, space, path (for get_document), link and a snippet with the matching words in bold.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Words to find, in any order. \"Quoted phrases\", OR and -excluded words work as in a web search." },
                    "space": { "type": "string", "description": "Only this space, by its key from list_spaces." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": MAX_RESULTS, "description": "At most this many results; 10 unless given." },
                },
                "required": ["query"],
            },
            "annotations": read_only,
        },
        {
            "name": "get_document",
            "title": "Read a document",
            "description": "A document's content as Markdown, with where it came from, the resources it documents, its labels and its link.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "space": { "type": "string", "description": "The space's key, such as card-gateway." },
                    "path": { "type": "string", "description": "The document's path in the space, as search_docs gives it, such as architecture/overview.md." },
                },
                "required": ["space", "path"],
            },
            "annotations": read_only,
        },
        {
            "name": "list_spaces",
            "title": "List the spaces",
            "description": "Every space: its key, name, how many documents it holds, where they come from and the resource it documents, if one.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": read_only,
        },
        {
            "name": "find_docs_for_resource",
            "title": "Find docs for a resource",
            "description": "Every document about a resource from Resource Definitions, from every source, such as all the docs for service:card-gateway.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "resource": { "type": "string", "description": "Written kind:name, such as service:card-gateway or team:payments-core." },
                },
                "required": ["resource"],
            },
            "annotations": read_only,
        },
    ])
}

/// Where the frontend is, so links work outside DOC.
fn public_url() -> String {
    std::env::var("DOC_PUBLIC_URL")
        .map(|url| url.trim_end_matches('/').to_string())
        .unwrap_or_default()
}

fn text_of(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().to_string()
}

/// What a tool found: a readable text for the model, and the same as structured content.
fn found(text: &str, structured: &Value) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "structuredContent": structured, "isError": false })
}

/// A tool that could not do what was asked says why, for the model to try again.
fn failed(reason: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": reason }], "isError": true })
}

async fn call(store: &Store<'_>, params: &Value) -> Result<Value, (i64, String)> {
    let arguments = &params["arguments"];
    let argument = |key: &str| {
        arguments[key].as_str().map(str::trim).filter(|value| !value.is_empty()).map(str::to_string)
    };
    let outcome = match params["name"].as_str().unwrap_or_default() {
        "search_docs" => match argument("query") {
            Some(query) => {
                let limit = arguments["limit"].as_i64().unwrap_or(10).clamp(1, MAX_RESULTS);
                search(store, &query, argument("space").as_deref(), limit).await
            }
            None => return Ok(failed("search_docs needs a query")),
        },
        "get_document" => match (argument("space"), argument("path")) {
            (Some(space), Some(path)) => document(store, &space, &path).await,
            _ => {
                return Ok(failed(
                    "get_document needs a space and a path, as search_docs gives them",
                ));
            }
        },
        "list_spaces" => spaces(store).await,
        "find_docs_for_resource" => {
            match argument("resource").map(|resource| checked_resource(&resource)) {
                Some(Ok(resource)) => documenting(store, &resource).await,
                Some(Err(refusal)) => return Ok(failed(&refusal.detail)),
                None => {
                    return Ok(failed(
                        "find_docs_for_resource needs a resource, such as service:card-gateway",
                    ));
                }
            }
        }
        other => return Err((-32602, format!("there is no tool {other}"))),
    };
    Ok(outcome.unwrap_or_else(|refusal| failed(&refusal.detail)))
}

async fn space_names(store: &Store<'_>) -> Result<BTreeMap<String, String>, Refusal> {
    Ok(store.spaces().await?.into_iter().map(|space| (space.key, space.name)).collect())
}

async fn search(
    store: &Store<'_>,
    query: &str,
    space: Option<&str>,
    limit: i64,
) -> Result<Value, Refusal> {
    let names = space_names(store).await?;
    let base = public_url();
    let mut results = Vec::new();
    let mut lines = Vec::new();
    for hit in store.search(query, space, limit).await? {
        let space = text_of(&hit, "space");
        let path = text_of(&hit, "path");
        let title = text_of(&hit, "title");
        let snippet = text_of(&hit, "snippet").replace(['⟦', '⟧'], "**");
        let url = format!("{base}{}", page_url(&space, &path));
        let name = names.get(&space).cloned().unwrap_or_else(|| space.clone());
        lines.push(format!(
            "{}. {title} — {name} (space: {space}, path: {path})\n   {url}\n   {snippet}",
            results.len() + 1
        ));
        results.push(json!({ "title": title, "space": space, "space_name": name, "path": path, "url": url, "snippet": snippet }));
    }
    let text = match results.len() {
        0 => format!("Nothing in the Knowledge Base matches \"{query}\"."),
        1 => format!("The best match for \"{query}\":\n\n{}", lines.join("\n\n")),
        count => format!("{count} best matches for \"{query}\":\n\n{}", lines.join("\n\n")),
    };
    Ok(found(&text, &json!({ "query": query, "results": results })))
}

async fn document(store: &Store<'_>, space: &str, path: &str) -> Result<Value, Refusal> {
    let path = decoded(path);
    let path = path.trim_start_matches('/');
    let found_page = match store.document(space, path).await? {
        Some(page) => Some(page),
        None => store.document(space, &format!("{path}.md")).await?,
    };
    let Some(page) = found_page else {
        return Ok(failed(&format!(
            "{space} has no document {path}; search_docs gives each result's space and path"
        )));
    };
    let base = public_url();
    let title = text_of(&page, "title");
    let mut markdown = text::markdown(&text_of(&page, "html"), &base);
    if !markdown.starts_with("# ") {
        markdown = format!("# {title}\n\n{markdown}");
    }
    let list = |key: &str| -> Vec<String> {
        page[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    };
    let url = format!("{base}{}", page_url(space, &text_of(&page, "path")));
    let source = text_of(&page, "source");
    let mut about = vec![
        format!("Space: {space}"),
        format!("From: {source}"),
        format!("Updated: {}", text_of(&page, "updated_at")),
    ];
    if !list("resources").is_empty() {
        about.push(format!("Documents: {}", list("resources").join(", ")));
    }
    if !list("tags").is_empty() {
        about.push(format!("Labels: {}", list("tags").join(", ")));
    }
    about.push(format!("Link: {url}"));
    if let Some(original) = page["source_url"].as_str() {
        about.push(format!("Original: {original}"));
    }
    let text = format!("{markdown}\n\n---\n{}", about.join("\n"));
    Ok(found(
        &text,
        &json!({
            "space": space,
            "path": page["path"],
            "title": title,
            "url": url,
            "source": source,
            "source_url": page["source_url"],
            "updated_at": page["updated_at"],
            "resources": list("resources"),
            "tags": list("tags"),
            "markdown": markdown,
        }),
    ))
}

async fn spaces(store: &Store<'_>) -> Result<Value, Refusal> {
    let mut kinds: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for source in sources::list(store).await? {
        let listed = kinds.entry(text_of(&source, "space")).or_default();
        let kind = text_of(&source, "kind");
        if !listed.contains(&kind) {
            listed.push(kind);
        }
    }
    let mut lines = Vec::new();
    let mut listed = Vec::new();
    for space in store.spaces().await? {
        let from = kinds.get(&space.key).cloned().unwrap_or_default();
        let about = space
            .resource
            .as_ref()
            .map(|resource| format!(", documents {resource}"))
            .unwrap_or_default();
        let kept = match space.owners.is_empty() {
            true => String::new(),
            false => format!(", kept by {}", space.owners.join(" and ")),
        };
        let count = match space.documents {
            1 => "1 document".to_string(),
            count => format!("{count} documents"),
        };
        lines.push(format!(
            "- {} ({}): {count} from {}{about}{kept}",
            space.name,
            space.key,
            from.join(", ")
        ));
        listed.push(json!({
            "key": space.key,
            "name": space.name,
            "documents": space.documents,
            "sources": from,
            "resource": space.resource,
            "owners": space.owners,
        }));
    }
    let text = match listed.len() {
        0 => "The Knowledge Base has no spaces yet.".to_string(),
        1 => format!("1 space:\n{}", lines.join("\n")),
        count => format!("{count} spaces:\n{}", lines.join("\n")),
    };
    Ok(found(&text, &json!({ "spaces": listed })))
}

async fn documenting(store: &Store<'_>, resource: &str) -> Result<Value, Refusal> {
    let names = space_names(store).await?;
    let base = public_url();
    let mut lines = Vec::new();
    let mut documents = Vec::new();
    for page in store.documenting(resource).await? {
        let space = text_of(&page, "space");
        let path = text_of(&page, "path");
        let title = text_of(&page, "title");
        let source = text_of(&page, "source");
        let url = format!("{base}{}", page_url(&space, &path));
        let name = names.get(&space).cloned().unwrap_or_else(|| space.clone());
        lines.push(format!(
            "- {title} — {name}, from {source} (space: {space}, path: {path})\n  {url}"
        ));
        documents.push(json!({ "title": title, "space": space, "space_name": name, "path": path, "source": source, "url": url }));
    }
    let text = match documents.len() {
        0 => format!("No document names {resource} yet."),
        1 => format!("1 document about {resource}:\n{}", lines.join("\n")),
        count => format!("{count} documents about {resource}:\n{}", lines.join("\n")),
    };
    Ok(found(&text, &json!({ "resource": resource, "documents": documents })))
}
