//! What the DNS server is configured with: the address it listens on, the domains DOC owns and so
//! answers for itself, the servers every other name is forwarded to, and who may have names
//! forwarded. The records themselves are not settings: they are kept in the `records` collection
//! and changed on the plugin's page by anyone who may write to it.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};

use doc_plugin_sdk::{Feature, Setting, SettingKind, Settings, SettingsVerdict};
use ipnet::IpNet;
use serde_json::json;

use crate::names;

/// The feature that has the server listen; while it is off, nothing is bound.
pub const SERVING: &str = "serving";
/// The feature that answers DNS questions over DOC's own HTTPS (RFC 8484). It needs no port of
/// its own, so it is worth having on its own: a platform can offer DNS over HTTPS without ever
/// opening 53.
pub const HTTPS: &str = "https";
/// The feature that gives each plugin a name of its own under `PLUGIN_DOMAIN`.
pub const PLUGIN_NAMES: &str = "plugin-names";
/// The feature that names a service after the infrastructure DOC stood up for it.
pub const SERVICE_NAMES: &str = "service-names";

/// Set where the plugin runs to offer the resolver to clients that cannot sign in. It is a
/// deployment's decision rather than a setting because it also needs `dns = ["public-routes"]`
/// in the platform's configuration, and both are edited in the same place by the same person.
pub const PUBLIC_RESOLVER: &str = "DOC_DNS_PUBLIC_RESOLVER";

/// Whether this deployment offers the resolver without signing in.
pub fn public_resolver() -> bool {
    std::env::var(PUBLIC_RESOLVER)
        .map(|set| matches!(set.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

pub const LISTEN: &str = "listen";
pub const ZONES: &str = "zones";
pub const UPSTREAMS: &str = "upstreams";
pub const FORWARD_FOR: &str = "forward-for";
pub const NAMESERVERS: &str = "nameservers";
pub const NAMESERVER_ADDRESSES: &str = "nameserver-addresses";
pub const TTL: &str = "ttl";
pub const PLUGIN_DOMAIN: &str = "plugin-domain";
pub const PLUGIN_ADDRESSES: &str = "plugin-addresses";
pub const SERVICE_DOMAIN: &str = "service-domain";

/// Unprivileged, so the plugin needs no special rights to listen; publish port 53 to it.
pub const DEFAULT_LISTEN: &str = "0.0.0.0:1053";
const DEFAULT_TTL: f64 = 300.0;
/// Loopback and the private ranges: forwarding for anyone would make DOC an open resolver, which
/// others use to amplify attacks.
const DEFAULT_FORWARD_FOR: [&str; 6] =
    ["127.0.0.0/8", "::1/128", "10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "fc00::/7"];

pub fn declared() -> Vec<Setting> {
    vec![
        Setting::text(LISTEN, "Listen on")
            .defaulting(json!(DEFAULT_LISTEN))
            .hinted(
                "The address and port the server answers on, over both UDP and TCP, such as \
                 0.0.0.0:1053. Publish port 53 to it to be the name server others ask.",
            )
            .grouped("Serving"),
        Setting::new(ZONES, "Domains DOC answers for", SettingKind::List)
            .hinted(
                "One domain on each line, such as internal.example.com. DOC answers for these \
                 and every name under them from the records on its DNS page, and says a name \
                 with no record does not exist. Leave it empty for the domain DOC is configured \
                 with, [instance] domain in its configuration.",
            )
            .grouped("Serving"),
        Setting::new(NAMESERVERS, "Name servers", SettingKind::List)
            .hinted(
                "The names this server is known by, such as ns1.internal.example.com, given as \
                 each domain's NS records. The first is the primary its SOA names. Leave it \
                 empty and each domain is served by ns1 under itself, such as \
                 ns1.internal.example.com.",
            )
            .grouped("Serving"),
        Setting::new(NAMESERVER_ADDRESSES, "Where the name servers are reached", SettingKind::List)
            .hinted(
                "The addresses resolvers ask this server at — its load balancer's, or the \
                 host's — one on each line, as IPv4 or IPv6. A name server inside DOC's domains \
                 is answered with them, which a domain handed to DOC by its parent needs. This \
                 server cannot find them itself: behind a load balancer it sees only its own.",
            )
            .grouped("Serving"),
        Setting::new(TTL, "How long answers may be kept", SettingKind::Number)
            .defaulting(json!(DEFAULT_TTL))
            .between(0.0, 86_400.0)
            .hinted(
                "In seconds, for a record that doesn't say, and for how long a resolver may \
                 remember that a name does not exist.",
            )
            .grouped("Serving"),
        Setting::text(PLUGIN_DOMAIN, "Where the plugins are named")
            .hinted(
                "The domain each plugin gets a name under, such as doc.example.com: the RBAC \
                 plugin is then rbac.doc.example.com. Use the host DOC is served at, since the \
                 frontend sends only names under that host on to a plugin's page. It must be one \
                 of the domains above, or a name inside one, since DOC has to answer for it. \
                 Leave it empty for the domain DOC is configured with.",
            )
            .grouped("Plugin names")
            .of_feature(PLUGIN_NAMES),
        Setting::new(PLUGIN_ADDRESSES, "Where those names point", SettingKind::List)
            .hinted(
                "The addresses of whatever serves DOC to a browser — its load balancer, ingress \
                 or the host itself — one on each line, as IPv4 or IPv6. Opening a plugin's name \
                 lands on that plugin's page.",
            )
            .grouped("Plugin names")
            .of_feature(PLUGIN_NAMES),
        Setting::text(SERVICE_DOMAIN, "Where the services are named")
            .hinted(
                "The domain a service gets a name under, such as internal.example.com: the \
                 payments service is then payments.internal.example.com, pointing at whatever \
                 Infra stood up for it. It must be one of the domains above, or a name inside \
                 one, since DOC has to answer for it.",
            )
            .grouped("Service names")
            .of_feature(SERVICE_NAMES),
        Setting::new(UPSTREAMS, "Forward everything else to", SettingKind::List)
            .hinted(
                "The DNS servers asked about any name outside DOC's domains, tried in order, as \
                 an address with an optional port, such as 10.0.0.2 or [2001:db8::53]:53. Leave \
                 it empty and DOC refuses those names.",
            )
            .grouped("Forwarding"),
        Setting::new(FORWARD_FOR, "Forward only for", SettingKind::List)
            .defaulting(json!(DEFAULT_FORWARD_FOR))
            .hinted(
                "The networks whose questions about other names are forwarded, such as \
                 10.0.0.0/8. Anyone else is refused, so DOC is never an open resolver. DOC's own \
                 domains are answered for everyone.",
            )
            .grouped("Forwarding"),
        Setting::new(crate::guide::OWNERS, "Who keeps development-environment", SettingKind::List)
            .hinted(
                "How to use DOC's DNS from a developer's machine is written into the Knowledge \
                 Base's development-environment space. If DNS is the first to write there, this is \
                 who keeps the space: team:<name> or organisation:<name>, one on each line. Empty \
                 is the organisation, when there is only one.",
            )
            .grouped("Developer guide"),
    ]
}

pub fn features() -> Vec<Feature> {
    vec![
        Feature::new(
            SERVING,
            "DNS server",
            "Answers DNS questions about DOC's domains from the records on the DNS page, and \
             forwards questions about every other name to the servers in the settings.",
        )
        .warning(
            "This opens the UDP and TCP port under Listen on, on whichever host runs this \
             plugin. Anyone who can reach it can ask about DOC's domains.",
        ),
        Feature::new(
            PLUGIN_NAMES,
            "A name for each plugin",
            "Answers for a name per plugin under the domain in the settings — rbac.rundoc.sh for \
             the RBAC plugin — pointing at wherever DOC is served, so opening one goes to that \
             plugin's page. The names are worked out from the plugins the platform is running \
             and are not records anybody keeps: adding a plugin adds its name, and a record \
             written by hand for the same name is used instead.",
        ),
        Feature::new(
            SERVICE_NAMES,
            "A name for each service",
            "Answers for a name per service under the domain in the settings, pointing at the \
             address of whatever the Infra plugin stood up for that service. The record appears \
             when the machine starts answering and goes when it is torn down, so a service that \
             moves is still reached by the same name. These are ordinary records: one written by \
             hand is left alone, and one DOC made can be changed on this page like any other.",
        ),
        Feature::new(
            HTTPS,
            "DNS over HTTPS",
            "Answers the same questions over DOC's own HTTPS, as RFC 8484 describes, so a \
             resolver or a browser can be pointed at DOC without a plain DNS port between them. \
             It opens no port of its own and needs no certificate: it is a route on the \
             platform, and whoever may read this plugin may resolve through it.",
        ),
    ]
}

/// The settings the server runs by, read once and checked.
#[derive(Debug, Clone, PartialEq)]
pub struct Serving {
    pub on: bool,
    /// Whether questions are answered over HTTPS as well.
    pub over_https: bool,
    pub listen: SocketAddr,
    /// Each domain as `names::plain` writes it, without duplicates.
    pub zones: Vec<String>,
    /// As set; `nameservers_of` is what a domain is actually served by.
    pub nameservers: Vec<String>,
    /// Where the name servers are reached, for the ones inside DOC's domains.
    pub nameserver_addresses: Vec<IpAddr>,
    pub ttl: u32,
    pub upstreams: Vec<SocketAddr>,
    pub forward_for: Vec<IpNet>,
    /// The domain each plugin is named under, where plugin names are on and it is in a zone.
    pub plugin_domain: Option<String>,
    /// Where those names point.
    pub plugin_addresses: Vec<IpAddr>,
    /// The domain a service is named under, where service names are on and it is in a zone.
    pub service_domain: Option<String>,
    /// The domain DOC is reached at, from its configuration: what a page's addresses are under.
    pub domain: Option<String>,
    /// The machine DOC runs on, as other machines reach it, from its configuration.
    pub machine: Option<IpAddr>,
}

impl Serving {
    /// The settings as they are, with anything unreadable left out; `check` says what it was.
    pub fn read(settings: &Settings) -> Self {
        let read = Read::of(settings);
        Self {
            on: settings.feature(SERVING),
            over_https: settings.feature(HTTPS),
            listen: read.listen.unwrap_or_else(|_| default_listen()),
            zones: read.zones.into_iter().filter_map(Result::ok).collect(),
            nameservers: read.nameservers.into_iter().filter_map(Result::ok).collect(),
            nameserver_addresses: read
                .nameserver_addresses
                .into_iter()
                .filter_map(Result::ok)
                .collect(),
            ttl: read.ttl,
            upstreams: read.upstreams.into_iter().filter_map(Result::ok).collect(),
            forward_for: read.forward_for.into_iter().filter_map(Result::ok).collect(),
            plugin_domain: match settings.feature(PLUGIN_NAMES) {
                true => read.plugin_domain.ok().flatten(),
                false => None,
            },
            plugin_addresses: match settings.feature(PLUGIN_NAMES) {
                true => read.plugin_addresses.into_iter().filter_map(Result::ok).collect(),
                false => Vec::new(),
            },
            service_domain: match settings.feature(SERVICE_NAMES) {
                true => read.service_domain.ok().flatten(),
                false => None,
            },
            domain: configured_domain(settings).and_then(|domain| names::plain(&domain).ok()),
            machine: settings.instance().address.parse().ok(),
        }
    }

    /// The name a service answers to, where services are named at all and DOC answers for it.
    pub fn service_name(&self, service: &str) -> Option<String> {
        let service = service.trim().trim_start_matches("service:");
        if service.is_empty() {
            return None;
        }
        let name = format!("{service}.{}", self.service_domain.as_ref()?);
        let name = names::plain(&name).ok()?;
        self.zone_of(&name).is_some().then_some(name)
    }

    /// The name a plugin answers to, where plugins are named at all and DOC answers for it.
    pub fn plugin_name(&self, plugin: &str) -> Option<String> {
        let domain = self.plugin_domain.as_ref()?;
        if self.plugin_addresses.is_empty() {
            return None;
        }
        let name = format!("{plugin}.{domain}");
        self.zone_of(&name).is_some().then_some(name)
    }

    /// The name servers `zone` is served by: those in the settings, or ns1 under the domain
    /// itself where there are none, so a domain always has one to name.
    pub fn nameservers_of(&self, zone: &str) -> Vec<String> {
        match self.nameservers.is_empty() {
            true => names::plain(&format!("ns1.{zone}")).into_iter().collect(),
            false => self.nameservers.clone(),
        }
    }

    /// The zone `name` falls in, the deepest when zones nest.
    pub fn zone_of(&self, name: &str) -> Option<&str> {
        self.zones
            .iter()
            .filter(|zone| names::within(name, zone))
            .max_by_key(|zone| zone.len())
            .map(String::as_str)
    }

    /// Whether the table has to be kept fresh: either way of answering needs it.
    pub fn answering(&self) -> bool {
        self.on || self.over_https
    }

    /// Whether a question from `client` about another name may be forwarded.
    pub fn forwards_for(&self, client: IpAddr) -> bool {
        let client = match client {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(client, IpAddr::V4),
            v4 => v4,
        };
        !self.upstreams.is_empty() && self.forward_for.iter().any(|net| net.contains(&client))
    }
}

fn default_listen() -> SocketAddr {
    DEFAULT_LISTEN.parse().unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 1053)))
}

/// Every setting as typed, each entry read or refused with the reason.
struct Read {
    listen: Result<SocketAddr, String>,
    zones: Vec<Result<String, String>>,
    nameservers: Vec<Result<String, String>>,
    nameserver_addresses: Vec<Result<IpAddr, String>>,
    ttl: u32,
    upstreams: Vec<Result<SocketAddr, String>>,
    forward_for: Vec<Result<IpNet, String>>,
    plugin_domain: Result<Option<String>, String>,
    plugin_addresses: Vec<Result<IpAddr, String>>,
    service_domain: Result<Option<String>, String>,
}

impl Read {
    fn of(settings: &Settings) -> Self {
        let lines = |key: &str| -> Vec<String> {
            settings
                .list(key)
                .into_iter()
                .map(|line| line.trim().to_string())
                .filter(|line| !line.is_empty())
                .collect()
        };
        let listen = settings.some_text(LISTEN).map(|text| text.trim().to_string());
        let listen = match listen.filter(|text| !text.is_empty()) {
            Some(text) => text.parse::<SocketAddr>().map_err(|_| {
                format!("{text} is not an address and port, such as {DEFAULT_LISTEN}")
            }),
            None => Ok(default_listen()),
        };
        let mut zones: Vec<Result<String, String>> =
            lines(ZONES).iter().map(|zone| names::plain(zone)).collect();
        if zones.is_empty() {
            zones.extend(configured_domain(settings).map(|domain| names::plain(&domain)));
        }
        let mut seen = Vec::new();
        zones.retain(|zone| match zone {
            Ok(zone) if seen.contains(zone) => false,
            Ok(zone) => {
                seen.push(zone.clone());
                true
            }
            Err(_) => true,
        });
        let forward_for = settings
            .list(FORWARD_FOR)
            .iter()
            .map(|line| line.trim())
            .filter(|line| !line.is_empty())
            .map(network)
            .collect();
        let domain = |key: &str| match settings
            .some_text(key)
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty())
        {
            Some(text) => names::plain(&text).map(Some),
            None => Ok(None),
        };
        let plugin_domain = match domain(PLUGIN_DOMAIN) {
            Ok(None) => configured_domain(settings).map(|domain| names::plain(&domain)).transpose(),
            set => set,
        };
        let service_domain = domain(SERVICE_DOMAIN);
        let addresses = |key: &str| -> Vec<Result<IpAddr, String>> {
            lines(key)
                .iter()
                .map(|text| {
                    text.parse::<IpAddr>()
                        .map_err(|_| format!("{text} is not an IP address, such as 10.0.0.5"))
                })
                .collect()
        };
        Self {
            listen,
            zones,
            nameservers: lines(NAMESERVERS).iter().map(|name| names::plain(name)).collect(),
            nameserver_addresses: addresses(NAMESERVER_ADDRESSES),
            ttl: settings.number(TTL).unwrap_or(DEFAULT_TTL).clamp(0.0, 86_400.0) as u32,
            upstreams: lines(UPSTREAMS).iter().map(|text| upstream(text)).collect(),
            forward_for,
            plugin_domain,
            plugin_addresses: addresses(PLUGIN_ADDRESSES),
            service_domain,
        }
    }
}

/// The domain DOC is configured with (`[instance] domain`), which is what DOC answers for and names
/// its plugins under when its settings say nothing else.
fn configured_domain(settings: &Settings) -> Option<String> {
    Some(settings.instance().domain.trim().to_string()).filter(|domain| !domain.is_empty())
}

/// `10.0.0.2`, `10.0.0.2:5353`, `2001:db8::53` or `[2001:db8::53]:53`.
pub fn upstream(text: &str) -> Result<SocketAddr, String> {
    if let Ok(address) = text.parse::<SocketAddr>() {
        return Ok(address);
    }
    let bare = text.trim_start_matches('[').trim_end_matches(']');
    bare.parse::<IpAddr>().map(|ip| SocketAddr::new(ip, 53)).map_err(|_| {
        format!("{text} is not an IP address with an optional port; a server is named by address")
    })
}

/// A network, or one address as a network of its own.
fn network(text: &str) -> Result<IpNet, String> {
    text.parse::<IpNet>()
        .or_else(|_| text.parse::<IpAddr>().map(IpNet::from))
        .map_err(|_| format!("{text} is not a network, such as 10.0.0.0/8"))
}

/// The first entry of a list that could not be read, and why.
fn first<T>(entries: &[Result<T, String>]) -> Option<String> {
    entries.iter().find_map(|entry| entry.as_ref().err().cloned())
}

/// Each setting that could not be read, and why, for the Settings page to show on the field.
pub fn check(proposed: &Settings) -> SettingsVerdict {
    let read = Read::of(proposed);
    let mut problems = BTreeMap::new();
    if let Err(problem) = &read.listen {
        problems.insert(LISTEN.to_string(), problem.clone());
    }
    // A name DOC does not answer for would simply not exist, so it is said here rather than left
    // to somebody wondering why rbac.example.com never resolves. Only while its feature is on:
    // the plugin domain has a default, and a field nobody is using should not stop a save.
    let zones: Vec<&String> = read.zones.iter().flatten().collect();
    for (key, feature, read) in [
        (PLUGIN_DOMAIN, PLUGIN_NAMES, &read.plugin_domain),
        (SERVICE_DOMAIN, SERVICE_NAMES, &read.service_domain),
    ] {
        match read {
            Err(problem) => {
                problems.insert(key.to_string(), problem.clone());
            }
            Ok(Some(domain))
                if proposed.feature(feature)
                    && !zones.iter().any(|zone| names::within(domain, zone)) =>
            {
                problems.insert(
                    key.to_string(),
                    format!("{domain} is not one of the domains DOC answers for, or inside one"),
                );
            }
            Ok(_) => {}
        }
    }
    for (key, problem) in [
        (ZONES, first(&read.zones)),
        (NAMESERVERS, first(&read.nameservers)),
        (NAMESERVER_ADDRESSES, first(&read.nameserver_addresses)),
        (UPSTREAMS, first(&read.upstreams)),
        (FORWARD_FOR, first(&read.forward_for)),
        (PLUGIN_ADDRESSES, first(&read.plugin_addresses)),
    ] {
        if let Some(problem) = problem {
            problems.insert(key.to_string(), problem);
        }
    }
    // Forwarding to itself would send each question round until it timed out.
    if let Ok(listen) = &read.listen
        && let Some(own) = read.upstreams.iter().flatten().find(|upstream| {
            upstream.port() == listen.port()
                && (upstream.ip() == listen.ip()
                    || (listen.ip().is_unspecified() && upstream.ip().is_loopback()))
        })
    {
        problems
            .insert(UPSTREAMS.to_string(), format!("{own} is this server; forward to another one"));
    }
    SettingsVerdict { problems, ..SettingsVerdict::ok() }
}
