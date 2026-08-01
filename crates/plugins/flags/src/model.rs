//! What a flag and a piece of runtime configuration are, and what an upstream provider is. A flag
//! is a switch a service reads while it runs; configuration is a value it reads the same way. They
//! are the same record with a different role, because a service asks for both in one call.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAX_KEY: usize = 120;
pub const MAX_DESCRIPTION: usize = 500;
pub const MAX_STRING: usize = 4_000;
/// Every service and every environment: what a flag applies to unless it says otherwise.
pub const ANY: &str = "*";

/// A switch, or a value read at runtime. The difference is what a page calls it and which tab it
/// is on; a service is given both in one answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Flag,
    Config,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Flag => "flag",
            Self::Config => "config",
        }
    }

    pub fn named(text: &str) -> Option<Self> {
        match text {
            "flag" | "flags" => Some(Self::Flag),
            "config" | "configuration" => Some(Self::Config),
            _ => None,
        }
    }
}

/// What a value is, which is what an SDK asks for and what the form checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    #[default]
    Boolean,
    String,
    Number,
    Json,
}

pub const KINDS: [Kind; 4] = [Kind::Boolean, Kind::String, Kind::Number, Kind::Json];

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Boolean => "boolean",
            Self::String => "string",
            Self::Number => "number",
            Self::Json => "json",
        }
    }

    pub fn named(text: &str) -> Option<Self> {
        KINDS.into_iter().find(|kind| kind.as_str() == text)
    }

    /// What somebody typed, as this kind, or what is wrong with it.
    pub fn read(self, written: &str) -> Result<Value, String> {
        let written = written.trim();
        match self {
            Self::Boolean => match written {
                "true" | "on" | "yes" | "1" => Ok(Value::Bool(true)),
                "false" | "off" | "no" | "0" | "" => Ok(Value::Bool(false)),
                other => Err(format!("`{other}` is not true or false")),
            },
            Self::Number => written
                .parse::<f64>()
                .map(|number| serde_json::json!(number))
                .map_err(|_| format!("`{written}` is not a number")),
            Self::String => match written.len() > MAX_STRING {
                true => Err(format!("a value is at most {MAX_STRING} characters")),
                false => Ok(Value::String(written.to_string())),
            },
            Self::Json => {
                serde_json::from_str(written).map_err(|err| format!("that is not JSON: {err}"))
            }
        }
    }

    /// A value as the form shows it for editing.
    pub fn written(self, value: &Value) -> String {
        match (self, value) {
            (Self::Json, value) => {
                serde_json::to_string_pretty(value).unwrap_or_else(|_| "null".into())
            }
            (_, Value::String(text)) => text.clone(),
            (_, Value::Null) => String::new(),
            (_, other) => other.to_string(),
        }
    }

    /// Whether a value is of this kind, which is checked before anything is stored.
    pub fn holds(self, value: &Value) -> bool {
        match self {
            Self::Boolean => value.is_boolean(),
            Self::String => value.is_string(),
            Self::Number => value.is_number(),
            Self::Json => true,
        }
    }
}

/// Where flags are read from when they are not DOC's own: the platforms a team may already have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Upstream {
    /// Unleash's client API: `GET <url>/api/client/features`.
    Unleash,
    /// Flagsmith's environment flags: `GET <url>/api/v1/flags/`.
    Flagsmith,
    /// OpenFeature's remote evaluation protocol: `POST <url>/ofrep/v1/evaluate/flags`.
    Ofrep,
    /// Runcfg's SDK API: `GET <url>/api/v1/sdk/project/<id>/flags`, and each configuration the
    /// provider names from `…/config?name=<name>`. It holds runtime configuration as well as flags,
    /// so it is the one provider whose values can land on either side.
    Runcfg,
    /// Anything that answers a JSON object of keys and values to a `GET`.
    Http,
}

pub const UPSTREAMS: [Upstream; 5] =
    [Upstream::Unleash, Upstream::Flagsmith, Upstream::Ofrep, Upstream::Runcfg, Upstream::Http];

impl Upstream {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unleash => "unleash",
            Self::Flagsmith => "flagsmith",
            Self::Ofrep => "ofrep",
            Self::Runcfg => "runcfg",
            Self::Http => "http",
        }
    }

    pub fn named(text: &str) -> Option<Self> {
        UPSTREAMS.into_iter().find(|kind| kind.as_str() == text)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Unleash => "Unleash",
            Self::Flagsmith => "Flagsmith",
            Self::Ofrep => "OpenFeature (OFREP)",
            Self::Runcfg => "Runcfg",
            Self::Http => "A JSON endpoint",
        }
    }

    /// What it is read from and what it gives, which the form shows once it is chosen.
    pub fn about(self) -> &'static str {
        match self {
            Self::Unleash => "Every toggle from its client API, GET <url>/api/client/features.",
            Self::Flagsmith => "An environment's flags, GET <url>/api/v1/flags/.",
            Self::Ofrep => "Anything speaking OpenFeature's remote evaluation protocol.",
            Self::Runcfg => {
                "A project's flags, GET <url>/api/v1/sdk/project/<id>/flags, and the \
                 configurations you name."
            }
            Self::Http => "Anything that answers a GET with a JSON object of keys and values.",
        }
    }

    /// Where it is when nobody says: only a hosted provider has an answer.
    pub fn default_url(self) -> Option<&'static str> {
        match self {
            Self::Runcfg => Some("https://runcfg.com"),
            _ => None,
        }
    }

    pub fn placeholder(self) -> &'static str {
        match self {
            Self::Unleash => "https://unleash.example",
            Self::Flagsmith => "https://edge.api.flagsmith.com",
            Self::Ofrep => "https://flags.example",
            Self::Runcfg => "https://runcfg.com",
            Self::Http => "https://config.example/flags.json",
        }
    }

    /// The credentials it is read with, in the order a provider names them.
    pub fn credentials(self) -> &'static [Slot] {
        const BEARER: Slot = Slot {
            label: "Token",
            hint: "Sent as a bearer token. Leave it out if it needs none.",
            required: false,
        };
        match self {
            Self::Unleash => &[Slot {
                label: "Client API token",
                hint: "Sent in the Authorization header.",
                required: false,
            }],
            Self::Flagsmith => &[Slot {
                label: "Environment key",
                hint: "Sent in the X-Environment-Key header.",
                required: false,
            }],
            Self::Runcfg => &[
                Slot { label: "Project ID", hint: "It goes in the path.", required: true },
                Slot {
                    label: "Client ID",
                    hint: "Sent in the X-Client-ID header.",
                    required: true,
                },
                Slot {
                    label: "Client secret",
                    hint: "Sent in the X-Client-Secret header.",
                    required: true,
                },
            ],
            Self::Ofrep | Self::Http => &[BEARER],
        }
    }

    /// Whether it holds configurations by name, which a provider then has to name to be read:
    /// Runcfg has no way to list them.
    pub fn reads_configs(self) -> bool {
        self == Self::Runcfg
    }
}

/// One credential a provider is read with: what the form calls it and how it is used.
pub struct Slot {
    pub label: &'static str,
    pub hint: &'static str,
    pub required: bool,
}

/// Which side wins when DOC and an upstream provider both hold a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Precedence {
    /// DOC is the store and the provider fills in what it does not hold.
    Doc,
    /// The provider is the store and DOC is where a value is overridden.
    Upstream,
}

impl Precedence {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Doc => "doc",
            Self::Upstream => "upstream",
        }
    }

    pub fn named(text: &str) -> Option<Self> {
        match text {
            "doc" => Some(Self::Doc),
            "upstream" => Some(Self::Upstream),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Doc => "DOC wins",
            Self::Upstream => "The provider wins",
        }
    }
}

/// A key a service reads: lowercase letters, digits, `-`, `_` and `.`, which is what every SDK and
/// every provider agrees on.
pub fn key(name: &str) -> Result<String, String> {
    let name = name.trim();
    let shaped = !name.is_empty()
        && name.len() <= MAX_KEY
        && name.chars().next().is_some_and(|letter| letter.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|letter| letter.is_ascii_alphanumeric() || matches!(letter, '-' | '_' | '.'));
    match shaped {
        true => Ok(name.to_ascii_lowercase()),
        false => Err(format!(
            "`{name}` is not a key: 1 to {MAX_KEY} letters, digits, dashes, underscores and dots"
        )),
    }
}

/// A service or an environment a flag applies to, or `*` for all of them.
pub fn scope(name: &str, what: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() || name == ANY {
        return Ok(ANY.to_string());
    }
    let shaped = name.len() <= 120
        && name.chars().next().is_some_and(|letter| letter.is_ascii_alphanumeric())
        && name.chars().all(|letter| {
            letter.is_ascii_alphanumeric() || matches!(letter, '-' | '_' | '.' | '/')
        });
    match shaped {
        true => Ok(name.to_ascii_lowercase()),
        false => Err(format!("`{name}` is not {what}")),
    }
}

/// How precisely an entry names what it is for: the more it names, the later it is applied.
pub fn specificity(service: &str, environment: &str) -> u8 {
    u8::from(service != ANY) * 2 + u8::from(environment != ANY)
}
