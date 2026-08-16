//! The answers DOC gives for its own domains, worked out from a snapshot of the records and the
//! settings. The server holds one `Table` at a time and swaps in a new one whenever the records or
//! the settings change, so a question never waits on the backend.
//!
//! A name under one of DOC's domains is answered with authority: its records, a CNAME followed
//! while it stays in DOC's domains, a wildcard (RFC 4592) where nothing more exact exists, and
//! otherwise "no such name" or "no records of that type" with the domain's SOA, so resolvers
//! remember the absence for as long as the settings say. Every other name is for the server to
//! forward.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::net::IpAddr;

use hickory_proto::op::{Message, MessageType, OpCode, Query as Question, ResponseCode};
use hickory_proto::rr::rdata::{NS, SOA};
use hickory_proto::rr::{DNSClass, Name, RData, Record, RecordType};

use crate::names;
use crate::settings::Serving;
use crate::store::{self, Kind};

/// How many CNAMEs are followed within DOC's domains before the answer stops where it is.
const MAX_CHAIN: usize = 8;
/// What the SOA tells a secondary server; none copies from DOC, so these are only conventional.
const REFRESH: i32 = 3_600;
const RETRY: i32 = 600;
const EXPIRE: i32 = 1_209_600;

/// What to do with a question.
pub enum Decision {
    /// Send this back.
    Answer(Message),
    /// The name is not in DOC's domains: forward it, if the asker may have that.
    Forward,
    /// Not a question at all, such as a stray response: say nothing.
    Ignore,
}

struct Entry {
    rdata: RData,
    ttl: u32,
}

pub struct Table {
    serving: Serving,
    /// Each name's records, from the records whose names are in a domain DOC answers for.
    records: HashMap<String, Vec<Entry>>,
    /// Every name with records and every name between one and its domain, which exist even with
    /// no records of their own; a name not here and not a domain does not exist.
    existing: HashSet<String>,
    serial: u32,
    fingerprint: u64,
    /// False until the records have been read: DOC's domains fail rather than claim to be empty.
    ready: bool,
    /// Records outside every domain in the settings, which are kept but not answered with.
    pub unserved: usize,
    /// The plugins this table named, so a refresh that cannot read them keeps the names it had.
    plugins: Vec<String>,
    /// Each plugin name answered for, for the page to show.
    named: Vec<String>,
}

impl Table {
    /// A table from the records as stored. `before` is the table this replaces: the SOA serial
    /// stays the same until something in the answers changes.
    pub fn new(
        serving: Serving,
        stored: &[store::Record],
        plugins: &[String],
        before: Option<&Table>,
    ) -> Self {
        let mut rows: Vec<(&str, Kind, &str, Option<u32>)> = stored
            .iter()
            .map(|record| (record.name.as_str(), record.kind, record.value.as_str(), record.ttl))
            .collect();
        rows.sort_unstable_by(|a, b| (a.0, a.1.id(), a.2).cmp(&(b.0, b.1.id(), b.2)));
        let mut hasher = DefaultHasher::new();
        (&serving.zones, &serving.nameservers, serving.ttl).hash(&mut hasher);
        let (mut records, mut existing, mut unserved) = (HashMap::new(), HashSet::new(), 0);
        // A name per plugin, worked out rather than kept: adding a plugin adds its name and
        // removing one takes it away, with nothing to write and nothing to go stale. They go in
        // first so that a record written by hand for the same name replaces this one — somebody
        // who points rbac.rundoc.sh somewhere of their own meant it.
        let named: Vec<(String, Kind, String)> = plugins
            .iter()
            .filter_map(|plugin| serving.plugin_name(plugin))
            .flat_map(|name| pointing(name, &serving.plugin_addresses))
            .collect();
        // The name servers' own addresses, where their names are in DOC's domains: a resolver the
        // parent domain sends here asks for them, and they have to agree with the parent's glue.
        // Worked out and replaced by hand in the same way as a plugin's name.
        let mut servers: Vec<String> = serving
            .zones
            .iter()
            .flat_map(|zone| serving.nameservers_of(zone))
            .filter(|server| serving.zone_of(server).is_some())
            .collect();
        servers.sort();
        servers.dedup();
        let servers: Vec<(String, Kind, String)> = servers
            .into_iter()
            .flat_map(|server| pointing(server, &serving.nameserver_addresses))
            .collect();
        let by_hand: HashSet<&str> = rows.iter().map(|(name, _, _, _)| *name).collect();
        for (name, kind, value) in named.iter().chain(&servers) {
            (name, kind.id(), value).hash(&mut hasher);
            if by_hand.contains(name.as_str()) {
                continue;
            }
            let (Some(zone), Some(rdata)) = (serving.zone_of(name), kind.rdata(value)) else {
                continue;
            };
            let mut above = Some(name.as_str());
            while let Some(at) = above.filter(|at| *at != zone) {
                existing.insert(at.to_string());
                above = names::parent(at);
            }
            records
                .entry(name.clone())
                .or_insert_with(Vec::new)
                .push(Entry { rdata, ttl: serving.ttl });
        }
        for (name, kind, value, ttl) in rows {
            (name, kind.id(), value, ttl).hash(&mut hasher);
            let (Some(zone), Some(rdata)) = (serving.zone_of(name), kind.rdata(value)) else {
                unserved += 1;
                continue;
            };
            let mut above = Some(name);
            while let Some(at) = above.filter(|at| *at != zone) {
                existing.insert(at.to_string());
                above = names::parent(at);
            }
            let ttl = ttl.unwrap_or(serving.ttl);
            records.entry(name.to_string()).or_insert_with(Vec::new).push(Entry { rdata, ttl });
        }
        let fingerprint = hasher.finish();
        let now = u32::try_from(chrono::Utc::now().timestamp()).unwrap_or(u32::MAX);
        let serial = match before {
            Some(before) if before.fingerprint == fingerprint => before.serial,
            Some(before) => now.max(before.serial.wrapping_add(1)),
            None => now,
        };
        let mut answered: Vec<String> = named
            .iter()
            .map(|(name, _, _)| name.clone())
            .filter(|name| records.contains_key(name))
            .collect();
        answered.sort();
        answered.dedup();
        Self {
            serving,
            records,
            existing,
            serial,
            fingerprint,
            ready: true,
            unserved,
            plugins: plugins.to_vec(),
            named: answered,
        }
    }

    /// A table for before the records have been read.
    pub fn unready(serving: Serving) -> Self {
        let mut table = Self::new(serving, &[], &[], None);
        table.ready = false;
        table
    }

    pub fn ready(&self) -> bool {
        self.ready
    }

    pub fn serving(&self) -> &Serving {
        &self.serving
    }

    pub fn names(&self) -> usize {
        self.records.len()
    }

    /// The plugins this table was built from.
    pub fn plugins(&self) -> &[String] {
        &self.plugins
    }

    /// The plugin names it answers for, in order.
    pub fn named(&self) -> &[String] {
        &self.named
    }

    pub fn decide(&self, request: &Message) -> Decision {
        if request.metadata.message_type != MessageType::Query {
            return Decision::Ignore;
        }
        if request.metadata.op_code != OpCode::Query {
            return Decision::Answer(failure(request, ResponseCode::NotImp));
        }
        let [question] = request.queries.as_slice() else {
            return Decision::Answer(failure(request, ResponseCode::FormErr));
        };
        let name = names::from_wire(question.name());
        let Some(zone) = self.serving.zone_of(&name) else { return Decision::Forward };
        // Zone transfers would hand every name over at once; nothing copies from DOC.
        if question.query_class() != DNSClass::IN
            || matches!(question.query_type(), RecordType::AXFR | RecordType::IXFR)
        {
            return Decision::Answer(failure(request, ResponseCode::Refused));
        }
        if !self.ready {
            return Decision::Answer(failure(request, ResponseCode::ServFail));
        }
        Decision::Answer(self.authoritative(request, question, &name, zone))
    }

    fn authoritative(
        &self,
        request: &Message,
        question: &Question,
        asked: &str,
        zone: &str,
    ) -> Message {
        let mut response = reply(request);
        response.metadata.authoritative = true;
        let wanted = question.query_type();
        let (mut owner, mut zone) = (asked.to_string(), zone.to_string());
        let mut followed = HashSet::from([owner.clone()]);
        for _ in 0..=MAX_CHAIN {
            let shown = match owner == asked {
                true => Some(question.name().clone()),
                false => names::fqdn(&owner),
            };
            let Some(shown) = shown else { break };
            let Some(found) = self.lookup(&owner, &zone, &shown) else {
                // RFC 6604: the code is about the last name in the chain.
                response.metadata.response_code = ResponseCode::NXDomain;
                response.add_authorities(self.soa(&zone));
                break;
            };
            if wanted != RecordType::CNAME
                && let Some(alias) =
                    found.iter().find(|record| record.record_type() == RecordType::CNAME)
            {
                response.add_answer(alias.clone());
                let next = store::target(&alias.data).map(names::from_wire);
                match next.and_then(|next| Some((self.serving.zone_of(&next)?.to_string(), next))) {
                    Some((next_zone, next)) if followed.insert(next.clone()) => {
                        (owner, zone) = (next, next_zone);
                        continue;
                    }
                    // Outside DOC's domains, the asker's resolver follows it from here.
                    _ => break,
                }
            }
            let matching: Vec<Record> = found
                .into_iter()
                .filter(|record| wanted == RecordType::ANY || record.record_type() == wanted)
                .collect();
            match matching.is_empty() {
                true => response.add_authorities(self.soa(&zone)),
                false => response.add_answers(matching),
            };
            break;
        }
        let extra = self.additional(&response.answers);
        response.add_additionals(extra);
        response
    }

    /// The records at `owner`, shown under the name `shown`: its own, the domain's SOA and NS at
    /// the domain itself, or a wildcard's. `None` when the name does not exist at all.
    fn lookup(&self, owner: &str, zone: &str, shown: &Name) -> Option<Vec<Record>> {
        let mut found: Vec<Record> = match owner == zone {
            true => self.soa(zone).into_iter().chain(self.ns(zone)).collect(),
            false => Vec::new(),
        };
        let entries = match self.records.get(owner) {
            Some(entries) => Some(entries),
            None if owner == zone || self.existing.contains(owner) => None,
            None => {
                // RFC 4592: the closest name above that exists decides which wildcard applies.
                let mut encloser = names::parent(owner)?;
                while encloser != zone && !self.existing.contains(encloser) {
                    encloser = names::parent(encloser)?;
                }
                Some(self.records.get(&format!("*.{encloser}"))?)
            }
        };
        let own = entries.into_iter().flatten();
        found.extend(
            own.map(|entry| Record::from_rdata(shown.clone(), entry.ttl, entry.rdata.clone())),
        );
        Some(found)
    }

    /// The domain's SOA, with the settings' TTL as how long an absence may be remembered.
    fn soa(&self, zone: &str) -> Option<Record> {
        let servers = self.serving.nameservers_of(zone);
        let primary = servers.first().map_or(zone, String::as_str);
        let soa = SOA::new(
            names::fqdn(primary)?,
            names::fqdn(&format!("hostmaster.{zone}"))?,
            self.serial,
            REFRESH,
            RETRY,
            EXPIRE,
            self.serving.ttl,
        );
        Some(Record::from_rdata(names::fqdn(zone)?, self.serving.ttl, RData::SOA(soa)))
    }

    fn ns(&self, zone: &str) -> Vec<Record> {
        let Some(owner) = names::fqdn(zone) else { return Vec::new() };
        self.serving
            .nameservers_of(zone)
            .iter()
            .filter_map(|server| names::fqdn(server))
            .map(|server| {
                Record::from_rdata(owner.clone(), self.serving.ttl, RData::NS(NS(server)))
            })
            .collect()
    }

    /// The addresses of the mail servers, services and name servers an answer points at, where
    /// they are in DOC's domains, so the asker needn't ask again.
    fn additional(&self, answers: &[Record]) -> Vec<Record> {
        let mut seen = HashSet::new();
        let mut extra = Vec::new();
        for answer in answers {
            let target = match &answer.data {
                RData::NS(NS(name)) => Some(name),
                rdata @ (RData::MX(_) | RData::SRV(_)) => store::target(rdata),
                _ => None,
            };
            let Some(target) = target else { continue };
            let name = names::from_wire(target);
            if !seen.insert(name.clone()) {
                continue;
            }
            let Some(entries) = self.serving.zone_of(&name).and(self.records.get(&name)) else {
                continue;
            };
            extra.extend(
                entries
                    .iter()
                    .filter(|entry| matches!(entry.rdata, RData::A(_) | RData::AAAA(_)))
                    .map(|entry| {
                        Record::from_rdata(target.clone(), entry.ttl, entry.rdata.clone())
                    }),
            );
        }
        extra
    }
}

/// `name` pointed at each of `addresses`, as A or AAAA by the address.
fn pointing(name: String, addresses: &[IpAddr]) -> impl Iterator<Item = (String, Kind, String)> {
    addresses.iter().map(move |address| {
        let kind = match address {
            IpAddr::V4(_) => Kind::A,
            IpAddr::V6(_) => Kind::AAAA,
        };
        (name.clone(), kind, address.to_string())
    })
}

/// A response to `request` with its question, and nothing else yet.
fn reply(request: &Message) -> Message {
    let mut response = Message::response(request.metadata.id, request.metadata.op_code);
    response.metadata.recursion_desired = request.metadata.recursion_desired;
    response.metadata.checking_disabled = request.metadata.checking_disabled;
    response.add_queries(request.queries.iter().cloned());
    response
}

/// A response to `request` saying only `code`, such as SERVFAIL or REFUSED.
pub fn failure(request: &Message, code: ResponseCode) -> Message {
    let mut response = reply(request);
    response.metadata.response_code = code;
    response
}
