//! What `cli login` saves: the backend URL and a token, in `~/.config/doc/credentials`, which only
//! its owner can read.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

use doc_secret::Secret;
use serde_json::{Value, json};

pub struct Saved {
    pub url: Option<String>,
    pub token: Option<Secret<String>>,
}

fn path() -> Option<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(config.join("doc").join("credentials"))
}

pub fn load() -> Saved {
    let saved: Option<Value> = path()
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str(&text).ok());
    let field = |name: &str| saved.as_ref()?.get(name)?.as_str().map(str::to_string);
    Saved { url: field("url"), token: field("token").map(Secret::new) }
}

pub fn save(url: &str, token: &Secret<String>) -> Result<PathBuf, String> {
    let path = path().ok_or("neither XDG_CONFIG_HOME nor HOME is set")?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
            .map_err(|err| err.to_string())?;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .map_err(|err| format!("{}: {err}", path.display()))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).map_err(|err| err.to_string())?;
    let body = json!({ "url": url, "token": token.expose() });
    writeln!(file, "{body:#}").map_err(|err| format!("{}: {err}", path.display()))?;
    Ok(path)
}

pub fn forget() -> Result<Option<PathBuf>, String> {
    let Some(path) = path() else { return Ok(None) };
    match fs::remove_file(&path) {
        Ok(()) => Ok(Some(path)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!("{}: {err}", path.display())),
    }
}
