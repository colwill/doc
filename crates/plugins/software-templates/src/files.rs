//! The files a template makes: a skeleton read from a repository, the files the template carries
//! itself, and every one of them rendered against the answers. Text is rendered; anything that is
//! not UTF-8, such as an image, is carried across untouched.

use std::collections::BTreeMap;
use std::io::Read;
use std::time::Duration;

use doc_plugin_sdk::Backend;
use doc_plugin_sdk::telemetry::sent;
use serde_json::{Value, json};

use crate::model::{Definition, MAX_FILES, MAX_TOTAL_BYTES, safe_path};
use crate::{render, scaffolds};

const DOWNLOAD: Duration = Duration::from_secs(30);
const MAX_ARCHIVE_BYTES: u64 = 32 * 1024 * 1024;

/// One file as it will be committed.
#[derive(Debug, Clone)]
pub struct Rendered {
    pub path: String,
    /// The file's text, or `None` for one that is not text and was carried across as it was.
    pub text: Option<String>,
    pub bytes: Vec<u8>,
}

impl Rendered {
    pub fn size(&self) -> usize {
        self.bytes.len()
    }

    /// A short piece of the file for the preview, so a page never carries a whole repository.
    pub fn preview(&self, lines: usize) -> String {
        match &self.text {
            None => format!("({} bytes, not text)", self.bytes.len()),
            Some(text) => {
                let shown: Vec<&str> = text.lines().take(lines).collect();
                let rest = text.lines().count().saturating_sub(shown.len());
                match rest {
                    0 => shown.join("\n"),
                    rest => format!("{}\n… {rest} more lines", shown.join("\n")),
                }
            }
        }
    }
}

/// Everything the template makes, rendered against `context`: the skeleton first, then the files
/// the template carries, which replace a skeleton file of the same path.
pub async fn assemble(
    backend: &Backend,
    definition: &Definition,
    context: &Value,
) -> Result<Vec<Rendered>, String> {
    let mut files: BTreeMap<String, Rendered> = BTreeMap::new();
    if let Some(scaffold) = &definition.scaffold {
        let language = render::line(&scaffold.language, context)
            .map_err(|err| format!("the language: {err}"))?;
        let app = render::line(&scaffold.app, context)
            .map_err(|err| format!("the application: {err}"))?;
        if !scaffolds::holds(&language, &app) {
            return Err(format!("there is no {app} scaffold for {language}"));
        }
        let name = service_name(context);
        let context = &with_scaffold(context, &name, &language, &app);
        let package = context["scaffold"]["package"].as_str().unwrap_or(&name).to_string();
        for (path, content) in scaffolds::files(&language, &app) {
            let path = scaffolds::named(&path, &name, &package);
            let rendered = one(&path, content.as_bytes().to_vec(), context)?;
            files.insert(rendered.path.clone(), rendered);
        }
    }
    if let Some(source) = &definition.source {
        let skeleton = skeleton(backend, &source.repository, source.reference.as_deref()).await?;
        let path = source.path.as_deref().unwrap_or_default();
        for (name, bytes) in under(skeleton, path) {
            let rendered = one(&name, bytes, context)?;
            files.insert(rendered.path.clone(), rendered);
        }
    }
    for file in &definition.files {
        let path = render::line(&file.path, context)
            .map_err(|err| format!("the path `{}`: {err}", file.path))?;
        let text =
            render::render(&file.content, context).map_err(|err| format!("`{path}`: {err}"))?;
        safe_path(&path)?;
        files.insert(path.clone(), Rendered::of_text(path, text));
    }
    if files.len() > MAX_FILES {
        return Err(format!("a template makes at most {MAX_FILES} files"));
    }
    let total: usize = files.values().map(Rendered::size).sum();
    if total > MAX_TOTAL_BYTES {
        return Err(format!(
            "what it makes comes to more than {} MiB",
            MAX_TOTAL_BYTES / 1024 / 1024
        ));
    }
    Ok(files.into_values().collect())
}

impl Rendered {
    /// A file the template carried itself, which is text by definition.
    fn of_text(path: String, text: String) -> Self {
        Self { path, bytes: text.clone().into_bytes(), text: Some(text) }
    }
}

/// What a scaffold is given beyond the answers: the names it writes into packages, modules and
/// Kubernetes kinds, worked out once here so that five languages do not each do it differently.
fn with_scaffold(context: &Value, name: &str, language: &str, app: &str) -> Value {
    let filtered = |filter: &str| {
        render::render(&format!("{{{{ name | {filter} }}}}"), &json!({ "name": name }))
            .unwrap_or_else(|_| name.to_string())
    };
    let answered = |field: &str| {
        render::lookup(context, &format!("values.{field}"))
            .as_str()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    };
    let kind = answered("resource")
        .map(|resource| {
            render::render("{{ resource | pascal }}", &json!({ "resource": resource }))
                .unwrap_or(resource)
        })
        .unwrap_or_else(|| filtered("pascal"));
    let module = match answered("module") {
        Some(module) => format!("{}/{name}", module.trim_end_matches('/')),
        None => format!("example.com/{name}"),
    };
    let mut with = context.clone();
    with["scaffold"] = json!({
        "name": name,
        "package": filtered("snake"),
        "pascal": filtered("pascal"),
        "language": language,
        "app": app,
        "module": module,
        "group": answered("group").unwrap_or_else(|| "example.com".to_string()),
        "noun": match app {
            "cli" => "tool",
            "doc-plugin" | "doc-plugin-tutorial" => "plugin",
            _ => "service",
        },
        "plural": format!("{}s", kind.to_lowercase()),
        "kind": kind,
    });
    with
}

/// What the service is called, which is what a scaffold's paths and packages are named after.
fn service_name(context: &Value) -> String {
    let named = render::lookup(context, "values.name");
    named
        .as_str()
        .map(str::to_string)
        .filter(|name| !name.trim().is_empty())
        .or_else(|| render::lookup(context, "run.template").as_str().map(str::to_string))
        .unwrap_or_else(|| "service".to_string())
}

/// One file from the skeleton: its path and, when it is text, its contents are rendered.
fn one(path: &str, bytes: Vec<u8>, context: &Value) -> Result<Rendered, String> {
    let path = render::line(path, context).map_err(|err| format!("the path `{path}`: {err}"))?;
    safe_path(&path)?;
    match String::from_utf8(bytes) {
        Ok(text) => {
            let rendered =
                render::render(&text, context).map_err(|err| format!("`{path}`: {err}"))?;
            Ok(Rendered { path, bytes: rendered.clone().into_bytes(), text: Some(rendered) })
        }
        Err(err) => Ok(Rendered { path, bytes: err.into_bytes(), text: None }),
    }
}

/// The repository's files, through the GitHub plugin's archive link. The plugin must be one of
/// those the GitHub plugin is configured to give links to.
async fn skeleton(
    backend: &Backend,
    repository: &str,
    reference: Option<&str>,
) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let asked = json!({ "repository": repository, "ref": reference.unwrap_or("main") });
    let (status, answer) = backend
        .discovery("github", "POST", "archive-links", None, Some(asked))
        .await
        .map_err(|err| {
            format!("the GitHub plugin could not be asked for {repository}: {}", err.detail())
        })?;
    if status != 200 {
        let detail = answer["detail"].as_str().unwrap_or("it gave no reason");
        return Err(format!("GitHub would not give an archive of {repository}: {detail}"));
    }
    let url = answer["url"].as_str().ok_or("the archive link was empty")?;
    files(&download(url).await?)
}

async fn download(url: &str) -> Result<Vec<u8>, String> {
    let client = reqwest::Client::builder()
        .timeout(DOWNLOAD)
        .user_agent(concat!("doc-templates/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|err| err.to_string())?;
    let answer = client.get(url).send().await;
    sent("github", "download", &answer);
    let answer = answer.map_err(|err| format!("the skeleton could not be fetched: {err}"))?;
    if !answer.status().is_success() {
        return Err(format!("the archive link answered {}", answer.status()));
    }
    if answer.content_length().is_some_and(|length| length > MAX_ARCHIVE_BYTES) {
        return Err("the skeleton is too large".into());
    }
    let bytes =
        answer.bytes().await.map_err(|err| format!("the skeleton could not be read: {err}"))?;
    Ok(bytes.to_vec())
}

/// The regular files in a `.tar.gz`, without the single top directory GitHub's archives add, and
/// with nothing that climbs out of the archive.
fn files(gzipped: &[u8]) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(gzipped));
    let entries =
        archive.entries().map_err(|err| format!("the archive could not be read: {err}"))?;
    let (mut files, mut total) = (BTreeMap::new(), 0usize);
    for entry in entries {
        let mut entry = entry.map_err(|err| format!("the archive could not be read: {err}"))?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path =
            entry.path().map_err(|err| format!("a name in the archive is unreadable: {err}"))?;
        let parts: Vec<String> = path
            .components()
            .filter_map(|part| match part {
                std::path::Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect();
        if parts.is_empty()
            || path.components().any(|part| matches!(part, std::path::Component::ParentDir))
        {
            continue;
        }
        total += usize::try_from(entry.size()).unwrap_or(usize::MAX);
        if files.len() > MAX_FILES || total > MAX_TOTAL_BYTES {
            return Err(format!(
                "a skeleton holds at most {MAX_FILES} files and {} MiB",
                MAX_TOTAL_BYTES / 1024 / 1024
            ));
        }
        let mut bytes = Vec::new();
        entry
            .read_to_end(&mut bytes)
            .map_err(|err| format!("the archive could not be read: {err}"))?;
        files.insert(parts.join("/"), bytes);
    }
    Ok(without_top(files))
}

/// Everything in one top directory, as GitHub's tarballs have it, is lifted out of it.
fn without_top(files: BTreeMap<String, Vec<u8>>) -> BTreeMap<String, Vec<u8>> {
    let tops: std::collections::BTreeSet<&str> =
        files.keys().map(|path| path.split('/').next().unwrap_or_default()).collect();
    let nested = files.keys().all(|path| path.contains('/'));
    match (tops.len(), nested) {
        (1, true) => files
            .into_iter()
            .map(|(path, bytes)| {
                (path.split_once('/').map(|(_, rest)| rest.to_string()).unwrap_or(path), bytes)
            })
            .collect(),
        _ => files,
    }
}

/// Only what lies under `path`, as if it were the whole repository.
fn under(files: BTreeMap<String, Vec<u8>>, path: &str) -> BTreeMap<String, Vec<u8>> {
    let prefix = path.trim_matches('/');
    if prefix.is_empty() {
        return files;
    }
    files
        .into_iter()
        .filter_map(|(name, bytes)| {
            name.strip_prefix(&format!("{prefix}/")).map(|rest| (rest.to_string(), bytes))
        })
        .collect()
}
