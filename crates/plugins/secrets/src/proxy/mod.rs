//! The vendor proxy (ADR-0014). A caller holds a DOC key, calls DOC, and DOC makes the vendor
//! call with the account's credential. Nobody but DOC ever holds a vendor's credential, and what
//! a caller may reach is written in DOC as rules, under denials no allowance may cross.
//!
//! Everything a call needs is held here in memory as a **working set**, refreshed from core and
//! over the Event Bus. The proxy serves from it even when core is healthy, so core is never on
//! the hot path — and when core is not there at all the proxy carries on until its staleness
//! bound runs out (§11).

pub mod keys;
pub mod listen;
pub mod manage;
pub mod record;
pub mod rules;
pub mod through;
pub mod upstream;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use doc_plugin_sdk::protocol::Secret as Hidden;
use doc_plugin_sdk::{Backend, Query};
use uuid::Uuid;

use crate::directory::Directory;
use crate::store::{
    ACCOUNTS, ACTIVE, Account, Call, DENIED, NO_ACCOUNT, NO_KEY, NO_RULE, PROXIED, Store,
};
use record::Recorder;
use rules::Rule;
use upstream::Upstream;

/// How much of an address is the account's name: `/via/<account>/<path…>`.
pub const VIA: &str = "/via/";

/// The port the proxy listens on until a deployment says otherwise. Deliberately clear of the
/// range `just dev` hands plugins for their QUIC listeners, which starts at 4440: those are UDP
/// and this is TCP, so they would not actually collide, but nobody should have to know that.
pub const PORT: i64 = 8460;

/// What the proxy is configured with. Every one of these is a setting, because where the proxy
/// runs, which names it answers for and how long it may run alone are deployment decisions
/// rather than code ones.
#[derive(Debug, Clone)]
pub struct Configured {
    pub on: bool,
    pub port: u16,
    /// The address callers reach the proxy at, used to rewrite `Link` headers so pagination
    /// keeps working.
    pub public: String,
    /// The domain an account's own host sits under (ADR-0015 §1), so `github` answers at
    /// `github.<zone>`. Empty until a deployment names one, and then only the path form works.
    pub zone: String,
    pub refresh: Duration,
    /// How long the proxy will serve without hearing from the platform (§11).
    pub stale_after: Duration,
    /// How many records it will hold while core is away before it stops (§12).
    pub held_calls: usize,
}

impl Configured {
    pub fn read(backend: &Backend) -> Self {
        let settings = backend.settings();
        let seconds = |key: &str, fallback: u64| {
            settings.duration(key).map(|held| held.as_secs()).unwrap_or(fallback)
        };
        Self {
            on: backend.feature(crate::PROXY),
            port: settings.integer("proxy-port").unwrap_or(PORT).clamp(1, 65_535) as u16,
            public: settings.text("proxy-address").trim_end_matches('/').to_string(),
            zone: tidy_zone(&settings.text("proxy-zone")),
            refresh: Duration::from_secs(seconds("proxy-refresh", 60).clamp(5, 3_600)),
            stale_after: Duration::from_secs(seconds("proxy-stale-after", 4 * 3_600).max(60)),
            held_calls: settings.integer("proxy-held-calls").unwrap_or(5_000).clamp(100, 1_000_000)
                as usize,
        }
    }

    /// Where calls to `account` arrive, which is what a rewritten `Link` has to say (ADR-0015
    /// §6): a client that followed one has to land back the way it came, not at a second address
    /// that may not be reachable from where it is. Empty when there is nothing to rewrite to.
    pub fn here(&self, addressed: &Addressed, account: &str) -> String {
        match addressed {
            Addressed::Host(authority) => format!("{}://{authority}", self.scheme()),
            Addressed::Path if self.public.is_empty() => String::new(),
            Addressed::Path => format!("{}{VIA}{account}", self.public),
        }
    }

    /// The scheme a caller reached the proxy by. Taken from the public address rather than
    /// guessed, because whether TLS stops at the ingress or here is the deployment's business.
    fn scheme(&self) -> &str {
        match self.public.starts_with("http://") {
            true => "http",
            false => "https",
        }
    }

    /// The host callers reach the proxy at, which is what an account's own name points at. None
    /// when no public address is set, and then there is nothing to point a name at.
    pub fn public_host(&self) -> Option<String> {
        url::Url::parse(&self.public).ok()?.host_str().map(str::to_string)
    }
    /// Where calls go, in the words a caller gets at the proxy's own address. Both forms, because
    /// the path one is still the only one that works through core (ADR-0014 §8).
    pub fn ways(&self) -> String {
        let path = "Calls go to /via/<account>/<path>";
        match self.zone.is_empty() {
            true => format!("This is DOC's vendor proxy. {path}."),
            false => format!(
                "This is DOC's vendor proxy. Calls go to {}://<account>.{}/<path>, or {path} \
                 here.",
                self.scheme(),
                self.zone
            ),
        }
    }
}

/// The listener's own TLS, where a deployment has given it a certificate (ADR-0015 §5). `None` is
/// plain HTTP behind the deployment's ingress, which is what ADR-0014 §9 describes and what a
/// development stack runs. Whichever issuer produced the certificate, it arrives here the same
/// way (ADR-0016 §1): the chain as a setting, the key as a secret.
///
/// An `Err` is a certificate that was given and cannot be used. That stops the proxy rather than
/// quietly serving in the clear, because a deployment that configured TLS asked for TLS.
pub fn tls(backend: &Backend) -> Result<Option<Arc<rustls::ServerConfig>>, String> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};

    let settings = backend.settings();
    let chain = settings.text("proxy-certificate");
    let chain = chain.trim();
    // Blank counts as absent for both. A declared setting nobody filled in must leave the proxy
    // exactly as it was before there was a setting at all, or turning the feature on would stop
    // every deployment that terminates TLS at its ingress.
    let held = settings.secret("proxy-certificate-key");
    let key = held.as_ref().map(|key| key.expose().trim()).filter(|key| !key.is_empty());
    let (chain, key) = match (chain.is_empty(), key) {
        (true, None) => return Ok(None),
        (false, Some(key)) => (chain, key),
        (true, Some(_)) => {
            return Err("the proxy has a certificate's key but no certificate".into());
        }
        (false, None) => return Err("the proxy has a certificate but no key for it".into()),
    };
    let certificates = CertificateDer::pem_slice_iter(chain.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("the proxy's certificate could not be read: {err}"))?;
    if certificates.is_empty() {
        return Err("the proxy's certificate has nothing in it".into());
    }
    let private = PrivateKeyDer::from_pem_slice(key.as_bytes())
        .map_err(|err| format!("the proxy's certificate key could not be read: {err}"))?;
    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificates, private)
        .map_err(|err| format!("the proxy's certificate and its key do not go together: {err}"))?;
    // Both, in the order a caller should prefer them: §9's streaming path wants HTTP/2 where the
    // client can manage it, and a client that cannot is not turned away.
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Some(Arc::new(config)))
}
/// A zone as a suffix can be compared against: lowercase, with no dot at either end.
fn tidy_zone(zone: &str) -> String {
    zone.trim().trim_matches('.').to_ascii_lowercase()
}

/// How a caller named the account. Which one it was decides what a rewritten `Link` says, and
/// whether the path is the vendor's whole path or has the account's name on the front.
#[derive(Debug, Clone)]
pub enum Addressed {
    /// By a host of the account's own (ADR-0015 §1), kept as the caller wrote it.
    Host(String),
    /// By the `/via/<account>/` prefix of ADR-0014 §8.
    Path,
}

/// The account a `Host` names, when the host is one of the proxy's own: `<account>.<zone>`.
/// One label and no deeper, because an account's name is one label (ADR-0015 §2). `None` for
/// anything else, which leaves the path form to read the address instead.
pub fn named_by_host(authority: &str, zone: &str) -> Option<String> {
    let zone = tidy_zone(zone);
    if zone.is_empty() {
        return None;
    }
    let authority = authority.trim().trim_end_matches('.').to_ascii_lowercase();
    // A literal address is never one of these names, and bracketed IPv6 would defeat the port
    // split below.
    if authority.starts_with('[') {
        return None;
    }
    let host = authority.split(':').next().unwrap_or_default();
    let label = host.strip_suffix(&format!(".{zone}"))?;
    match !label.is_empty() && !label.contains('.') {
        true => Some(label.to_string()),
        false => None,
    }
}

/// One proxied account, ready to serve: where its calls go, the credential that goes with them,
/// what is denied on it, and what each of its allowances grants.
pub struct Held {
    pub id: Uuid,
    pub name: String,
    pub upstream: Upstream,
    /// In memory alone, never written anywhere. A replica that starts while core is away starts
    /// without this and serves nothing (§11).
    credential: Hidden<String>,
    denials: Vec<Rule>,
    allowances: BTreeMap<Uuid, Grant>,
}

/// What one allowance grants on an account, with whom it covers already worked out.
struct Grant {
    rules: Vec<Rule>,
    covers: Covering,
    /// Who it names, in words, for the record of a call made with no key to copy it from.
    label: String,
}

/// Who an allowance covers (§6), resolved when the working set was filled rather than per call.
enum Covering {
    /// The references it came to: a team's members, an organisation's people, one person, one
    /// service account, one plugin.
    These(BTreeSet<String>),
    /// A permission or an attribute. Whether a caller holds one is core's answer, not this
    /// plugin's, and asking it per call is the protocol addition ADR-0014 leaves open — so it is
    /// settled when the key is issued, and the key is what is revoked.
    WhenIssued,
}

impl Covering {
    fn covers(&self, subject: &str) -> bool {
        match self {
            Self::These(references) => references.contains(subject),
            Self::WhenIssued => true,
        }
    }

    /// Whether it names `subject` outright: a permission or attribute never covers a keyless call.
    fn names(&self, subject: &str) -> bool {
        matches!(self, Self::These(references) if references.contains(subject))
    }
}

/// A DOC key as the proxy holds it: never the key, only what it opens.
pub struct Opening {
    pub id: Uuid,
    pub account: Uuid,
    pub allowance: Uuid,
    pub subject: String,
    pub subject_label: String,
    pub expires_at: DateTime<Utc>,
}

/// Everything the proxy knows, as of one moment.
#[derive(Default)]
pub struct Set {
    pub filled_at: Option<DateTime<Utc>>,
    accounts: BTreeMap<Uuid, Held>,
    /// A name belongs to one account, or to none when two accounts share it. A name is half the
    /// address callers write, so an ambiguous one is refused rather than guessed at.
    by_name: BTreeMap<String, Option<Uuid>>,
    keys: BTreeMap<String, Opening>,
    /// Denials written on the instance, which are on every account.
    everywhere: Vec<Rule>,
}

impl Set {
    fn account(&self, named: &str) -> Option<&Held> {
        if let Ok(id) = named.parse::<Uuid>() {
            return self.accounts.get(&id);
        }
        self.by_name.get(named).copied().flatten().and_then(|id| self.accounts.get(&id))
    }

    /// The denial covering a call, and why, before any rule: no allowance crosses one (§5).
    fn denied(&self, held: &Held, method: &str, path: &str) -> Option<(String, String)> {
        rules::built_in()
            .iter()
            .find(|rule| rule.covers(method, path))
            .map(|rule| (rule.shown(), rules::MINTING_DENIAL.to_string()))
            .or_else(|| {
                self.everywhere
                    .iter()
                    .chain(held.denials.iter())
                    .find(|rule| rule.covers(method, path))
                    .map(|rule| (rule.shown(), "a denial covers it".to_string()))
            })
    }
}

/// A call the rules allow, with everything it takes to make it.
pub struct Going {
    pub upstream: Upstream,
    pub credential: Hidden<String>,
    pub path: String,
    pub query: String,
    pub account: String,
}

/// What the proxy decided, and the record it leaves whichever way it went. There is always a
/// record: a refusal is as much of an audit row as a call that went through.
pub struct Verdict {
    pub call: Call,
    pub going: Option<Going>,
    pub status: u16,
    pub detail: String,
}

impl Verdict {
    fn no(mut call: Call, status: u16, outcome: &str, detail: &str) -> Self {
        call.outcome = outcome.to_string();
        call.detail = detail.to_string();
        Self { call, going: None, status, detail: detail.to_string() }
    }

    /// A call a rule allowed, ready to go to the vendor.
    fn allowed(mut call: Call, held: &Held, rule: &Rule, path: &str, query: &str) -> Self {
        call.rule = rule.shown();
        call.outcome = crate::store::ALLOWED.to_string();
        Self {
            going: Some(Going {
                upstream: held.upstream.clone(),
                credential: held.credential.clone(),
                path: path.to_string(),
                query: query.to_string(),
                account: held.name.clone(),
            }),
            call,
            status: 200,
            detail: String::new(),
        }
    }
}

/// The working set, the audit buffer and the settings, shared by every connection the listener
/// holds. Filling it is the only thing here that talks to core.
pub struct Working {
    pub set: tokio::sync::RwLock<Set>,
    pub recorder: Arc<Recorder>,
    pub configured: Configured,
    /// Which replica this is, so an incident can find the one that held a stream (§10).
    pub replica: String,
    /// What the listener presents, where it terminates TLS itself (ADR-0015 §5).
    pub tls: Option<Arc<rustls::ServerConfig>>,
    filling: AtomicBool,
}

impl Working {
    pub fn new(configured: Configured, tls: Option<Arc<rustls::ServerConfig>>) -> Self {
        let replica = std::env::var("HOSTNAME").unwrap_or_else(|_| "secrets".to_string());
        Self {
            recorder: Arc::new(Recorder::new(configured.held_calls)),
            set: tokio::sync::RwLock::new(Set::default()),
            configured,
            tls,
            replica,
            filling: AtomicBool::new(false),
        }
    }

    /// Whether it has ever read the rules. A replica that has not is starting, not stale, and a
    /// caller should be told which.
    pub async fn ready(&self) -> bool {
        self.set.read().await.filled_at.is_some()
    }

    /// Whether what it holds is recent enough to act on. Past the bound the proxy refuses rather
    /// than serving a policy it can no longer vouch for (§11).
    pub async fn fresh(&self) -> bool {
        let set = self.set.read().await;
        match set.filled_at {
            Some(at) => {
                Utc::now().signed_duration_since(at).to_std().unwrap_or_default()
                    < self.configured.stale_after
            }
            None => false,
        }
    }

    /// Reads everything the proxy serves from out of core, and swaps it in whole. Nothing is
    /// served from a half-filled set, so a refresh that fails leaves the last good one in place.
    pub async fn fill(&self, backend: &Backend) -> Result<usize, crate::Refusal> {
        if self.filling.swap(true, Ordering::SeqCst) {
            return Ok(0);
        }
        let filled = self.filled(backend).await;
        self.filling.store(false, Ordering::SeqCst);
        let set = filled?;
        let held = set.accounts.len();
        *self.set.write().await = set;
        Ok(held)
    }

    async fn filled(&self, backend: &Backend) -> Result<Set, crate::Refusal> {
        let store = Store(backend);
        let directory = Directory::read(backend).await?;
        let accounts: Vec<Account> = backend
            .query_all(Query::new(ACCOUNTS).filter(serde_json::json!({ "vendor": PROXIED })))
            .await?;
        let allowances = store.allowances(None).await?;
        let denials = store.denials(None).await?;
        let living = store
            .keys(serde_json::json!({ "state": ACTIVE }))
            .await?
            .into_iter()
            .filter(|key| key.expires_at > Utc::now());

        let everywhere: Vec<Rule> =
            denials.iter().filter(|denial| denial.account.is_none()).map(Rule::of).collect();
        let mut set = Set { filled_at: Some(Utc::now()), everywhere, ..Set::default() };

        for account in accounts {
            let upstream = match Upstream::read(&account.config) {
                Ok(upstream) => upstream,
                Err(wrong) => {
                    tracing::warn!(account = %account.id, wrong, "a proxied account is unusable");
                    continue;
                }
            };
            let credential = match crate::ops::open(
                backend,
                ACCOUNTS,
                account.id,
                &account.label(),
                account.sealed.as_ref(),
            )
            .await
            {
                Ok(credential) => credential,
                Err(wrong) => {
                    tracing::warn!(account = %account.id, wrong, "an account's credential would not open");
                    continue;
                }
            };
            let mut granted = BTreeMap::new();
            for allowance in allowances.iter().filter(|held| held.account == account.id) {
                let rules = Rule::all(&allowance.rules);
                if rules.is_empty() {
                    continue;
                }
                let label = match allowance.who_label.is_empty() {
                    true => directory.label(&allowance.who),
                    false => allowance.who_label.clone(),
                };
                granted.insert(
                    allowance.id,
                    Grant { rules, covers: covering(&directory, &allowance.who), label },
                );
            }
            match set.by_name.entry(account.name.clone()) {
                std::collections::btree_map::Entry::Vacant(spare) => {
                    spare.insert(Some(account.id));
                }
                std::collections::btree_map::Entry::Occupied(mut taken) => {
                    tracing::warn!(
                        name = account.name,
                        "two proxied accounts share a name, so neither can be addressed by it"
                    );
                    taken.insert(None);
                }
            }
            set.accounts.insert(
                account.id,
                Held {
                    id: account.id,
                    name: account.name.clone(),
                    upstream,
                    credential,
                    denials: denials
                        .iter()
                        .filter(|denial| denial.account == Some(account.id))
                        .map(Rule::of)
                        .collect(),
                    allowances: granted,
                },
            );
        }

        for key in living {
            set.keys.insert(
                key.digest.clone(),
                Opening {
                    id: key.id,
                    account: key.account,
                    allowance: key.allowance,
                    subject: key.subject.clone(),
                    subject_label: key.subject_label.clone(),
                    expires_at: key.expires_at,
                },
            );
        }
        Ok(set)
    }

    /// Decides one call, and starts its record. Nothing here reads a body or asks core anything.
    pub async fn decide(
        &self,
        named: &str,
        presented: Option<&str>,
        method: &str,
        path: &str,
        query: &str,
    ) -> Verdict {
        let mut call = record::started(&self.replica);
        call.account_name = named.to_string();
        call.method = method.to_ascii_uppercase();
        call.path = path.to_string();
        call.query = keys_of(query);
        call.who = "unknown".to_string();

        let set = self.set.read().await;
        let Some(held) = set.account(named) else {
            return Verdict::no(call, 404, NO_ACCOUNT, "there is no such account here");
        };
        call.account = Some(held.id);
        call.account_name = held.name.clone();
        call.vendor = held.upstream.host.clone();

        let Some(opening) = keys::presented(presented)
            .and_then(|key| set.keys.get(&keys::digest_of(&key)))
            .filter(|opening| opening.expires_at > Utc::now())
        else {
            return Verdict::no(call, 401, NO_KEY, "send a DOC key this account knows");
        };
        call.key = Some(opening.id);
        call.who = opening.subject.clone();
        call.who_label = opening.subject_label.clone();
        if opening.account != held.id {
            return Verdict::no(call, 403, NO_KEY, "that key is for another account");
        }

        if let Some((rule, why)) = set.denied(held, method, path) {
            call.denial = rule;
            return Verdict::no(call, 403, DENIED, &why);
        }

        call.allowance = Some(opening.allowance);
        let Some(grant) = held.allowances.get(&opening.allowance) else {
            return Verdict::no(call, 403, NO_RULE, "the allowance this key came from is gone");
        };
        if !grant.covers.covers(&opening.subject) {
            return Verdict::no(call, 403, NO_RULE, "the allowance no longer covers you");
        }
        let Some(rule) = grant.rules.iter().find(|rule| rule.covers(method, path)) else {
            return Verdict::no(call, 403, NO_RULE, "no rule allows that here");
        };
        Verdict::allowed(call, held, rule, path, query)
    }

    /// Decides a keyless call a plugin makes as itself: any allowance naming it, under the denials.
    pub async fn decide_as(
        &self,
        named: &str,
        subject: &str,
        method: &str,
        path: &str,
        query: &str,
    ) -> Verdict {
        let mut call = record::started(&self.replica);
        call.account_name = named.to_string();
        call.method = method.to_ascii_uppercase();
        call.path = path.to_string();
        call.query = keys_of(query);
        call.who = subject.to_string();
        call.who_label = subject.to_string();

        let set = self.set.read().await;
        let Some(held) = set.account(named) else {
            return Verdict::no(call, 404, NO_ACCOUNT, "there is no such account here");
        };
        call.account = Some(held.id);
        call.account_name = held.name.clone();
        call.vendor = held.upstream.host.clone();

        if let Some((rule, why)) = set.denied(held, method, path) {
            call.denial = rule;
            return Verdict::no(call, 403, DENIED, &why);
        }
        let naming: Vec<(&Uuid, &Grant)> =
            held.allowances.iter().filter(|(_, grant)| grant.covers.names(subject)).collect();
        let Some((_, first)) = naming.first() else {
            return Verdict::no(call, 403, NO_RULE, "no allowance on this account names you");
        };
        call.who_label = first.label.clone();
        let found = naming.iter().find_map(|(id, grant)| {
            grant.rules.iter().find(|rule| rule.covers(method, path)).map(|rule| (*id, grant, rule))
        });
        let Some((id, grant, rule)) = found else {
            return Verdict::no(call, 403, NO_RULE, "no rule allows that here");
        };
        call.allowance = Some(*id);
        call.who_label = grant.label.clone();
        Verdict::allowed(call, held, rule, path, query)
    }
}

/// Who an allowance covers, worked out once per refresh. A team is its people, an organisation
/// is everybody in it, and the three singular kinds are themselves.
fn covering(directory: &Directory, who: &str) -> Covering {
    let Some((kind, id)) = who.split_once(':') else { return Covering::These(BTreeSet::new()) };
    let uuid = id.parse::<Uuid>().ok();
    match (kind, uuid) {
        ("permission" | "attribute", _) => Covering::WhenIssued,
        ("user" | "service", Some(id)) => {
            Covering::These([format!("{kind}:{id}")].into_iter().collect())
        }
        ("plugin", _) => Covering::These([format!("plugin:{id}")].into_iter().collect()),
        ("team", Some(team)) => Covering::These(
            directory
                .people
                .values()
                .filter(|person| !person.disabled && directory.within(person.id, team))
                .map(|person| format!("user:{}", person.id))
                .collect(),
        ),
        ("organisation", Some(organisation)) => Covering::These(
            directory
                .people
                .values()
                .filter(|person| !person.disabled && person.organisation_id == Some(organisation))
                .map(|person| format!("user:{}", person.id))
                .collect(),
        ),
        _ => Covering::These(BTreeSet::new()),
    }
}

/// The query's keys, never its values (§12): enough to say what kind of call it was, without
/// keeping something as likely to be data as not.
fn keys_of(query: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for (key, _) in url::form_urlencoded::parse(query.trim_start_matches('?').as_bytes()) {
        let key = key.trim().to_string();
        if !key.is_empty() && !found.contains(&key) {
            found.push(key);
        }
    }
    found
}
