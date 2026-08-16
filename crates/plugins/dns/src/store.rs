//! What the plugin keeps: the records DOC answers with, one row each. A record's name must fall in
//! one of the domains DOC owns when it is written; one left behind by a domain taken off the
//! Settings page is kept, but not answered with.

use std::net::{Ipv4Addr, Ipv6Addr};

use doc_plugin_sdk::{Backend, Collection, Declaration, Field, Query};
use hickory_proto::rr::rdata::{A, AAAA, CNAME, MX, PTR, SRV, TXT};
use hickory_proto::rr::{Name, RData};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::settings::Serving;
use crate::{Refusal, names};

pub const RECORDS: &str = "records";
pub const MAX_VALUE: usize = 4_000;
pub const MAX_NOTE: usize = 500;
/// A week: longer and a mistake stays in resolvers' memories after it is put right.
pub const MAX_TTL: u32 = 604_800;
/// A TXT record's text is sent as strings of at most this many bytes.
const TXT_STRING: usize = 255;

/// The kinds of record DOC keeps. SOA and NS are made from the settings rather than kept. The
/// names are DNS's own, as they are stored and shown.
#[allow(clippy::upper_case_acronyms)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    A,
    AAAA,
    CNAME,
    MX,
    TXT,
    SRV,
    PTR,
}

impl Kind {
    pub const ALL: [Kind; 7] =
        [Kind::A, Kind::AAAA, Kind::CNAME, Kind::MX, Kind::TXT, Kind::SRV, Kind::PTR];

    pub fn id(self) -> &'static str {
        match self {
            Kind::A => "A",
            Kind::AAAA => "AAAA",
            Kind::CNAME => "CNAME",
            Kind::MX => "MX",
            Kind::TXT => "TXT",
            Kind::SRV => "SRV",
            Kind::PTR => "PTR",
        }
    }

    pub fn parse(text: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|kind| kind.id().eq_ignore_ascii_case(text.trim()))
    }

    /// What the value field holds, as the form says it.
    pub fn shape(self) -> &'static str {
        match self {
            Kind::A => "An IPv4 address, such as 10.0.4.12",
            Kind::AAAA => "An IPv6 address, such as 2001:db8::12",
            Kind::CNAME => "The name this one is another name for, such as api.example.com",
            Kind::MX => "A preference and a mail server, such as 10 mail.example.com",
            Kind::TXT => "Any text, such as v=spf1 include:_spf.example.com ~all",
            Kind::SRV => "Priority, weight, port and target, such as 10 5 5060 sip.example.com",
            Kind::PTR => "The name an address belongs to, such as host.example.com",
        }
    }

    /// The value made normal, or why it will not do.
    pub fn check(self, value: &str) -> Result<String, String> {
        let value = value.trim();
        if value.is_empty() {
            return Err(format!("a record of type {} needs a value. {}", self.id(), self.shape()));
        }
        if value.chars().count() > MAX_VALUE {
            return Err(format!("a value is at most {MAX_VALUE} characters"));
        }
        let wrong =
            || format!("that value doesn't fit a record of type {}. {}", self.id(), self.shape());
        let target = names::plain;
        let number = |text: &str| text.parse::<u16>().map_err(|_| wrong());
        let parts: Vec<&str> = value.split_whitespace().collect();
        match self {
            Kind::A => value.parse::<Ipv4Addr>().map(|ip| ip.to_string()).map_err(|_| wrong()),
            Kind::AAAA => value.parse::<Ipv6Addr>().map(|ip| ip.to_string()).map_err(|_| wrong()),
            Kind::CNAME | Kind::PTR => match parts.as_slice() {
                [name] => target(name),
                _ => Err(wrong()),
            },
            Kind::MX => match parts.as_slice() {
                [preference, exchange] => {
                    Ok(format!("{} {}", number(preference)?, target(exchange)?))
                }
                _ => Err(wrong()),
            },
            Kind::SRV => match parts.as_slice() {
                [priority, weight, port, host] => Ok(format!(
                    "{} {} {} {}",
                    number(priority)?,
                    number(weight)?,
                    number(port)?,
                    target(host)?
                )),
                _ => Err(wrong()),
            },
            Kind::TXT => {
                let unquoted = value
                    .strip_prefix('"')
                    .and_then(|inner| inner.strip_suffix('"'))
                    .filter(|inner| !inner.contains('"'))
                    .unwrap_or(value);
                match unquoted.chars().all(|c| !c.is_control()) {
                    true => Ok(unquoted.to_string()),
                    false => Err("the text cannot hold line breaks or control characters".into()),
                }
            }
        }
    }

    /// A stored value as it goes on the wire; `None` for one that no longer reads.
    pub fn rdata(self, value: &str) -> Option<RData> {
        let parts: Vec<&str> = value.split_whitespace().collect();
        let name = |text: &str| names::fqdn(text);
        let number = |text: &str| text.parse::<u16>().ok();
        Some(match (self, parts.as_slice()) {
            (Kind::A, _) => RData::A(A(value.parse().ok()?)),
            (Kind::AAAA, _) => RData::AAAA(AAAA(value.parse().ok()?)),
            (Kind::CNAME, [target]) => RData::CNAME(CNAME(name(target)?)),
            (Kind::PTR, [target]) => RData::PTR(PTR(name(target)?)),
            (Kind::MX, [preference, exchange]) => {
                RData::MX(MX::new(number(preference)?, name(exchange)?))
            }
            (Kind::SRV, [priority, weight, port, host]) => {
                RData::SRV(SRV::new(number(priority)?, number(weight)?, number(port)?, name(host)?))
            }
            (Kind::TXT, _) => RData::TXT(TXT::new(strings(value))),
            _ => return None,
        })
    }
}

/// Text cut into strings of at most 255 bytes, never inside a character.
fn strings(text: &str) -> Vec<String> {
    let mut strings = vec![String::new()];
    for c in text.chars() {
        if strings.last().is_some_and(|last| last.len() + c.len_utf8() > TXT_STRING) {
            strings.push(String::new());
        }
        if let Some(last) = strings.last_mut() {
            last.push(c);
        }
    }
    strings
}

/// The name a record's target points at, when it is a name.
pub fn target(rdata: &RData) -> Option<&Name> {
    match rdata {
        RData::CNAME(CNAME(name)) | RData::PTR(PTR(name)) => Some(name),
        RData::MX(mx) => Some(&mx.exchange),
        RData::SRV(srv) => Some(&srv.target),
        _ => None,
    }
}

pub fn declaration() -> Declaration {
    let kinds: Vec<&str> = Kind::ALL.iter().map(|kind| kind.id()).collect();
    Declaration::default().collection(
        RECORDS,
        Collection::new()
            .field("id", Field::uuid().key())
            .field(
                "name",
                Field::text().required().max(253.0).describe(
                    "The full name, lowercase with no trailing dot, such as api.example.com",
                ),
            )
            .field("type", Field::text().required().one_of(&kinds))
            .field("value", Field::text().required().max(MAX_VALUE as f64))
            .field(
                "ttl",
                Field::integer()
                    .min(0.0)
                    .max(f64::from(MAX_TTL))
                    .describe("Seconds; none means the plugin's own setting"),
            )
            .field("note", Field::text().required().default(json!("")).max(MAX_NOTE as f64))
            .field("changed_by", Field::text().required().default(json!("")))
            .unique(&["name", "type", "value"])
            .index(&["name"])
            .search(&["name", "value", "note"]),
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub id: Uuid,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: Kind,
    pub value: String,
    #[serde(default)]
    pub ttl: Option<u32>,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub changed_by: String,
    #[serde(rename = "_version")]
    pub version: i64,
    #[serde(rename = "_updated_at")]
    pub updated_at: String,
}

/// A record as someone asked for it, before it is checked. `name` is the full name; the page
/// sends the part before the domain and the domain apart, and joins them first.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Wanted {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub value: String,
    #[serde(default)]
    pub ttl: Option<u32>,
    #[serde(default)]
    pub note: String,
}

/// A record checked against the settings and the records beside it.
struct Checked {
    name: String,
    kind: Kind,
    value: String,
    ttl: Option<u32>,
    note: String,
}

/// Who is writing, as the record says.
pub fn author(backend: &Backend) -> String {
    backend
        .caller()
        .and_then(|caller| caller.label.clone().or_else(|| caller.id.clone()))
        .unwrap_or_else(|| "someone".into())
}

pub struct Store<'a>(pub &'a Backend);

impl Store<'_> {
    pub async fn records(&self) -> Result<Vec<Record>, Refusal> {
        Ok(self.0.query_all(Query::new(RECORDS)).await?)
    }

    /// The IDs of the plugins the platform knows, in order. `core.plugins` is one of the
    /// collections every plugin may read, so this needs no permission of its own and no discovery.
    pub async fn plugins(&self) -> Result<Vec<String>, Refusal> {
        let listed: Vec<Map<String, Value>> = self.0.query_all(Query::new("core.plugins")).await?;
        let mut ids: Vec<String> = listed
            .iter()
            .filter_map(|plugin| plugin.get("id").and_then(Value::as_str))
            // A name is a DNS label, so only an ID that can be one: every plugin ID is, and
            // anything else would make a name nobody could ask for.
            .filter(|id| {
                !id.is_empty()
                    && id.len() <= 63
                    && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                    && !id.starts_with('-')
                    && !id.ends_with('-')
            })
            .map(str::to_string)
            .collect();
        ids.sort();
        ids.dedup();
        Ok(ids)
    }

    pub async fn at(&self, name: &str) -> Result<Vec<Record>, Refusal> {
        Ok(self.0.query_all(Query::new(RECORDS).filter(json!({ "name": name }))).await?)
    }

    pub async fn found(&self, id: &str) -> Result<Record, Refusal> {
        let missing = || Refusal::missing("there is no such record");
        let id = Uuid::parse_str(id).map_err(|_| missing())?;
        self.0.get(RECORDS, id.to_string()).await?.ok_or_else(missing)
    }

    /// Checks a record against the domains DOC owns and the records already at its name: a CNAME
    /// stands alone at a name, and never at a domain itself. `replacing` is the record an edit
    /// changes, which is not in the way of itself.
    async fn check(
        &self,
        serving: &Serving,
        wanted: Wanted,
        replacing: Option<Uuid>,
    ) -> Result<Checked, Refusal> {
        let name = names::normal(&wanted.name).map_err(Refusal::bad)?;
        let kind = Kind::parse(&wanted.kind).ok_or_else(|| {
            let kinds: Vec<&str> = Kind::ALL.iter().map(|kind| kind.id()).collect();
            Refusal::bad(format!("the type is one of {}", kinds.join(", ")))
        })?;
        let value = kind.check(&wanted.value).map_err(Refusal::bad)?;
        if let Some(ttl) = wanted.ttl
            && ttl > MAX_TTL
        {
            return Err(Refusal::bad(format!("a TTL is at most {MAX_TTL} seconds, a week")));
        }
        let note = wanted.note.trim().to_string();
        if note.chars().count() > MAX_NOTE {
            return Err(Refusal::bad(format!("a note is at most {MAX_NOTE} characters")));
        }
        let Some(zone) = serving.zone_of(&name) else {
            return Err(Refusal::bad(match serving.zones.is_empty() {
                true => "DOC answers for no domains yet: name one on the DNS plugin's Settings \
                         page first"
                    .to_string(),
                false => format!(
                    "{name} is not in a domain DOC answers for: {}",
                    serving.zones.join(", ")
                ),
            }));
        };
        if kind == Kind::CNAME && name == zone {
            return Err(Refusal::bad(format!(
                "{zone} is a domain DOC answers for, so it cannot be a CNAME: its SOA and NS \
                 records have to be there too"
            )));
        }
        let beside: Vec<Record> = self
            .at(&name)
            .await?
            .into_iter()
            .filter(|record| Some(record.id) != replacing)
            .collect();
        if kind == Kind::CNAME && !beside.is_empty() {
            return Err(Refusal::conflict(format!(
                "{name} has other records already; a CNAME has to be the only record at its name"
            )));
        }
        if beside.iter().any(|record| record.kind == Kind::CNAME) {
            return Err(Refusal::conflict(format!(
                "{name} is a CNAME; a name with a CNAME can have no other records"
            )));
        }
        Ok(Checked { name, kind, value, ttl: wanted.ttl, note })
    }

    pub async fn create(&self, serving: &Serving, wanted: Wanted) -> Result<Record, Refusal> {
        self.create_as(serving, wanted, &author(self.0)).await
    }

    /// A record written by something other than a person at the page, which says so itself: an
    /// event carries no caller, so who wrote it has to be named.
    pub async fn create_as(
        &self,
        serving: &Serving,
        wanted: Wanted,
        who: &str,
    ) -> Result<Record, Refusal> {
        let checked = self.check(serving, wanted, None).await?;
        let values = json!({
            "id": Uuid::now_v7(),
            "name": checked.name,
            "type": checked.kind,
            "value": checked.value,
            "ttl": checked.ttl,
            "note": checked.note,
            "changed_by": who,
        });
        self.0.insert(RECORDS, values).await.map_err(|err| duplicate(err, &checked))
    }

    pub async fn replace(
        &self,
        serving: &Serving,
        id: &str,
        wanted: Wanted,
    ) -> Result<Record, Refusal> {
        self.replace_as(serving, id, wanted, &author(self.0)).await
    }

    pub async fn replace_as(
        &self,
        serving: &Serving,
        id: &str,
        wanted: Wanted,
        who: &str,
    ) -> Result<Record, Refusal> {
        let record = self.found(id).await?;
        let checked = self.check(serving, wanted, Some(record.id)).await?;
        let set = json!({
            "name": checked.name,
            "type": checked.kind,
            "value": checked.value,
            "ttl": checked.ttl,
            "note": checked.note,
            "changed_by": who,
        });
        let updated = self.0.update(RECORDS, record.id.to_string(), set, Some(record.version));
        match updated.await {
            Ok(Some(record)) => Ok(record),
            Ok(None) => Err(Refusal::missing("there is no such record")),
            Err(err) if err.is_version_conflict() => {
                Err(Refusal::conflict("somebody changed the record first; look again and retry"))
            }
            Err(err) => Err(duplicate(err, &checked)),
        }
    }

    pub async fn remove(&self, id: &str) -> Result<Record, Refusal> {
        let record = self.found(id).await?;
        match self.0.delete(RECORDS, record.id.to_string(), None).await? {
            true => Ok(record),
            false => Err(Refusal::missing("there is no such record")),
        }
    }
}

fn duplicate(err: doc_plugin_sdk::PluginError, checked: &Checked) -> Refusal {
    match err.is_duplicate() {
        true => Refusal::conflict(format!(
            "{} already has that {} record",
            checked.name,
            checked.kind.id()
        )),
        false => err.into(),
    }
}

/// A record as the API shows it.
pub fn shown(record: &Record) -> Value {
    json!({
        "id": record.id,
        "name": record.name,
        "type": record.kind,
        "value": record.value,
        "ttl": record.ttl,
        "note": record.note,
        "changed_by": record.changed_by,
        "updated_at": record.updated_at,
    })
}
