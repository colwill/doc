//! Identity types and the first-start bootstrap: the operator service account, the plugin
//! registry and the tokens issued by the bootstrap job.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::Config;
use crate::db::repositories::{NewToken, Repositories};
use crate::secrets::{self, TokenKind};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TokenOwner {
    User(Uuid),
    ServiceAccount(Uuid),
    Plugin(String),
}

/// A person, in exactly one organisation. They sign in with their identities, and can exist before
/// they have any (ADR-0005).
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct User {
    pub id: Uuid,
    /// The name they go by: the login of the account they were first known by, or an admin's choice.
    pub login: String,
    #[serde(default)]
    pub organisation_id: Uuid,
    pub name: Option<String>,
    pub email: Option<String>,
    #[serde(default)]
    pub first_name: Option<String>,
    #[serde(default)]
    pub surname: Option<String>,
    pub disabled: bool,
    pub first_signed_in_at: Option<DateTime<Utc>>,
    pub last_signed_in_at: Option<DateTime<Utc>>,
    /// Each linked account, as provider to login.
    #[sqlx(json)]
    #[serde(default)]
    pub linked: BTreeMap<String, String>,
    /// Set only while they act through a scoped token: the permissions it limits them to
    /// (FEAT-VACUUM). Never stored with the user; it comes from the token each time.
    #[sqlx(skip)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes: Option<Vec<String>>,
}

/// An account with a provider, which belongs to one user. At most one per provider per user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct Identity {
    pub id: Uuid,
    pub user_id: Uuid,
    pub provider: String,
    /// The provider's immutable ID for the account, never its login, which can change hands.
    pub external_id: String,
    pub login: String,
    /// `sign-in`, `link`, `admin` or `provider`.
    pub source: String,
    /// What the provider last reported of the person behind it, and when.
    pub name: Option<String>,
    pub email: Option<String>,
    pub first_name: Option<String>,
    pub surname: Option<String>,
    pub reported_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
}

/// An account as a provider or an admin describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub provider: String,
    pub external_id: String,
    pub login: String,
}

/// What a provider says about the person behind an account. The latest is kept, and what it leaves
/// out is never cleared.
#[derive(Debug, Clone, Default)]
pub struct Profile {
    pub name: Option<String>,
    pub email: Option<String>,
    pub first_name: Option<String>,
    pub surname: Option<String>,
}

impl Profile {
    pub fn is_empty(&self) -> bool {
        self.name.is_none()
            && self.email.is_none()
            && self.first_name.is_none()
            && self.surname.is_none()
    }
}

/// How an account arrived: someone signing in with it, or its provider describing a directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arrival {
    SignIn,
    Provider,
}

impl Arrival {
    pub fn source(self) -> &'static str {
        match self {
            Self::SignIn => "sign-in",
            Self::Provider => "provider",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ServiceAccount {
    pub id: Uuid,
    pub name: String,
    pub description: Option<String>,
    /// The user who owns it; `None` with no team either means the platform does.
    pub owner_id: Option<Uuid>,
    /// The team that owns it, whose members, and the members of teams below it, manage it.
    #[serde(default)]
    pub owner_team_id: Option<Uuid>,
    pub disabled: bool,
    pub created_at: DateTime<Utc>,
}

/// Who is calling. A plugin holds only a registration token until T20 gives it an identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Principal {
    User(User),
    ServiceAccount(ServiceAccount),
    Plugin { id: String },
}

impl Principal {
    pub fn disabled(&self) -> bool {
        match self {
            Self::User(user) => user.disabled,
            Self::ServiceAccount(account) => account.disabled,
            Self::Plugin { .. } => false,
        }
    }

    pub fn owner(&self) -> TokenOwner {
        match self {
            Self::User(user) => TokenOwner::User(user.id),
            Self::ServiceAccount(account) => TokenOwner::ServiceAccount(account.id),
            Self::Plugin { id } => TokenOwner::Plugin(id.clone()),
        }
    }

    pub fn label(&self) -> String {
        match self {
            Self::User(user) => user.login.clone(),
            Self::ServiceAccount(account) => account.name.clone(),
            Self::Plugin { id } => id.clone(),
        }
    }

    /// `user:<id>`, `service:<id>` or `plugin:<id>`: what a Service Bus request carries as the
    /// principal it is made for, and the key its cached permissions are stored under.
    pub fn reference(&self) -> String {
        match self {
            Self::User(user) => format!("user:{}", user.id),
            Self::ServiceAccount(account) => format!("service:{}", account.id),
            Self::Plugin { id } => format!("plugin:{id}"),
        }
    }

    /// The permissions a scoped token limits this principal to, when it came with one.
    pub fn scopes(&self) -> Option<&[String]> {
        match self {
            Self::User(user) => user.scopes.as_deref(),
            _ => None,
        }
    }

    /// The same principal without the limits of the token it came with.
    pub fn unscoped(&self) -> Self {
        match self {
            Self::User(user) => Self::User(User { scopes: None, ..user.clone() }),
            other => other.clone(),
        }
    }

    pub fn as_user(&self) -> Option<&User> {
        match self {
            Self::User(user) => Some(user),
            _ => None,
        }
    }
}

/// A user an admin makes before the person signs in.
#[derive(Debug, Clone)]
pub struct NewUser {
    pub login: String,
    pub organisation_id: Uuid,
    pub name: Option<String>,
    pub email: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub actor_kind: String,
    pub actor_id: Option<String>,
    pub actor_label: Option<String>,
    pub action: String,
    pub subject: Option<String>,
    pub detail: serde_json::Value,
    pub request_id: Option<String>,
}

impl AuditEntry {
    pub fn new(action: impl Into<String>) -> Self {
        Self {
            actor_kind: "anonymous".into(),
            actor_id: None,
            actor_label: None,
            action: action.into(),
            subject: None,
            detail: serde_json::Value::Object(Default::default()),
            request_id: None,
        }
    }

    pub fn by(mut self, principal: &Principal) -> Self {
        let (kind, id) = match principal {
            Principal::User(user) => ("user", user.id.to_string()),
            Principal::ServiceAccount(account) => ("service-account", account.id.to_string()),
            Principal::Plugin { id } => ("plugin", id.clone()),
        };
        self.actor_kind = kind.into();
        self.actor_id = Some(id);
        self.actor_label = Some(principal.label());
        self
    }

    pub fn by_login(mut self, login: &str) -> Self {
        self.actor_label = Some(login.to_string());
        self
    }

    pub fn subject(mut self, subject: impl Into<String>) -> Self {
        self.subject = Some(subject.into());
        self
    }

    pub fn detail(mut self, detail: serde_json::Value) -> Self {
        self.detail = detail;
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiToken {
    pub id: Uuid,
    pub kind: TokenKind,
    pub name: Option<String>,
    pub owner: TokenOwner,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
    /// For a scoped token only: the permissions it is limited to (FEAT-VACUUM).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes: Option<Vec<String>>,
    /// The plugin that minted it for its holder, if one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issued_by: Option<String>,
}

impl ApiToken {
    pub fn usable(&self, now: DateTime<Utc>) -> bool {
        self.revoked_at.is_none() && self.expires_at.is_none_or(|expiry| expiry > now)
    }
}

pub fn token_hash(token: &str) -> Vec<u8> {
    use sha2::Digest;
    sha2::Sha256::digest(token.as_bytes()).to_vec()
}

#[derive(Debug, Default)]
pub struct BootstrapReport {
    pub created_service_accounts: Vec<String>,
    pub created_tokens: Vec<String>,
    pub registered_plugins: Vec<String>,
}

impl BootstrapReport {
    pub fn is_empty(&self) -> bool {
        self.created_service_accounts.is_empty()
            && self.created_tokens.is_empty()
            && self.registered_plugins.is_empty()
    }
}

/// Records one of the platform's own service accounts and the token the bootstrap job wrote for it.
async fn ensure_account(
    repos: &Repositories,
    name: &str,
    description: &str,
    token_path: &Path,
    kind: TokenKind,
    report: &mut BootstrapReport,
) -> Result<()> {
    let account = match repos.identity.service_account_by_name(name).await? {
        Some(account) => account,
        None => {
            let account = repos
                .identity
                .create_service_account(
                    name,
                    Some(description),
                    crate::teams::AccountOwner::Platform,
                )
                .await?;
            report.created_service_accounts.push(account.name.clone());
            account
        }
    };
    if !token_path.exists() {
        return Ok(());
    }
    let token = secrets::read_trimmed(token_path)?;
    let created = repos
        .identity
        .record_token(NewToken {
            kind,
            token_hash: token_hash(&token),
            name: Some(format!("bootstrap {name} token")),
            owner: TokenOwner::ServiceAccount(account.id),
            expires_at: None,
            scopes: None,
            issued_by: None,
        })
        .await
        .with_context(|| format!("recording the bootstrap token for {name}"))?;
    if created {
        report.created_tokens.push(name.to_string());
    }
    Ok(())
}

/// Records the platform's service accounts, the plugin IDs and the tokens the bootstrap job wrote.
/// Running it again changes nothing: every insert is keyed by name, ID or token hash.
pub async fn ensure_bootstrap(
    repos: &Repositories,
    config: &Config,
    secrets_dir: &Path,
) -> Result<BootstrapReport> {
    let mut report = BootstrapReport::default();

    ensure_account(
        repos,
        &config.bootstrap.operator,
        "Used by just recipes and CI",
        &secrets_dir.join("tokens/operator.token"),
        TokenKind::Operator,
        &mut report,
    )
    .await?;
    ensure_account(
        repos,
        &config.bootstrap.frontend,
        "Used by the frontend to read the API",
        &secrets_dir.join("tokens/frontend.token"),
        TokenKind::Service,
        &mut report,
    )
    .await?;

    for id in &config.plugins.ids {
        if repos.identity.register_plugin(id).await? {
            report.registered_plugins.push(id.clone());
        }
        let path = secrets_dir.join(format!("tokens/plugins/{id}.token"));
        if !path.exists() {
            continue;
        }
        let token = secrets::read_trimmed(&path)?;
        let created = repos
            .identity
            .record_token(NewToken {
                kind: TokenKind::PluginRegistration,
                token_hash: token_hash(&token),
                name: Some(format!("registration token for {id}")),
                owner: TokenOwner::Plugin(id.clone()),
                expires_at: None,
                scopes: None,
                issued_by: None,
            })
            .await
            .with_context(|| format!("recording the registration token for {id}"))?;
        if created {
            report.created_tokens.push(format!("plugin:{id}"));
        }
    }

    Ok(report)
}
