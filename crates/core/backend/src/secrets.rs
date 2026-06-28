//! Reading and writing the secrets volume: token generation, file modes and ownership.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use anyhow::{Context, Result};
use doc_secret::Secret;
use serde::{Deserialize, Serialize};

const TOKEN_BYTES: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TokenKind {
    Session,
    Personal,
    Service,
    PluginRegistration,
    Operator,
    Bus,
    /// A user's, limited to named permissions for minutes (FEAT-VACUUM).
    Scoped,
}

impl TokenKind {
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Session => "doc_ses_",
            Self::Personal => "doc_pat_",
            Self::Service => "doc_svc_",
            Self::PluginRegistration => "doc_reg_",
            Self::Operator => "doc_ops_",
            Self::Bus => "doc_bus_",
            Self::Scoped => "doc_scp_",
        }
    }

    /// The value stored in `core.api_tokens.kind`; bus tokens live only in the secrets volume.
    pub fn stored_as(self) -> Option<&'static str> {
        Some(match self {
            Self::Session => "session",
            Self::Personal => "personal",
            Self::Service => "service",
            Self::PluginRegistration => "plugin-registration",
            Self::Operator => "operator",
            Self::Scoped => "scoped",
            Self::Bus => return None,
        })
    }

    pub fn of(token: &str) -> Option<Self> {
        [
            Self::Session,
            Self::Personal,
            Self::Service,
            Self::PluginRegistration,
            Self::Operator,
            Self::Scoped,
        ]
        .into_iter()
        .find(|kind| token.starts_with(kind.prefix()))
    }
}

pub fn generate_token(kind: TokenKind) -> Result<Secret<String>> {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes).context("reading random bytes for a token")?;
    Ok(Secret::new(format!("{}{}", kind.prefix(), base64_url(&bytes))))
}

/// A password that works once, easy to read out and type: 20 characters with no 0, O, 1, I or L,
/// in groups of four.
pub fn one_time_password() -> Result<Secret<String>> {
    const ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";
    let mut bytes = [0u8; 20];
    getrandom::fill(&mut bytes).context("reading random bytes for a one-time password")?;
    let characters: Vec<char> = bytes
        .iter()
        .map(|byte| char::from(ALPHABET[usize::from(*byte) % ALPHABET.len()]))
        .collect();
    let groups: Vec<String> = characters.chunks(4).map(|group| group.iter().collect()).collect();
    Ok(Secret::new(groups.join("-")))
}

/// A one-use ticket for linking an account (ADR-0005). It is not a token and never authenticates.
pub fn link_ticket() -> Result<Secret<String>> {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes).context("reading random bytes for a link ticket")?;
    Ok(Secret::new(format!("doc_lnk_{}", base64_url(&bytes))))
}

/// Where the key that encrypts stored secrets lives, beside the certificate authority (ADR-0007).
/// It must be backed up with the rest of the volume: secrets encrypted under a lost key are gone.
pub const SETTINGS_KEY_DIR: &str = "keys";
const SETTINGS_KEY_BYTES: usize = 32;
const NONCE_BYTES: usize = 12;

/// One key that stored secrets are encrypted under, and the ID recorded beside each ciphertext so
/// a value knows which key opens it. Rotating writes a new key and re-encrypts under it.
#[derive(Clone)]
pub struct SettingsKey {
    pub id: String,
    key: Secret<[u8; SETTINGS_KEY_BYTES]>,
}

impl std::fmt::Debug for SettingsKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingsKey").field("id", &self.id).finish_non_exhaustive()
    }
}

/// A secret as the database holds it: never the value, only what a key can turn back into one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sealed {
    pub key_id: String,
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum SealError {
    #[error("the settings key is not available: {0}")]
    NoKey(String),
    #[error("this secret was encrypted under key {0}, which is not loaded; set it again")]
    UnknownKey(String),
    #[error("this secret could not be decrypted")]
    Undecipherable,
    #[error("this secret could not be encrypted")]
    Unsealable,
}

/// Every key the platform can open a secret with: the current one, which new secrets are sealed
/// under, and any older ones kept while a rotation finishes.
#[derive(Clone, Debug, Default)]
pub struct SettingsKeys {
    keys: Vec<SettingsKey>,
}

impl SettingsKeys {
    /// Reads `keys/settings*.key` from the secrets volume, newest name last. Missing keys are not
    /// an error here: a platform that never stored a secret has nothing to open.
    pub fn load(dir: &Path) -> Result<Self> {
        let keys_dir = dir.join(SETTINGS_KEY_DIR);
        let paths: Vec<_> = match fs::read_dir(&keys_dir) {
            Ok(entries) => entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| settings_key_id(path).is_some())
                .collect(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(err) => return Err(err).context(format!("reading {}", keys_dir.display())),
        };
        let mut keys = Vec::new();
        for path in paths {
            let id = settings_key_id(&path).unwrap_or_default();
            keys.push(SettingsKey { id, key: read_settings_key(&path)? });
        }
        // Highest ID first: the current key is the last one written, and a file name sorts
        // `settings-2.key` before `settings.key`, so the number decides rather than the name.
        keys.sort_by_key(|key| std::cmp::Reverse(key.id.parse::<u32>().unwrap_or_default()));
        Ok(Self { keys })
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// A key that lives only as long as the process, for a test that has no secrets volume.
    #[cfg(test)]
    pub fn in_memory() -> Self {
        let mut bytes = [0u8; SETTINGS_KEY_BYTES];
        getrandom::fill(&mut bytes).expect("random bytes");
        Self { keys: vec![SettingsKey { id: "1".into(), key: Secret::new(bytes) }] }
    }

    pub fn current(&self) -> Option<&SettingsKey> {
        self.keys.first()
    }

    fn by_id(&self, id: &str) -> Option<&SettingsKey> {
        self.keys.iter().find(|key| key.id == id)
    }

    /// Encrypts under the current key, binding the plugin and the setting's key in as associated
    /// data so a ciphertext cannot be moved to another plugin or another setting.
    pub fn seal(&self, plugin: &str, setting: &str, value: &str) -> Result<Sealed, SealError> {
        let key = self
            .current()
            .ok_or_else(|| SealError::NoKey("no key has been created yet".to_string()))?;
        let mut nonce = [0u8; NONCE_BYTES];
        getrandom::fill(&mut nonce).map_err(|_| SealError::Unsealable)?;
        let sealing = sealing_key(key)?;
        let mut buffer = value.as_bytes().to_vec();
        sealing
            .seal_in_place_append_tag(
                ring::aead::Nonce::assume_unique_for_key(nonce),
                associated(plugin, setting),
                &mut buffer,
            )
            .map_err(|_| SealError::Unsealable)?;
        Ok(Sealed { key_id: key.id.clone(), nonce: nonce.to_vec(), ciphertext: buffer })
    }

    pub fn open(&self, plugin: &str, setting: &str, sealed: &Sealed) -> Result<String, SealError> {
        let key = self
            .by_id(&sealed.key_id)
            .ok_or_else(|| SealError::UnknownKey(sealed.key_id.clone()))?;
        let nonce: [u8; NONCE_BYTES] =
            sealed.nonce.clone().try_into().map_err(|_| SealError::Undecipherable)?;
        let opening = sealing_key(key)?;
        let mut buffer = sealed.ciphertext.clone();
        let opened = opening
            .open_in_place(
                ring::aead::Nonce::assume_unique_for_key(nonce),
                associated(plugin, setting),
                &mut buffer,
            )
            .map_err(|_| SealError::Undecipherable)?;
        String::from_utf8(opened.to_vec()).map_err(|_| SealError::Undecipherable)
    }
}

/// The plugin and the setting's key, bound into the ciphertext: opening it anywhere else fails.
fn associated(plugin: &str, setting: &str) -> ring::aead::Aad<Vec<u8>> {
    ring::aead::Aad::from(format!("{plugin}\u{0}{setting}").into_bytes())
}

fn sealing_key(key: &SettingsKey) -> Result<ring::aead::LessSafeKey, SealError> {
    let unbound = ring::aead::UnboundKey::new(&ring::aead::AES_256_GCM, key.key.expose())
        .map_err(|_| SealError::Unsealable)?;
    Ok(ring::aead::LessSafeKey::new(unbound))
}

/// `settings.key` is key `1`, and a rotation writes `settings-2.key`, `settings-3.key` and so on.
fn settings_key_id(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_suffix(".key")?;
    match stem {
        "settings" => Some("1".to_string()),
        stem => stem
            .strip_prefix("settings-")
            .filter(|id| id.chars().all(|c| c.is_ascii_digit()))
            .map(str::to_string),
    }
}

fn settings_key_name(id: &str) -> String {
    match id {
        "1" => format!("{SETTINGS_KEY_DIR}/settings.key"),
        id => format!("{SETTINGS_KEY_DIR}/settings-{id}.key"),
    }
}

fn read_settings_key(path: &Path) -> Result<Secret<[u8; SETTINGS_KEY_BYTES]>> {
    use base64::Engine;
    let text = read_trimmed(path)?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(text.as_bytes())
        .with_context(|| format!("{} is not a settings key", path.display()))?;
    let bytes: [u8; SETTINGS_KEY_BYTES] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("{} is not {SETTINGS_KEY_BYTES} bytes", path.display()))?;
    Ok(Secret::new(bytes))
}

/// The keys as the platform holds them while it serves: read from the volume at start-up, and
/// read again when a value turns up under a key this process has not seen — which is what
/// `rotate-settings-key`, running beside a serving backend, leaves behind.
pub struct SettingsKeyring {
    dir: std::path::PathBuf,
    keys: parking_lot::RwLock<SettingsKeys>,
}

impl SettingsKeyring {
    pub fn load(dir: &Path) -> Result<Self> {
        Ok(Self {
            dir: dir.to_path_buf(),
            keys: parking_lot::RwLock::new(SettingsKeys::load(dir)?),
        })
    }

    /// A keyring with one key that lives only in this process, for a test with no volume.
    #[cfg(test)]
    pub fn in_memory() -> Self {
        Self {
            dir: std::path::PathBuf::new(),
            keys: parking_lot::RwLock::new(SettingsKeys::in_memory()),
        }
    }

    /// An empty keyring: a platform whose volume has no key yet, which stores no secret rather
    /// than storing one in the clear.
    pub fn empty() -> Self {
        Self {
            dir: std::path::PathBuf::new(),
            keys: parking_lot::RwLock::new(SettingsKeys::default()),
        }
    }

    fn reload(&self) {
        if self.dir.as_os_str().is_empty() {
            return;
        }
        match SettingsKeys::load(&self.dir) {
            Ok(keys) => *self.keys.write() = keys,
            Err(err) => tracing::warn!(%err, "the settings keys could not be read again"),
        }
    }

    /// Whether a secret can be stored at all, which is what the page says when no key exists.
    pub fn available(&self) -> bool {
        if !self.keys.read().is_empty() {
            return true;
        }
        self.reload();
        !self.keys.read().is_empty()
    }

    /// The key new values are sealed under, read from the volume again so a rotation that ran
    /// beside this process counts: a value under any other key is due to be sealed again.
    pub fn current_id(&self) -> Option<String> {
        self.reload();
        self.keys.read().current().map(|key| key.id.clone())
    }

    /// Seals under the newest key on disk, which is the one a rotation has just written.
    pub fn seal(&self, plugin: &str, setting: &str, value: &str) -> Result<Sealed, SealError> {
        self.reload();
        self.keys.read().seal(plugin, setting, value)
    }

    /// Opens with the key the value names, reading the volume again for a key this process has
    /// not seen before giving up on it.
    pub fn open(&self, plugin: &str, setting: &str, sealed: &Sealed) -> Result<String, SealError> {
        match self.keys.read().open(plugin, setting, sealed) {
            Err(SealError::UnknownKey(_)) => {}
            answered => return answered,
        }
        self.reload();
        self.keys.read().open(plugin, setting, sealed)
    }
}

/// Writes a new settings key with the next free ID and returns it. The bootstrap calls this with
/// no key present; `rotate-settings-key` calls it beside the keys already there.
pub fn create_settings_key(dir: &Path) -> Result<String> {
    use base64::Engine;
    let existing = SettingsKeys::load(dir)?;
    let next = existing
        .keys
        .iter()
        .filter_map(|key| key.id.parse::<u32>().ok())
        .max()
        .map_or(1, |highest| highest + 1);
    let mut bytes = [0u8; SETTINGS_KEY_BYTES];
    getrandom::fill(&mut bytes).context("reading random bytes for the settings key")?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    let id = next.to_string();
    write(&dir.join(settings_key_name(&id)), encoded.as_bytes(), 0o600)?;
    Ok(id)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}

pub fn write(path: &Path, contents: &[u8], mode: u32) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(path)
        .with_context(|| format!("writing {}", path.display()))?;
    file.write_all(contents)?;
    file.sync_all()?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

pub fn read_trimmed(path: &Path) -> Result<String> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(text.trim().to_string())
}

/// Hands the whole tree to the uid/gid the services run as, since the bootstrap job runs as root.
pub fn own_tree(dir: &Path, uid: Option<u32>, gid: Option<u32>) -> Result<()> {
    if uid.is_none() && gid.is_none() {
        return Ok(());
    }
    let mut stack = vec![dir.to_path_buf()];
    while let Some(path) = stack.pop() {
        std::os::unix::fs::chown(&path, uid, gid)
            .with_context(|| format!("changing owner of {}", path.display()))?;
        if path.is_dir() {
            for entry in fs::read_dir(&path)? {
                stack.push(entry?.path());
            }
        }
    }
    Ok(())
}

fn base64_url(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_carry_their_kind_and_differ() {
        let a = generate_token(TokenKind::Operator).expect("token").expose().clone();
        let b = generate_token(TokenKind::Operator).expect("token").expose().clone();
        assert_ne!(a, b);
        assert_eq!(TokenKind::of(&a), Some(TokenKind::Operator));
        assert_eq!(TokenKind::of("nonsense"), None);
        assert_eq!(a.len(), TokenKind::Operator.prefix().len() + 43);
    }

    /// The settings key, the ciphertext it makes, and what a second key does to both: a value
    /// says which key sealed it, and the newest key is the one new values are sealed under — which
    /// a file name alone gets wrong, since `settings-2.key` sorts before `settings.key`.
    #[test]
    fn secrets_are_sealed_to_their_plugin_and_setting_under_the_newest_key() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert!(SettingsKeys::load(dir.path()).expect("no keys yet").is_empty());

        assert_eq!(create_settings_key(dir.path()).expect("a key"), "1");
        let keys = SettingsKeys::load(dir.path()).expect("keys");
        let sealed = keys.seal("github", "client-secret", "s3cret").expect("sealed");
        assert_eq!(sealed.key_id, "1");
        assert!(!sealed.ciphertext.windows(6).any(|window| window == b"s3cret"), "ciphertext");
        assert_eq!(keys.open("github", "client-secret", &sealed).expect("opened"), "s3cret");

        // The plugin and the key are bound in, so a ciphertext cannot be moved to either.
        assert!(matches!(
            keys.open("ghe", "client-secret", &sealed),
            Err(SealError::Undecipherable)
        ));
        assert!(matches!(keys.open("github", "token", &sealed), Err(SealError::Undecipherable)));

        // A second key: new values are sealed under it, and the old ones still open.
        assert_eq!(create_settings_key(dir.path()).expect("a second key"), "2");
        let rotated = SettingsKeys::load(dir.path()).expect("keys");
        assert_eq!(rotated.current().map(|key| key.id.as_str()), Some("2"), "the newest key");
        let again = rotated.seal("github", "client-secret", "s3cret").expect("sealed");
        assert_eq!(again.key_id, "2");
        assert_eq!(rotated.open("github", "client-secret", &sealed).expect("still"), "s3cret");

        // A value sealed under a key this platform does not have says so, rather than failing
        // as though the value were corrupt: the page asks for it to be set again.
        let elsewhere = Sealed { key_id: "9".into(), ..again };
        assert!(matches!(
            rotated.open("github", "client-secret", &elsewhere),
            Err(SealError::UnknownKey(id)) if id == "9"
        ));
    }

    #[test]
    fn secret_files_are_written_with_their_mode() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("nested/secret.token");
        write(&path, b"value", 0o600).expect("write");
        let mode = fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(read_trimmed(&path).expect("read"), "value");
    }
}
