//! The vendors a child token comes from. Each takes an account's own credential once and issues
//! tokens narrower than it: some of its groups, repositories, roles or scopes, for less time.

mod artifactory;
mod aws;
mod github;
mod linode;

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use doc_plugin_sdk::protocol::Secret;
use serde_json::{Map, Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    Artifactory,
    GitHubApp,
    AwsSts,
    Linode,
}

/// One box on the form that onboards an account.
pub struct ConfigField {
    pub key: &'static str,
    pub label: &'static str,
    pub hint: &'static str,
    pub required: bool,
    /// A box of lines rather than one line.
    pub lines: bool,
}

/// What a vendor gave: the token, its ID there when it can be revoked by it, and when it ends.
pub struct Issued {
    pub value: Secret<String>,
    pub vendor_id: Option<String>,
    pub expires_at: DateTime<Utc>,
}

const LEVELS: [&str; 3] = ["read", "write", "admin"];

impl Vendor {
    pub const ALL: [Vendor; 4] = [Self::Artifactory, Self::GitHubApp, Self::AwsSts, Self::Linode];

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|vendor| vendor.id() == text)
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Artifactory => "artifactory",
            Self::GitHubApp => "github-app",
            Self::AwsSts => "aws-sts",
            Self::Linode => "linode",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Artifactory => "JFrog Artifactory",
            Self::GitHubApp => "GitHub App",
            Self::AwsSts => "AWS STS",
            Self::Linode => "Linode",
        }
    }

    pub fn about(self) -> &'static str {
        match self {
            Self::Artifactory => {
                "Access tokens limited to some of the account's groups, for as long as an allowance lets them last."
            }
            Self::GitHubApp => {
                "Installation tokens limited to some repositories and permissions. GitHub makes each last an hour."
            }
            Self::AwsSts => {
                "Temporary keys for a role, narrowed by managed policies. They last 15 minutes to 12 hours and end by themselves."
            }
            Self::Linode => {
                "Personal access tokens limited to some scopes, for as long as an allowance lets them last."
            }
        }
    }

    pub fn credential_label(self) -> &'static str {
        match self {
            Self::Artifactory => "An access token allowed to create tokens",
            Self::GitHubApp => "The app's private key, as GitHub gave it (PEM)",
            Self::AwsSts => "The secret access key",
            Self::Linode => "A personal access token allowed to create tokens",
        }
    }

    /// The shortest and longest a token may last, in minutes.
    pub fn lifetime(self) -> (i64, i64) {
        match self {
            Self::Artifactory | Self::Linode => (5, 366 * 24 * 60),
            Self::GitHubApp => (60, 60),
            Self::AwsSts => (15, 12 * 60),
        }
    }

    /// Whether a token can be kept in a secret for a plugin: one value a setting can hold.
    pub fn keepable(self) -> bool {
        self != Self::AwsSts
    }

    /// Whether DOC can revoke a token before it ends, and what it takes.
    pub fn revocation(self) -> &'static str {
        match self {
            Self::Artifactory | Self::Linode => "Revoking asks the vendor to end it at once.",
            Self::GitHubApp => {
                "A token shown once cannot be revoked by DOC, which does not keep it; it ends within the hour. One kept for a plugin can."
            }
            Self::AwsSts => "AWS cannot end one session early: it ends when it runs out.",
        }
    }

    pub fn config_fields(self) -> Vec<ConfigField> {
        let field =
            |key, label, hint, required| ConfigField { key, label, hint, required, lines: false };
        match self {
            Self::Artifactory => {
                vec![field("url", "Address", "Such as https://acme.jfrog.io.", true)]
            }
            Self::GitHubApp => vec![
                field(
                    "api",
                    "API",
                    "https://api.github.com, or https://ghe.acme.dev/api/v3 for GitHub Enterprise.",
                    true,
                ),
                field("app_id", "App ID", "On the app's settings page.", true),
                field(
                    "installation_id",
                    "Installation ID",
                    "The number at the end of the installation's address.",
                    true,
                ),
            ],
            Self::AwsSts => vec![
                field("region", "Region", "Such as eu-west-2.", true),
                field(
                    "access_key_id",
                    "Access key ID",
                    "The key allowed to assume the roles below.",
                    true,
                ),
                ConfigField {
                    key: "roles",
                    label: "Roles it may assume",
                    hint: "Their ARNs, one to a line, such as arn:aws:iam::123456789012:role/deployer.",
                    required: true,
                    lines: true,
                },
                field("external_id", "External ID", "When the roles ask for one.", false),
                field(
                    "endpoint",
                    "STS endpoint",
                    "Only for a VPC endpoint or something standing in for STS; otherwise the region's own.",
                    false,
                ),
            ],
            Self::Linode => vec![field(
                "api",
                "API",
                "https://api.linode.com/v4 unless you use another.",
                false,
            )],
        }
    }

    /// An account's configuration from what the form sent, checked.
    pub fn config(self, given: &BTreeMap<String, String>) -> Result<Value, String> {
        let mut config = Map::new();
        for field in self.config_fields() {
            let value =
                given.get(field.key).map(|value| value.trim().to_string()).unwrap_or_default();
            if value.is_empty() {
                if field.required {
                    return Err(format!("give the {}", field.label.to_lowercase()));
                }
                continue;
            }
            let checked = match (self, field.key) {
                (_, "url" | "api" | "endpoint") => json!(address(&value)?),
                (Self::GitHubApp, "app_id" | "installation_id") => {
                    value
                        .parse::<u64>()
                        .map_err(|_| format!("the {} is a number", field.label.to_lowercase()))?;
                    json!(value)
                }
                (Self::AwsSts, "region") => {
                    let fine = value
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
                    if !fine {
                        return Err(format!("{value} is not a region"));
                    }
                    json!(value)
                }
                (Self::AwsSts, "roles") => {
                    let roles = lines(&value);
                    if let Some(wrong) = roles.iter().find(|role| !role_arn(role)) {
                        return Err(format!("{wrong} is not a role's ARN"));
                    }
                    json!(roles)
                }
                _ => json!(value),
            };
            config.insert(field.key.to_string(), checked);
        }
        Ok(Value::Object(config))
    }

    /// What an allowance may hand out, from what its form sent, checked against the account.
    pub fn grants(self, config: &Value, given: &BTreeMap<String, String>) -> Result<Value, String> {
        let listed = |key: &str| lines(given.get(key).map(String::as_str).unwrap_or_default());
        match self {
            Self::Artifactory => {
                let groups = listed("groups");
                if groups.is_empty() {
                    return Err("name at least one group a token may carry".into());
                }
                if let Some(wrong) = groups.iter().find(|group| !plain(group)) {
                    return Err(format!("{wrong} is not a group's name"));
                }
                Ok(json!({ "groups": groups }))
            }
            Self::GitHubApp => {
                let repositories = listed("repositories");
                if let Some(wrong) =
                    repositories.iter().find(|repository| !plain(&repository.replace('/', "")))
                {
                    return Err(format!("{wrong} is not a repository's name"));
                }
                let permissions = permissions(&listed("permissions"))?;
                if permissions.is_empty() {
                    return Err("name at least one permission, such as contents:read".into());
                }
                Ok(json!({ "repositories": repositories, "permissions": permissions }))
            }
            Self::AwsSts => {
                let held: Vec<String> = strings(&config["roles"]);
                let roles = listed("roles");
                if roles.is_empty() {
                    return Err("choose at least one role".into());
                }
                if let Some(wrong) = roles.iter().find(|role| !held.contains(role)) {
                    return Err(format!("{wrong} is not one of the roles this account may assume"));
                }
                let policies = listed("policies");
                if let Some(wrong) =
                    policies.iter().find(|policy| !policy.starts_with("arn:aws:iam::"))
                {
                    return Err(format!("{wrong} is not a managed policy's ARN"));
                }
                Ok(json!({ "roles": roles, "policies": policies }))
            }
            Self::Linode => {
                let scopes = listed("scopes");
                if scopes.is_empty() {
                    return Err("name at least one scope, such as linodes:read_only".into());
                }
                if let Some(wrong) = scopes.iter().find(|scope| !linode_scope(scope)) {
                    return Err(format!(
                        "{wrong} is not a scope: write area:read_only or area:read_write"
                    ));
                }
                Ok(json!({ "scopes": scopes }))
            }
        }
    }

    /// What somebody asked for, checked to be within what the allowance grants.
    pub fn within(self, grants: &Value, asked: &Value) -> Result<Value, String> {
        match self {
            Self::Artifactory => {
                let allowed = strings(&grants["groups"]);
                let groups = strings(&asked["groups"]);
                if groups.is_empty() {
                    return Err("choose at least one group".into());
                }
                if let Some(wrong) = groups.iter().find(|group| !allowed.contains(group)) {
                    return Err(format!("the allowance does not grant the group {wrong}"));
                }
                Ok(json!({ "groups": groups }))
            }
            Self::GitHubApp => {
                let allowed = strings(&grants["repositories"]);
                let repositories = strings(&asked["repositories"]);
                if !allowed.is_empty() {
                    if repositories.is_empty() {
                        return Err("choose at least one repository".into());
                    }
                    if let Some(wrong) =
                        repositories.iter().find(|repository| !allowed.contains(repository))
                    {
                        return Err(format!("the allowance does not grant {wrong}"));
                    }
                }
                let granted = grants["permissions"].as_object().cloned().unwrap_or_default();
                let wanted = asked["permissions"].as_object().cloned().unwrap_or_default();
                if wanted.is_empty() {
                    return Err("choose at least one permission".into());
                }
                for (name, level) in &wanted {
                    let level = level.as_str().unwrap_or_default();
                    let most = granted.get(name).and_then(Value::as_str);
                    let rank = |level: &str| LEVELS.iter().position(|known| *known == level);
                    match (rank(level), most.and_then(rank)) {
                        (Some(asked), Some(most)) if asked <= most => {}
                        _ => return Err(format!("the allowance does not grant {name}:{level}")),
                    }
                }
                Ok(json!({ "repositories": repositories, "permissions": wanted }))
            }
            Self::AwsSts => {
                let role = asked["role"].as_str().unwrap_or_default().to_string();
                if !strings(&grants["roles"]).contains(&role) {
                    return Err("choose one of the roles the allowance grants".into());
                }
                let allowed = strings(&grants["policies"]);
                let policies = strings(&asked["policies"]);
                if !allowed.is_empty() && policies.is_empty() {
                    return Err(
                        "choose at least one policy: the allowance narrows every session".into()
                    );
                }
                if let Some(wrong) = policies.iter().find(|policy| !allowed.contains(policy)) {
                    return Err(format!("the allowance does not grant {wrong}"));
                }
                Ok(json!({ "role": role, "policies": policies }))
            }
            Self::Linode => {
                let allowed = strings(&grants["scopes"]);
                let scopes = strings(&asked["scopes"]);
                if scopes.is_empty() {
                    return Err("choose at least one scope".into());
                }
                for scope in &scopes {
                    let (area, level) = scope.split_once(':').unwrap_or((scope, ""));
                    let fine = allowed.iter().any(|granted| {
                        let (granted_area, granted_level) =
                            granted.split_once(':').unwrap_or((granted, ""));
                        granted_area == area
                            && (granted_level == level
                                || (granted_level == "read_write" && level == "read_only"))
                    });
                    if !fine {
                        return Err(format!("the allowance does not grant {scope}"));
                    }
                }
                Ok(json!({ "scopes": scopes }))
            }
        }
    }

    /// What grants or restrictions come to, in words.
    pub fn describe(self, value: &Value) -> String {
        let joined = |key: &str| strings(&value[key]).join(", ");
        match self {
            Self::Artifactory => format!("groups {}", joined("groups")),
            Self::GitHubApp => {
                let permissions: Vec<String> = value["permissions"]
                    .as_object()
                    .map(|held| {
                        held.iter()
                            .map(|(name, level)| {
                                format!("{name}:{}", level.as_str().unwrap_or_default())
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let repositories = match joined("repositories") {
                    none if none.is_empty() => "every repository it is installed on".to_string(),
                    some => some,
                };
                format!("{repositories}; {}", permissions.join(", "))
            }
            Self::AwsSts => {
                let roles = match value["role"].as_str() {
                    Some(role) => role.to_string(),
                    None => joined("roles"),
                };
                match joined("policies") {
                    none if none.is_empty() => format!("{roles}, with everything the role allows"),
                    policies => format!("{roles}, narrowed to {policies}"),
                }
            }
            Self::Linode => format!("scopes {}", joined("scopes")),
        }
    }

    /// What can be chosen in a request, for the form: `(field, value, label)` for each box.
    pub fn choices(self, grants: &Value) -> Vec<(&'static str, String, String)> {
        match self {
            Self::Artifactory => strings(&grants["groups"])
                .into_iter()
                .map(|group| ("groups", group.clone(), group))
                .collect(),
            Self::GitHubApp => {
                let mut choices: Vec<(&'static str, String, String)> =
                    strings(&grants["repositories"])
                        .into_iter()
                        .map(|repository| ("repositories", repository.clone(), repository))
                        .collect();
                for (name, level) in grants["permissions"].as_object().cloned().unwrap_or_default()
                {
                    let most = level.as_str().unwrap_or_default();
                    let upto = LEVELS.iter().position(|known| *known == most).unwrap_or_default();
                    for level in &LEVELS[..=upto] {
                        choices.push((
                            "permissions",
                            format!("{name}:{level}"),
                            format!("{name}: {level}"),
                        ));
                    }
                }
                choices
            }
            Self::AwsSts => {
                let mut choices: Vec<(&'static str, String, String)> = strings(&grants["roles"])
                    .into_iter()
                    .map(|role| ("role", role.clone(), role))
                    .collect();
                choices.extend(
                    strings(&grants["policies"])
                        .into_iter()
                        .map(|policy| ("policies", policy.clone(), policy)),
                );
                choices
            }
            Self::Linode => {
                let mut choices = Vec::new();
                for scope in strings(&grants["scopes"]) {
                    if let Some(area) = scope.strip_suffix(":read_write") {
                        choices.push((
                            "scopes",
                            format!("{area}:read_only"),
                            format!("{area}: read only"),
                        ));
                    }
                    let shown = scope.replace(':', ": ").replace('_', " ");
                    choices.push(("scopes", scope, shown));
                }
                choices
            }
        }
    }

    /// A request as a form sends it: ticked boxes under each field.
    pub fn asked(self, ticked: &[(String, String)]) -> Value {
        let under = |key: &str| -> Vec<String> {
            ticked
                .iter()
                .filter(|(field, _)| field == key)
                .map(|(_, value)| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .collect()
        };
        match self {
            Self::Artifactory => json!({ "groups": under("groups") }),
            Self::GitHubApp => {
                let mut permissions = Map::new();
                for picked in under("permissions") {
                    let (name, level) = picked.split_once(':').unwrap_or((&picked, "read"));
                    let rank = |level: &str| {
                        LEVELS.iter().position(|known| *known == level).unwrap_or_default()
                    };
                    let higher = permissions
                        .get(name)
                        .and_then(Value::as_str)
                        .is_some_and(|held| rank(held) >= rank(level));
                    if !higher {
                        permissions.insert(name.to_string(), json!(level));
                    }
                }
                json!({ "repositories": under("repositories"), "permissions": permissions })
            }
            Self::AwsSts => {
                json!({ "role": under("role").into_iter().next().unwrap_or_default(), "policies": under("policies") })
            }
            Self::Linode => json!({ "scopes": under("scopes") }),
        }
    }
}

pub fn strings(value: &Value) -> Vec<String> {
    value.as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect()
}

fn lines(text: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for line in text.split(['\n', ',']).map(str::trim).filter(|line| !line.is_empty()) {
        if !found.iter().any(|seen| seen == line) {
            found.push(line.to_string());
        }
    }
    found
}

fn plain(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

fn role_arn(text: &str) -> bool {
    let Some(rest) = text.strip_prefix("arn:aws:iam::") else { return false };
    let Some((account, role)) = rest.split_once(":role/") else { return false };
    account.len() == 12 && account.chars().all(|c| c.is_ascii_digit()) && !role.is_empty()
}

fn linode_scope(text: &str) -> bool {
    let Some((area, level)) = text.split_once(':') else { return false };
    !area.is_empty()
        && area.chars().all(|c| c.is_ascii_lowercase() || c == '_')
        && matches!(level, "read_only" | "read_write")
}

/// `contents:read` lines into GitHub's permissions object.
fn permissions(given: &[String]) -> Result<Map<String, Value>, String> {
    let mut permissions = Map::new();
    for line in given {
        let (name, level) = line
            .split_once(':')
            .ok_or_else(|| format!("write {line} as name:level, such as contents:read"))?;
        let (name, level) = (name.trim(), level.trim());
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_lowercase() || c == '_') {
            return Err(format!("{name} is not a permission's name"));
        }
        if !LEVELS.contains(&level) {
            return Err(format!("{level} is not a level: read, write or admin"));
        }
        permissions.insert(name.to_string(), json!(level));
    }
    Ok(permissions)
}

/// An address a credential is sent to: https, or http only to this machine, never elsewhere.
pub fn address(text: &str) -> Result<String, String> {
    let parsed = url::Url::parse(text.trim()).map_err(|_| format!("{text} is not an address"))?;
    let local = matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    match (parsed.scheme(), local) {
        ("https", _) | ("http", true) => Ok(text.trim().trim_end_matches('/').to_string()),
        _ => Err(format!("{text} is not https: a credential is never sent in the clear")),
    }
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("doc-secrets")
        .build()
        .map_err(|err| format!("no HTTP client: {err}"))
}

/// What a vendor said when it refused, cut short for a page.
fn refused(vendor: Vendor, status: u16, body: &str) -> String {
    let detail: Value = serde_json::from_str(body).unwrap_or_default();
    let said = detail["message"]
        .as_str()
        .or_else(|| detail["errors"][0]["message"].as_str())
        .or_else(|| detail["errors"][0]["reason"].as_str())
        .map(str::to_string)
        .unwrap_or_else(|| body.chars().take(300).collect());
    format!("{} answered {status}: {said}", vendor.name())
}

fn configured(config: &Value, key: &str) -> Result<String, String> {
    config[key]
        .as_str()
        .map(str::to_string)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("the account has no {key}"))
}

/// Tries the account's credential, answering what the vendor says it belongs to.
pub async fn test(
    vendor: Vendor,
    config: &Value,
    credential: &Secret<String>,
) -> Result<String, String> {
    match vendor {
        Vendor::Artifactory => artifactory::test(config, credential).await,
        Vendor::GitHubApp => github::test(config, credential).await,
        Vendor::AwsSts => aws::test(config, credential).await,
        Vendor::Linode => linode::test(config, credential).await,
    }
}

/// A child token narrowed to `restrictions`, lasting `minutes`, named after who it is for.
pub async fn issue(
    vendor: Vendor,
    config: &Value,
    credential: &Secret<String>,
    restrictions: &Value,
    minutes: i64,
    subject: &str,
    purpose: &str,
) -> Result<Issued, String> {
    match vendor {
        Vendor::Artifactory => {
            artifactory::issue(config, credential, restrictions, minutes, subject, purpose).await
        }
        Vendor::GitHubApp => github::issue(config, credential, restrictions).await,
        Vendor::AwsSts => aws::issue(config, credential, restrictions, minutes, subject).await,
        Vendor::Linode => linode::issue(config, credential, restrictions, minutes, purpose).await,
    }
}

/// Ends a token early where the vendor allows it: by its ID there, or with the token itself.
pub async fn revoke(
    vendor: Vendor,
    config: &Value,
    credential: &Secret<String>,
    vendor_id: Option<&str>,
    token: Option<&Secret<String>>,
) -> Result<(), String> {
    match (vendor, vendor_id, token) {
        (Vendor::Artifactory, Some(id), _) => artifactory::revoke(config, credential, id).await,
        (Vendor::Linode, Some(id), _) => linode::revoke(config, credential, id).await,
        (Vendor::GitHubApp, _, Some(token)) => github::revoke(config, token).await,
        _ => Err(format!(
            "{} gives DOC no way to end this token early; it ends when it runs out",
            vendor.name()
        )),
    }
}
