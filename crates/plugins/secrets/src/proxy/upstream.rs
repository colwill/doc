//! A proxied account: a base address, a credential, and how that credential is applied
//! (ADR-0014 §1 and §2). This is the one place per-vendor knowledge survives, and it is a closed
//! set of four rows rather than an integration each.

use std::collections::BTreeMap;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use doc_plugin_sdk::protocol::Secret as Hidden;
use serde_json::{Value, json};

use crate::vendors::{ConfigField, address, strings};

/// How the account's credential is put on the call DOC makes upstream. A caller's own request
/// never carries any of this and is unaffected by which row an account uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placement {
    /// `Authorization: Bearer <secret>`. Most of them.
    Bearer,
    /// A header of the vendor's own, such as `X-JFrog-Art-Api`.
    Named(String),
    /// A username and the secret as its password. Registries, and older APIs.
    Basic(String),
    /// A query parameter. The few that still take one.
    Query(String),
}

impl Placement {
    fn read(config: &Value) -> Result<Self, String> {
        let text = |key: &str| config[key].as_str().unwrap_or_default().trim().to_string();
        match config["placement"].as_str().unwrap_or("bearer") {
            "bearer" => Ok(Self::Bearer),
            "header" => match text("header") {
                empty if empty.is_empty() => Err("the account has no header to send".into()),
                header => Ok(Self::Named(header)),
            },
            "basic" => Ok(Self::Basic(text("username"))),
            "query" => match text("parameter") {
                empty if empty.is_empty() => Err("the account has no parameter to send".into()),
                parameter => Ok(Self::Query(parameter)),
            },
            other => Err(format!("{other} is not a way to apply a credential")),
        }
    }

    /// What it comes to on the wire, for a page to show without showing the credential.
    pub fn shown(&self) -> String {
        match self {
            Self::Bearer => "Authorization: Bearer …".into(),
            Self::Named(header) => format!("{header}: …"),
            Self::Basic(username) => format!("Basic, as {username}"),
            Self::Query(parameter) => format!("?{parameter}=…"),
        }
    }
}

/// Everything the proxy needs to make one call on an account's behalf, with the credential
/// already opened. It is held in memory and never written anywhere (§11).
#[derive(Debug, Clone)]
pub struct Upstream {
    /// Where calls go, with no trailing slash.
    pub base: String,
    /// The host, which is what the log calls the vendor.
    pub host: String,
    pub placement: Placement,
    /// Request headers a caller may send that are not on the platform's own safelist, because
    /// this vendor needs them.
    pub extra: Vec<String>,
}

impl Upstream {
    pub fn read(config: &Value) -> Result<Self, String> {
        let base = config["base"].as_str().unwrap_or_default().trim().trim_end_matches('/');
        if base.is_empty() {
            return Err("the account has no address".into());
        }
        let host = url::Url::parse(base)
            .ok()
            .and_then(|parsed| parsed.host_str().map(str::to_string))
            .ok_or_else(|| format!("{base} is not an address"))?;
        Ok(Self {
            base: base.to_string(),
            host,
            placement: Placement::read(config)?,
            extra: strings(&config["headers"])
                .into_iter()
                .map(|header| header.trim().to_ascii_lowercase())
                .filter(|header| !header.is_empty())
                .collect(),
        })
    }

    /// Where a call on `path` goes, with the caller's query and the credential's parameter when
    /// that is how this account applies it.
    pub fn address(&self, path: &str, query: &str, credential: &Hidden<String>) -> String {
        let path = path.strip_prefix('/').unwrap_or(path);
        let mut address = format!("{}/{path}", self.base);
        let mut query = query.trim_start_matches('?').to_string();
        if let Placement::Query(parameter) = &self.placement {
            let applied = format!("{}={}", urlencoded(parameter), urlencoded(credential.expose()));
            query = match query.is_empty() {
                true => applied,
                false => format!("{query}&{applied}"),
            };
        }
        if !query.is_empty() {
            address.push('?');
            address.push_str(&query);
        }
        address
    }

    /// The header the credential goes in, when it goes in one. `None` for a query parameter,
    /// which is already on the address.
    pub fn header(&self, credential: &Hidden<String>) -> Option<(String, String)> {
        match &self.placement {
            Placement::Bearer => {
                Some(("authorization".into(), format!("Bearer {}", credential.expose())))
            }
            Placement::Named(header) => {
                Some((header.to_ascii_lowercase(), credential.expose().to_string()))
            }
            Placement::Basic(username) => {
                let pair = format!("{username}:{}", credential.expose());
                Some(("authorization".into(), format!("Basic {}", STANDARD.encode(pair))))
            }
            Placement::Query(_) => None,
        }
    }
}

/// Percent-encoding for the one place DOC builds a query itself.
fn urlencoded(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

/// The boxes on the form that onboards a proxied account. One form for every vendor there will
/// ever be, which is the whole point of the decision.
pub fn config_fields() -> Vec<ConfigField> {
    let field =
        |key, label, hint, required| ConfigField { key, label, hint, required, lines: false };
    vec![
        field(
            "base",
            "Address",
            "Where calls go, such as https://api.github.com or https://acme.jfrog.io/artifactory.",
            true,
        ),
        field(
            "header",
            "Header",
            "Only when the credential goes in a header of the vendor's own, such as X-JFrog-Art-Api.",
            false,
        ),
        field(
            "username",
            "Username",
            "Only for a vendor that wants the credential as a password.",
            false,
        ),
        field(
            "parameter",
            "Parameter",
            "Only for a vendor that wants the credential in the query.",
            false,
        ),
        field(
            "probe",
            "A path to try it against",
            "A harmless GET that proves the credential works, such as /user. DOC calls it before the account is stored.",
            true,
        ),
        ConfigField {
            key: "headers",
            label: "Headers callers may send",
            hint: "Only what this vendor needs beyond the usual ones, such as x-github-api-version. One to a line.",
            required: false,
            lines: true,
        },
    ]
}

/// A proxied account's configuration from what the form sent, checked. The placement is worked
/// out from which box was filled in, so nobody has to choose a word for it.
pub fn config(given: &BTreeMap<String, String>) -> Result<Value, String> {
    let at = |key: &str| given.get(key).map(|value| value.trim()).unwrap_or_default().to_string();
    let base = address(&at("base"))?;
    let probe = at("probe");
    if probe.is_empty() {
        return Err("give a path DOC can try the credential against".into());
    }
    let (header, username, parameter) = (at("header"), at("username"), at("parameter"));
    let chosen: Vec<&str> =
        [("header", &header), ("username", &username), ("parameter", &parameter)]
            .iter()
            .filter(|(_, value)| !value.is_empty())
            .map(|(name, _)| *name)
            .collect();
    let placement = match chosen.as_slice() {
        [] => "bearer",
        ["header"] => "header",
        ["username"] => "basic",
        ["parameter"] => "query",
        _ => return Err("fill in one of header, username and parameter, or none of them".into()),
    };
    let headers: Vec<String> = at("headers")
        .split(['\n', ','])
        .map(|header| header.trim().to_ascii_lowercase())
        .filter(|header| !header.is_empty())
        .collect();
    if let Some(wrong) = headers.iter().find(|header| !header_name(header)) {
        return Err(format!("{wrong} is not a header's name"));
    }
    let config = json!({
        "base": base,
        "placement": placement,
        "header": header,
        "username": username,
        "parameter": parameter,
        "probe": format!("/{}", probe.trim_start_matches('/')),
        "headers": headers,
    });
    Upstream::read(&config)?;
    Ok(config)
}

fn header_name(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 64
        && text.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// What an account comes to, in words, for whoever is looking at it.
pub fn describe(config: &Value) -> String {
    match Upstream::read(config) {
        Ok(upstream) => format!("{} — {}", upstream.base, upstream.placement.shown()),
        Err(wrong) => wrong,
    }
}
