//! DNS over HTTPS (RFC 8484): the same questions the server answers on its port, sent as HTTP on
//! DOC's own HTTPS instead.
//!
//! There is no listener and no certificate here. A DoH endpoint is a route like any other, so it
//! is reached over whatever TLS the platform is served with, and whoever may read this plugin may
//! resolve through it. That is also what replaces the address-based rule the DNS port uses: the
//! platform does not pass a caller's IP address to a plugin, so `forward-for` cannot be applied
//! here, and who the caller signed in as is used instead.
//!
//! Two routes, and the difference between them is the whole of the safety argument:
//!
//! - `api/dns-query` — signed in. The caller is a DOC principal who can read this plugin, so a
//!   name outside DOC's domains is forwarded, as it would be for an allowed network.
//! - `public/dns-query` — nobody signed in, for resolvers and browsers that cannot. It answers
//!   **only** for DOC's own domains and refuses everything else — with a DNS REFUSED, the way the
//!   port does — so opening it cannot make DOC an open resolver. It exists only where the
//!   deployment asked for it twice over: `dns` allowed
//!   `public-routes` in the platform's configuration, and `DOC_DNS_PUBLIC_RESOLVER` set where the
//!   plugin runs.

use base64::Engine;
use doc_plugin_sdk::{Request, Response};
use hickory_proto::op::Message;

use crate::server::{NotResolved, Server};
use crate::settings::{self, Serving};

/// What a DNS message is carried as, both ways.
pub const MEDIA: &str = "application/dns-message";
/// The most a question may be. A DNS message cannot exceed 65 535 bytes, and a question is tiny;
/// anything approaching the limit is not one.
const MAX_QUESTION: usize = 8 * 1024;
/// What a resolver may cache an answer for when nothing in it says: RFC 8484 asks for the smallest
/// TTL among its records, and an answer with no records at all is not cached.
const NO_CACHE: &str = "no-store";

/// The base the platform is reached at from outside: the domain it is configured with, over HTTPS,
/// else the API's public address as the other plugins read it.
fn base(serving: &Serving) -> String {
    if let Some(domain) = &serving.domain {
        return format!("https://{domain}");
    }
    std::env::var("DOC_API_PUBLIC_URL")
        .ok()
        .filter(|base| !base.trim().is_empty())
        .unwrap_or_default()
        .trim_end_matches('/')
        .to_string()
}

/// The URI template a client is given, as RFC 8484 describes it.
pub fn url(serving: &Serving, public: bool) -> String {
    let surface = if public { "public" } else { "api" };
    format!("{}/api/v1/plugins/{}/{surface}/dns-query", base(serving), crate::ID)
}

/// The template for the route anyone may use, where this deployment offers one.
pub fn public_url(serving: &Serving) -> Option<String> {
    settings::public_resolver().then(|| url(serving, true))
}

/// A question as `?dns=` carries it: base64url, and RFC 8484 says without padding. Padding is
/// accepted anyway, since a client that sends it is otherwise doing nothing wrong.
fn from_query(query: &str) -> Option<Vec<u8>> {
    let asked = url::form_urlencoded::parse(query.as_bytes())
        .find(|(name, _)| name == "dns")
        .map(|(_, value)| value.into_owned())?;
    let trimmed = asked.trim_end_matches('=');
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(trimmed).ok()
}

fn refused(status: u16, kind: &str, detail: &str) -> Response {
    Response::problem(status, kind, detail)
}

/// How long the answer may be kept: the smallest TTL in it, as RFC 8484 §5.1 asks, so a resolver
/// does not hold a name longer than the zone says it may.
fn cache_for(answer: &[u8]) -> String {
    let Ok(message) = Message::from_vec(answer) else { return NO_CACHE.to_string() };
    let smallest = message
        .answers
        .iter()
        .chain(&message.authorities)
        .chain(&message.additionals)
        .map(|record| record.ttl)
        .min();
    match smallest {
        Some(ttl) => format!("max-age={ttl}"),
        None => NO_CACHE.to_string(),
    }
}

/// One question, however it arrived.
async fn answer(server: &Server, question: &[u8], forwarding: bool) -> Response {
    if question.is_empty() {
        return refused(400, "bad-request", "no DNS question was sent");
    }
    if question.len() > MAX_QUESTION {
        return refused(413, "too-large", "a DNS question is smaller than this");
    }
    match server.resolve(question, forwarding).await {
        Ok(answer) => {
            let cache = cache_for(&answer);
            Response::new(200, MEDIA, answer).with_header("cache-control", &cache)
        }
        Err(NotResolved::Off) => refused(
            503,
            "unavailable",
            "DNS over HTTPS is off: turn it on under this plugin's Features tab",
        ),
        Err(NotResolved::Malformed) => {
            refused(400, "bad-request", "that is not a DNS message this can read")
        }
        Err(NotResolved::NotAQuestion) => {
            refused(400, "bad-request", "a DNS message sent here must be a question")
        }
    }
}

/// `GET …/dns-query?dns=<base64url>` and `POST …/dns-query` with the question as the body.
/// `forwarding` is false on the route nobody signs in to, which answers only for DOC's domains.
pub async fn handle(server: &Server, request: &Request, forwarding: bool) -> Response {
    // Said before anything else, since a client that asked for something else will not read it.
    if let Some(accept) = request.headers.get("accept")
        && !accept.is_empty()
        && !accept.contains(MEDIA)
        && !accept.contains("*/*")
        && !accept.contains("application/*")
    {
        return refused(406, "not-acceptable", &format!("this answers with {MEDIA}"));
    }
    match request.method.as_str() {
        "GET" => match from_query(&request.query) {
            Some(question) => answer(server, &question, forwarding).await,
            None => refused(
                400,
                "bad-request",
                "ask with ?dns=<the question, base64url without padding>, or POST it as \
                 application/dns-message",
            ),
        },
        "POST" => {
            let kind = request.headers.get("content-type").map_or("", String::as_str);
            // The media type alone, without a charset or anything else after it.
            let kind = kind.split(';').next().unwrap_or_default().trim();
            if !kind.is_empty() && !kind.eq_ignore_ascii_case(MEDIA) {
                return refused(415, "unsupported-media-type", &format!("send {MEDIA}"));
            }
            answer(server, &request.body, forwarding).await
        }
        _ => refused(405, "method-not-allowed", "a question is sent with GET or POST"),
    }
}
