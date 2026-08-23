//! A repository's files at one commit, through the archive link its source plugin gives: fetched
//! to disk within a size limit, and unpacked with nothing but regular files and directories, none
//! of them outside the directory it is unpacked into.

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use doc_plugin_sdk::Backend;
use doc_plugin_sdk::telemetry::sent;
use futures::StreamExt;
use serde_json::json;
use tokio::io::AsyncWriteExt;

/// The largest archive fetched, and the most its files may come to once unpacked.
const MAX_ARCHIVE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_UNPACKED_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_FILES: usize = 300_000;
const CONNECT: Duration = Duration::from_secs(30);
const READING: Duration = Duration::from_secs(120);
/// The source's setting naming the plugins it gives archive links to.
const ARCHIVE_SETTING: &str = "archive-plugins";
/// How long the source plugin is given to answer for a link.
const ASKING: Duration = Duration::from_secs(20);

/// Where an archive of a repository can be fetched, and where the repository is on the web.
pub struct Link {
    pub url: String,
    pub web: Option<String>,
}

/// What was unpacked, and the commit it is, when the archive says.
pub struct Unpacked {
    pub commit: Option<String>,
    pub files: usize,
}

/// An archive link for `repository` at `reference`, from the plugin that reads it.
pub async fn link(
    backend: &Backend,
    source: &str,
    repository: &str,
    reference: &str,
) -> Result<Link, String> {
    let asked = json!({ "repository": repository, "ref": reference });
    let answered = tokio::time::timeout(
        ASKING,
        backend.discovery(source, "POST", "archive-links", None, Some(asked)),
    )
    .await
    .map_err(|_| format!("{source} did not answer for an archive of {repository} in time"))?
    .map_err(|err| format!("{source} could not be asked for {repository}: {}", err.detail()))?;
    let (status, answer) = answered;
    let detail = answer["detail"].as_str().unwrap_or("it gave no reason");
    match status {
        200 => {}
        403 => return Err(refused(backend, source).await),
        404 => return Err(format!("{source} found no archive of {repository}: {detail}")),
        _ => return Err(format!("{source} would not give an archive of {repository}: {detail}")),
    }
    let url = answer["url"]
        .as_str()
        .filter(|url| url.starts_with("https://") || url.starts_with("http://"));
    let url = url.ok_or_else(|| format!("{source} gave no usable archive link"))?.to_string();
    // `web` ends in /blob/<ref>/; the repository's own page is what is left.
    let web = answer["web"]
        .as_str()
        .filter(|web| web.starts_with("https://") || web.starts_with("http://"))
        .and_then(|web| web.split("/blob/").next())
        .map(|web| web.trim_end_matches('/').to_string());
    Ok(Link { url, web })
}

/// The source will not give this plugin archive links: its administrators are asked to add it,
/// once, and what the scan says is where that stands.
async fn refused(backend: &Backend, source: &str) -> String {
    let reason = "Repository insights fetches each repository through an archive link to scan it \
                  with ccc after a merge into its main branch.";
    let asked = backend.request_access(source, ARCHIVE_SETTING, reason).await;
    let manually =
        format!("add insights to \"Plugins given archive links\" on {source}'s Settings page");
    match asked {
        Ok(answer) if answer.state == "pending" => format!(
            "{source} does not give this plugin archive links yet: whoever administers {source} has \
             been asked to approve it, and scans start again once they do"
        ),
        Ok(answer) if answer.state == "denied" => format!(
            "{source} does not give this plugin archive links: {} denied the request; {manually} to \
             scan anyway",
            answer.decided_by.as_deref().unwrap_or("an administrator")
        ),
        Ok(_) => format!("{source} gives this plugin archive links now; scan again"),
        Err(err) => format!(
            "{source} does not give this plugin archive links: {manually} ({})",
            err.detail()
        ),
    }
}

/// Fetches the archive at `url` into `to`.
pub async fn fetch(url: &str, to: &Path) -> Result<u64, String> {
    let client = reqwest::Client::builder()
        .connect_timeout(CONNECT)
        .read_timeout(READING)
        .user_agent(concat!("doc-insights/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|err| err.to_string())?;
    let answer = client.get(url).send().await;
    sent("github", "archive-download", &answer);
    let answer = answer.map_err(|err| format!("the archive could not be fetched: {err}"))?;
    if !answer.status().is_success() {
        return Err(format!("the archive link answered {}", answer.status()));
    }
    if answer.content_length().is_some_and(|length| length > MAX_ARCHIVE_BYTES) {
        return Err(too_large());
    }
    let mut file = tokio::fs::File::create(to)
        .await
        .map_err(|err| format!("the archive could not be written: {err}"))?;
    let mut stream = answer.bytes_stream();
    let mut total = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|err| format!("the archive could not be read: {err}"))?;
        total += chunk.len() as u64;
        if total > MAX_ARCHIVE_BYTES {
            return Err(too_large());
        }
        file.write_all(&chunk)
            .await
            .map_err(|err| format!("the archive could not be written: {err}"))?;
    }
    file.flush().await.map_err(|err| format!("the archive could not be written: {err}"))?;
    Ok(total)
}

fn too_large() -> String {
    format!("the repository's archive is larger than {} MiB", MAX_ARCHIVE_BYTES / 1024 / 1024)
}

/// Unpacks the `.tar.gz` at `archive` into `into`, without the one top directory GitHub adds.
pub async fn unpack(archive: PathBuf, into: PathBuf) -> Result<Unpacked, String> {
    tokio::task::spawn_blocking(move || unpacked(&archive, &into))
        .await
        .map_err(|err| format!("unpacking stopped: {err}"))?
}

fn unpacked(archive: &Path, into: &Path) -> Result<Unpacked, String> {
    let file = std::fs::File::open(archive).map_err(|err| format!("the archive is gone: {err}"))?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(std::io::BufReader::new(file)));
    std::fs::create_dir_all(into).map_err(|err| format!("{}: {err}", into.display()))?;
    let unreadable = |err: std::io::Error| format!("the archive could not be read: {err}");
    let (mut commit, mut top, mut files, mut total) = (None, None::<String>, 0usize, 0u64);
    for entry in tar.entries().map_err(unreadable)? {
        let mut entry = entry.map_err(unreadable)?;
        let kind = entry.header().entry_type();
        // `git archive` names the commit in a global pax header, as `comment`.
        if kind.is_pax_global_extensions() {
            if let Ok(Some(extensions)) = entry.pax_extensions() {
                for extension in extensions.flatten() {
                    if extension.key() == Ok("comment")
                        && let Ok(value) = extension.value()
                        && is_commit(value.trim())
                    {
                        commit = Some(value.trim().to_string());
                    }
                }
            }
            continue;
        }
        if !(kind.is_file() || kind.is_dir()) {
            continue;
        }
        let path = entry.path().map_err(unreadable)?.into_owned();
        let mut parts = Vec::new();
        for part in path.components() {
            match part {
                Component::Normal(name) => parts.push(name.to_string_lossy().into_owned()),
                Component::CurDir => {}
                _ => {
                    return Err(format!(
                        "the archive names a path outside itself: {}",
                        path.display()
                    ));
                }
            }
        }
        let Some((first, rest)) = parts.split_first() else { continue };
        match &top {
            None => top = Some(first.clone()),
            Some(held) if held != first => {
                return Err("the archive does not hold one top directory, as GitHub's do".into());
            }
            Some(_) => {}
        }
        if rest.is_empty() || rest.iter().any(|part| part == ".git") {
            continue;
        }
        let target = rest.iter().fold(into.to_path_buf(), |path, part| path.join(part));
        if kind.is_dir() {
            std::fs::create_dir_all(&target)
                .map_err(|err| format!("{}: {err}", target.display()))?;
            continue;
        }
        files += 1;
        total += entry.size();
        if files > MAX_FILES || total > MAX_UNPACKED_BYTES {
            return Err(format!(
                "the repository holds more than {MAX_FILES} files or {} GiB",
                MAX_UNPACKED_BYTES / 1024 / 1024 / 1024
            ));
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("{}: {err}", parent.display()))?;
        }
        let mut out =
            std::fs::File::create(&target).map_err(|err| format!("{}: {err}", target.display()))?;
        std::io::copy(&mut entry, &mut out)
            .map_err(|err| format!("{}: {err}", target.display()))?;
    }
    // Without the pax header, the top directory is `owner-name-<short sha>`.
    let commit = commit.or_else(|| {
        top.as_deref()
            .and_then(|top| top.rsplit('-').next())
            .filter(|short| short.len() >= 7 && is_commit(short))
            .map(str::to_string)
    });
    Ok(Unpacked { commit, files })
}

fn is_commit(text: &str) -> bool {
    (7..=40).contains(&text.len()) && text.chars().all(|c| c.is_ascii_hexdigit())
}
