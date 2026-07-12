//! Reading an uploaded or downloaded `.tar.gz` or `.zip`: regular files only, nothing that climbs
//! out of the archive, within limits, and without the single top directory GitHub's archives, and
//! `git archive` with a prefix, put everything in.

use std::collections::BTreeMap;
use std::io::Read;

const MAX_FILES: usize = 5_000;
const MAX_BYTES: u64 = 64 * 1024 * 1024;

pub fn files(gzipped: &[u8]) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(gzipped));
    let entries =
        archive.entries().map_err(|err| format!("the archive could not be read: {err}"))?;
    let (mut files, mut total) = (BTreeMap::new(), 0u64);
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
        total += entry.size();
        if files.len() >= MAX_FILES || total > MAX_BYTES {
            return Err(format!(
                "an archive holds at most {MAX_FILES} files and {} MiB",
                MAX_BYTES / 1024 / 1024
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

/// A `.zip` or a `.tar.gz`, told apart by how it starts.
pub fn read(bytes: &[u8]) -> Result<BTreeMap<String, Vec<u8>>, String> {
    match bytes.starts_with(b"PK\x03\x04") || bytes.starts_with(b"PK\x05\x06") {
        true => zipped(bytes),
        false => files(bytes),
    }
}

pub fn zipped(bytes: &[u8]) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|err| format!("the zip could not be read: {err}"))?;
    let (mut files, mut total) = (BTreeMap::new(), 0u64);
    for index in 0..archive.len() {
        let mut entry =
            archive.by_index(index).map_err(|err| format!("the zip could not be read: {err}"))?;
        if !entry.is_file() {
            continue;
        }
        // `enclosed_name` is None for a name that is absolute or climbs out with `..`.
        let Some(path) = entry.enclosed_name() else { continue };
        let parts: Vec<String> = path
            .components()
            .filter_map(|part| match part {
                std::path::Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect();
        if parts.is_empty() {
            continue;
        }
        total += entry.size();
        if files.len() >= MAX_FILES || total > MAX_BYTES {
            return Err(format!(
                "an archive holds at most {MAX_FILES} files and {} MiB",
                MAX_BYTES / 1024 / 1024
            ));
        }
        let mut bytes = Vec::new();
        // Read no more than it said it holds, however it was made.
        (&mut entry)
            .take(MAX_BYTES)
            .read_to_end(&mut bytes)
            .map_err(|err| format!("the zip could not be read: {err}"))?;
        files.insert(parts.join("/"), bytes);
    }
    Ok(without_top(files))
}

/// Everything in one top directory, as `git archive` and GitHub's tarballs have it, is lifted out.
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
