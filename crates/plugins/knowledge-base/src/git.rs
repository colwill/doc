//! A git repository uploaded as a `.zip`: its working tree as zipped from a computer, or as a git
//! host or `git archive --format=zip` gives it. What it says of itself is read — the branch and
//! commit from a `.git` directory zipped with it, and its name from `origin` there — and `.git`
//! itself, and what the top `.gitignore` leaves out, are dropped before its pages are read.

use std::collections::BTreeMap;

pub struct Repository {
    pub files: BTreeMap<String, Vec<u8>>,
    /// `owner/name` from its `origin` remote, where the `.git` directory came with it.
    pub name: Option<String>,
    pub branch: Option<String>,
    pub commit: Option<String>,
}

fn text(files: &BTreeMap<String, Vec<u8>>, path: &str) -> Option<String> {
    files.get(path).map(|bytes| String::from_utf8_lossy(bytes).trim().to_string())
}

/// `refs/heads/main`'s commit, loose or packed.
fn resolved(files: &BTreeMap<String, Vec<u8>>, reference: &str) -> Option<String> {
    if let Some(commit) = text(files, &format!(".git/{reference}")) {
        return Some(commit);
    }
    text(files, ".git/packed-refs")?.lines().find_map(|line| {
        let (commit, name) = line.split_once(' ')?;
        (name.trim() == reference).then(|| commit.to_string())
    })
}

/// `owner/name` from a remote URL: `git@github.com:owner/name.git` or `https://host/owner/name`.
fn named(url: &str) -> Option<String> {
    let path = url.trim().trim_end_matches('/').trim_end_matches(".git");
    let path = path.rsplit_once(':').filter(|_| !path.contains("://")).map_or(path, |(_, p)| p);
    let mut parts = path.rsplit('/');
    let name = parts.next().filter(|name| !name.is_empty())?;
    let owner = parts.next().filter(|owner| !owner.is_empty() && !owner.contains('.'))?;
    Some(format!("{owner}/{name}"))
}

fn origin(files: &BTreeMap<String, Vec<u8>>) -> Option<String> {
    let config = text(files, ".git/config")?;
    let mut in_origin = false;
    for line in config.lines().map(str::trim) {
        if line.starts_with('[') {
            in_origin = line == r#"[remote "origin"]"#;
        } else if in_origin && let Some(url) = line.strip_prefix("url") {
            return named(url.trim_start().trim_start_matches('=').trim());
        }
    }
    None
}

/// A pattern of the top `.gitignore`, in the forms that cover nearly every repository: a name or
/// path, `/` anchored, ending in `/` for a directory, and `*` within a name.
struct Ignored {
    pattern: String,
    anchored: bool,
}

fn wildcard(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut at = 0usize;
    for (index, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        let found = match index {
            0 => name.starts_with(part).then_some(0),
            _ => name[at..].find(part).map(|found| at + found),
        };
        let Some(found) = found else { return false };
        at = found + part.len();
    }
    parts.last().is_none_or(|last| last.is_empty() || at == name.len())
}

impl Ignored {
    fn read(line: &str) -> Option<Self> {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
            return None;
        }
        let anchored = line.starts_with('/') || line.trim_end_matches('/').contains('/');
        let pattern = line.trim_start_matches('/').trim_end_matches('/').to_string();
        (!pattern.is_empty()).then_some(Self { pattern, anchored })
    }

    /// Whether `path`, or a directory it is in, matches.
    fn covers(&self, path: &str) -> bool {
        let parts: Vec<&str> = path.split('/').collect();
        match self.anchored {
            true => (1..=parts.len()).any(|end| wildcard(&self.pattern, &parts[..end].join("/"))),
            false => parts.iter().any(|part| wildcard(&self.pattern, part)),
        }
    }
}

pub fn read(mut files: BTreeMap<String, Vec<u8>>) -> Repository {
    let head = text(&files, ".git/HEAD");
    let branch =
        head.as_deref().and_then(|head| head.strip_prefix("ref: refs/heads/")).map(str::to_string);
    let commit = match head.as_deref() {
        Some(head) if head.starts_with("ref: ") => {
            resolved(&files, head.trim_start_matches("ref: "))
        }
        Some(head) if head.len() == 40 => Some(head.to_string()),
        _ => None,
    };
    let name = origin(&files);
    let ignored: Vec<Ignored> = text(&files, ".gitignore")
        .map(|gitignore| gitignore.lines().filter_map(Ignored::read).collect())
        .unwrap_or_default();
    files.retain(|path, _| {
        !path.starts_with(".git/") && !ignored.iter().any(|ignored| ignored.covers(path))
    });
    Repository { files, name, branch, commit }
}

/// What an upload of a repository started.
pub struct Imported {
    pub pages: usize,
    pub repository: String,
    pub space: String,
}

/// Imports a zipped repository into `space` — or a space named for the repository when that is
/// empty — through the space's source for it, which says where the repository was.
pub async fn import(
    backend: &doc_plugin_sdk::Backend,
    store: &crate::store::Store<'_>,
    files: BTreeMap<String, Vec<u8>>,
    space: &str,
    resource: Option<&str>,
    owners: &[String],
) -> Result<Imported, crate::Refusal> {
    use serde_json::json;

    let repository = read(files);
    let key = match space.trim() {
        "" => crate::imports::slug(repository.name.as_deref().unwrap_or("repository")),
        space => crate::imports::slug(space),
    };
    let named = repository.name.clone().unwrap_or_else(|| key.clone());
    let resource = resource.map(crate::imports::checked_resource).transpose()?;
    let owners = crate::owners::checked(owners)?;
    let existing = store.space(&key).await?;
    // The source needs its space first; a new one still has to say who looks after it.
    if existing.is_none() {
        if owners.is_empty() {
            return Err(crate::Refusal::bad(crate::owners::NEEDED));
        }
        let space = store.ensure_space(&key, &named, resource.as_deref(), &[]).await?;
        crate::owners::set(backend, store, &space, &owners).await?;
    }
    let settings = json!({
        "repository": named,
        "branch": repository.branch,
        "commit": repository.commit,
        "uploaded_by": backend.caller().and_then(|caller| caller.label.clone()),
        "uploaded_at": chrono::Utc::now().to_rfc3339(),
    });
    let source = store.git_source(&key, &settings).await?;
    let origin = crate::imports::Origin { source, web: None };
    let started = crate::imports::start(
        backend,
        store,
        &repository.files,
        Some(&key),
        resource.as_deref(),
        &owners,
        Some(origin),
    )
    .await?;
    store.synced(source, "succeeded", None).await?;
    crate::sources::announce(backend, store).await;
    Ok(Imported { pages: started.pages, repository: named, space: started.space.key })
}
