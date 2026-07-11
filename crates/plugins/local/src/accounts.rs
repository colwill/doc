//! The accounts DOC keeps itself: a username, the person's name and email address, and a password,
//! stored only as an Argon2 hash. An admin gives each account a one-time password, which signs in
//! once and is then swapped for the person's own (ADR-0005).
//!
//! An account is never renamed or deleted, only disabled, so its username, which is the account's
//! ID in core, never passes to someone else.

use std::sync::OnceLock;
use std::time::Duration;

use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use chrono::{DateTime, Utc};
use doc_plugin_sdk::{Backend, Collection, Declaration, Field, PluginError, Query};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::Digest;

pub const COLLECTION: &str = "accounts";
pub const MAX_USERNAME: usize = 64;
pub const MIN_PASSWORD: usize = 12;
pub const MAX_PASSWORD: usize = 256;
const MAX_NAME: usize = 100;
const MAX_EMAIL: usize = 256;
/// How long a one-time password an admin gives works for. The first sign-in's has no limit.
pub const ONE_TIME_DAYS: i64 = 7;
/// Refused sign-ins to one username before it rests.
const ATTEMPTS: u64 = 5;
const REST: Duration = Duration::from_secs(15 * 60);
/// How long someone has to choose their password after signing in with a one-time one.
const TICKET: Duration = Duration::from_secs(10 * 60);

pub fn declaration() -> Declaration {
    Declaration::default().collection(
        COLLECTION,
        Collection::new()
            .field("id", Field::text().key().describe("The username in lower case"))
            .field("username", Field::text().required().describe("As it was made: its ID in core"))
            .field("first_name", Field::text().required().default(json!("")))
            .field("surname", Field::text().required().default(json!("")))
            .field("email", Field::text().required().default(json!("")))
            .field("password_hash", Field::text().describe("Argon2; none until it is chosen"))
            .field("one_time_hash", Field::text().describe("Argon2 of a one-time password"))
            .field("one_time_expires_at", Field::timestamp().describe("None: until it is used"))
            .field("disabled", Field::boolean().required().default(json!(false)))
            .field("signed_in_at", Field::timestamp())
            .field("created_at", Field::timestamp().required().default(json!("now"))),
    )
}

/// An account as stored.
#[derive(Debug, Clone, Deserialize)]
pub struct Account {
    pub id: String,
    pub username: String,
    #[serde(default)]
    pub first_name: String,
    #[serde(default)]
    pub surname: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub password_hash: Option<String>,
    #[serde(default)]
    pub one_time_hash: Option<String>,
    #[serde(default)]
    pub one_time_expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub signed_in_at: Option<DateTime<Utc>>,
}

impl Account {
    /// The name to show, from what is known of it.
    pub fn name(&self) -> Option<String> {
        let name = format!("{} {}", self.first_name, self.surname).trim().to_string();
        (!name.is_empty()).then_some(name)
    }

    /// Where it is: disabled, waiting for its first sign-in, waiting for a one-time password, or
    /// in use.
    pub fn state(&self) -> &'static str {
        match (self.disabled, &self.password_hash, &self.one_time_hash) {
            (true, _, _) => "Disabled",
            (false, _, Some(_)) => "One-time password given",
            (false, None, None) => "No password yet",
            (false, Some(_), None) => "Active",
        }
    }

    fn one_time_expired(&self, now: DateTime<Utc>) -> bool {
        self.one_time_expires_at.is_some_and(|expiry| expiry <= now)
    }
}

/// What someone entered, and whether it was right.
pub enum Checked {
    /// Their own password: they are signed in.
    Password(Account),
    /// A one-time password: they choose their own before going further.
    OneTime(Account),
    Wrong,
    Disabled,
    Resting,
}

/// A refusal, as the routes answer it.
#[derive(Debug)]
pub struct Refusal {
    pub status: u16,
    pub detail: String,
}

impl Refusal {
    pub fn new(status: u16, detail: impl Into<String>) -> Self {
        Self { status, detail: detail.into() }
    }

    pub fn bad(detail: impl Into<String>) -> Self {
        Self::new(400, detail)
    }

    pub fn unavailable(err: &PluginError) -> Self {
        Self::new(503, format!("DOC accounts could not reach the backend: {err}"))
    }
}

impl From<PluginError> for Refusal {
    fn from(err: PluginError) -> Self {
        match err.problem() {
            Some((status, detail)) if (400..500).contains(&status) => Self::new(status, detail),
            _ => Self::unavailable(&err),
        }
    }
}

/// A username: letters, digits, `.`, `_` and `-`, up to 64, starting with a letter or a digit.
pub fn username(text: &str) -> Result<String, Refusal> {
    let text = text.trim();
    let mut chars = text.chars();
    let valid = text.len() <= MAX_USERNAME
        && chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
    match valid {
        true => Ok(text.to_string()),
        false => Err(Refusal::bad(format!(
            "a username is 1 to {MAX_USERNAME} letters, digits, `.`, `_` and `-`, starting with a \
             letter or a digit"
        ))),
    }
}

pub fn key(username: &str) -> String {
    username.trim().to_ascii_lowercase()
}

/// A name or an email address as a form gives it: trimmed, with nothing that isn't text.
pub fn detail(text: &str, what: &str, email: bool) -> Result<String, Refusal> {
    let text = text.trim();
    let limit = if email { MAX_EMAIL } else { MAX_NAME };
    if text.chars().count() > limit || text.chars().any(char::is_control) {
        return Err(Refusal::bad(format!("{what} is at most {limit} characters")));
    }
    if email && !text.is_empty() && !text.contains('@') {
        return Err(Refusal::bad("an email address has an @ in it"));
    }
    Ok(text.to_string())
}

/// A password someone chooses: long enough to hold, and not absurdly long.
pub fn chosen(password: &str) -> Result<&str, Refusal> {
    match password.chars().count() {
        count if count < MIN_PASSWORD => Err(Refusal::bad(format!(
            "a password is at least {MIN_PASSWORD} characters: try a few words together"
        ))),
        count if count > MAX_PASSWORD => {
            Err(Refusal::bad(format!("a password is at most {MAX_PASSWORD} characters")))
        }
        _ => Ok(password),
    }
}

pub fn hash(password: &str) -> Result<String, Refusal> {
    argon2::Argon2::default()
        .hash_password(password.as_bytes())
        .map(|hash| hash.to_string())
        .map_err(|err| Refusal::new(500, format!("the password could not be hashed: {err}")))
}

fn verify(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash).is_ok_and(|parsed| {
        argon2::Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok()
    })
}

/// Checked against when there is no such account, so that a missing one takes as long as a wrong
/// password and nobody learns which usernames exist.
fn decoy() -> &'static str {
    static DECOY: OnceLock<String> = OnceLock::new();
    DECOY.get_or_init(|| {
        argon2::Argon2::default()
            .hash_password(b"no account has this password")
            .map(|hash| hash.to_string())
            .unwrap_or_default()
    })
}

/// A password that works once: 20 characters with no 0, O, 1, I or L, in groups of four.
pub fn one_time_password() -> Result<String, Refusal> {
    const ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";
    let mut bytes = [0u8; 20];
    getrandom::fill(&mut bytes)
        .map_err(|err| Refusal::new(500, format!("no randomness for a password: {err}")))?;
    let characters: Vec<char> = bytes
        .iter()
        .map(|byte| char::from(ALPHABET[usize::from(*byte) % ALPHABET.len()]))
        .collect();
    let groups: Vec<String> = characters.chunks(4).map(|group| group.iter().collect()).collect();
    Ok(groups.join("-"))
}

pub struct Store<'a>(pub &'a Backend);

impl Store<'_> {
    pub async fn get(&self, username: &str) -> Result<Option<Account>, Refusal> {
        let found: Option<Map<String, Value>> = self
            .0
            .get(COLLECTION, key(username))
            .await
            .map_err(|err| Refusal::unavailable(&err))?;
        found.map(read).transpose()
    }

    pub async fn all(&self) -> Result<Vec<Account>, Refusal> {
        let rows: Vec<Map<String, Value>> = self
            .0
            .query_all(Query::new(COLLECTION))
            .await
            .map_err(|err| Refusal::unavailable(&err))?;
        let mut accounts = rows.into_iter().map(read).collect::<Result<Vec<_>, _>>()?;
        accounts.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(accounts)
    }

    pub async fn any(&self) -> Result<bool, Refusal> {
        let page = self
            .0
            .query::<Map<String, Value>>(Query::new(COLLECTION).limit(1))
            .await
            .map_err(|err| Refusal::unavailable(&err))?;
        Ok(!page.records.is_empty())
    }

    /// A new account, with a one-time password if one is given; a taken username is refused.
    pub async fn create(
        &self,
        username: &str,
        profile: [&str; 3],
        one_time: Option<(&str, Option<DateTime<Utc>>)>,
    ) -> Result<Account, Refusal> {
        let [first_name, surname, email] = profile;
        let mut values = json!({
            "id": key(username), "username": username,
            "first_name": first_name, "surname": surname, "email": email,
        });
        if let Some((password, expires_at)) = one_time {
            values["one_time_hash"] = json!(hash(password)?);
            values["one_time_expires_at"] = json!(expires_at);
        }
        match self.0.insert::<Map<String, Value>>(COLLECTION, values).await {
            Ok(stored) => read(stored),
            Err(err) if err.is_duplicate() => {
                Err(Refusal::new(409, format!("there is an account called {username} already")))
            }
            Err(err) => Err(Refusal::unavailable(&err)),
        }
    }

    pub async fn set(&self, username: &str, set: Value) -> Result<Option<Account>, Refusal> {
        let updated: Option<Map<String, Value>> = self
            .0
            .update(COLLECTION, key(username), set, None)
            .await
            .map_err(|err| Refusal::unavailable(&err))?;
        updated.map(read).transpose()
    }

    /// A fresh one-time password, which also ends the password they had: a reset is for someone
    /// who lost theirs, or whose password others may know.
    pub async fn give_one_time(
        &self,
        username: &str,
        password: &str,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<Option<Account>, Refusal> {
        let set = json!({
            "one_time_hash": hash(password)?, "one_time_expires_at": expires_at,
            "password_hash": Value::Null,
        });
        self.set(username, set).await
    }

    pub async fn choose(&self, username: &str, password: &str) -> Result<Option<Account>, Refusal> {
        let set = json!({
            "password_hash": hash(chosen(password)?)?,
            "one_time_hash": Value::Null, "one_time_expires_at": Value::Null,
        });
        self.set(username, set).await
    }

    /// Whether `password` opens the account, counting refusals so that a username rests after
    /// too many.
    pub async fn check(&self, username: &str, password: &str) -> Result<Checked, Refusal> {
        let failures = format!("failures/{}", key(username));
        let tried = self.0.cache_get(&failures).await.ok().flatten();
        if tried.and_then(|tried| tried.as_u64()).is_some_and(|tried| tried >= ATTEMPTS) {
            return Ok(Checked::Resting);
        }
        let account = self.get(username).await?;
        let now = Utc::now();
        let checked = match &account {
            None => {
                verify(password, decoy());
                Checked::Wrong
            }
            Some(account) => {
                let own =
                    account.password_hash.as_deref().is_some_and(|hash| verify(password, hash));
                let one_time = !own
                    && !account.one_time_expired(now)
                    && account.one_time_hash.as_deref().is_some_and(|hash| verify(password, hash));
                match (own || one_time, account.disabled, one_time) {
                    (true, true, _) => Checked::Disabled,
                    (true, false, true) => Checked::OneTime(account.clone()),
                    (true, false, false) => Checked::Password(account.clone()),
                    (false, ..) => Checked::Wrong,
                }
            }
        };
        match &checked {
            Checked::Wrong => {
                let tried = tried_so_far(self.0, &failures).await + 1;
                let _ = self.0.cache_set(&failures, json!(tried), Some(REST)).await;
            }
            Checked::Password(_) | Checked::OneTime(_) => {
                let _ = self.0.cache_delete(&failures).await;
            }
            _ => {}
        }
        Ok(checked)
    }

    /// A ticket to choose a password with, after signing in with a one-time one. It lives in the
    /// cache under its hash, for ten minutes, and works once.
    pub async fn ticket(&self, username: &str) -> Result<String, Refusal> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes)
            .map_err(|err| Refusal::new(500, format!("no randomness for a ticket: {err}")))?;
        let ticket = hex::encode(bytes);
        self.0
            .cache_set(&ticket_key(&ticket), json!(key(username)), Some(TICKET))
            .await
            .map_err(|err| Refusal::unavailable(&err))?;
        Ok(ticket)
    }

    /// The username a ticket was for, once.
    pub async fn redeem(&self, ticket: &str) -> Result<Option<String>, Refusal> {
        let at = ticket_key(ticket);
        let found = self.0.cache_get(&at).await.map_err(|err| Refusal::unavailable(&err))?;
        let Some(username) = found.and_then(|found| found.as_str().map(str::to_string)) else {
            return Ok(None);
        };
        self.0.cache_delete(&at).await.map_err(|err| Refusal::unavailable(&err))?;
        Ok(Some(username))
    }
}

async fn tried_so_far(backend: &Backend, failures: &str) -> u64 {
    backend.cache_get(failures).await.ok().flatten().and_then(|tried| tried.as_u64()).unwrap_or(0)
}

fn ticket_key(ticket: &str) -> String {
    format!("tickets/{}", hex::encode(sha2::Sha256::digest(ticket.as_bytes())))
}

fn read(record: Map<String, Value>) -> Result<Account, Refusal> {
    serde_json::from_value(Value::Object(record))
        .map_err(|err| Refusal::new(503, format!("a stored account could not be read: {err}")))
}
