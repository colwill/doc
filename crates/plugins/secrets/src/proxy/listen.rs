//! The data path (ADR-0014 §9). Bulk does not go through core: this is the plugin's own TCP
//! listener, behind the deployment's ingress, speaking HTTP/1.1 and HTTP/2 where every client
//! already works and a clone runs at line rate.
//!
//! Nothing is buffered. The request body is forwarded as it arrives and the answer returned as
//! it arrives, so a four-gigabyte artifact costs a buffer rather than four gigabytes. Everything
//! that decides whether the call may be made is read from the head before a byte of body moves,
//! which is why rules are method-and-path.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use http::header::{AUTHORIZATION, CONTENT_TYPE, HOST};
use http::{HeaderMap, HeaderValue, Response, StatusCode};
use http_body_util::{BodyExt, BodyStream, Full, StreamBody, combinators::BoxBody};
use hyper::body::{Body as _, Frame, Incoming};
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_rustls::TlsAcceptor;

use super::record::Ledger;
use super::{Addressed, Going, VIA, Working};
use crate::store::FAILED;

/// What the proxy answers with. The error type is an I/O one because a stream that breaks
/// halfway is exactly that, and hyper needs somewhere to put it.
type Body = BoxBody<Bytes, std::io::Error>;

/// DOC's own ID for a call, sent to the vendor and kept on the record, so the two logs join
/// (§12). Also returned to the caller, so they can quote it when asking what happened.
pub(super) const CORRELATION: &str = "x-doc-call";

/// How long DOC waits for the vendor to start answering, and then for each further piece of the
/// answer. A stream is allowed to take as long as it takes; silence is not.
const CONNECT: Duration = Duration::from_secs(10);
const QUIET: Duration = Duration::from_secs(120);

/// Idle upstream connections kept per vendor, so a burst of small calls does not pay a handshake
/// each (§10).
const POOLED: usize = 32;

/// The request headers a caller may send on. A safelist rather than a denylist: anything a
/// vendor needs beyond these is named on the account, so adding one is a decision somebody makes.
const SENT: [&str; 16] = [
    "accept",
    "accept-encoding",
    "accept-language",
    "cache-control",
    "content-encoding",
    "content-length",
    "content-type",
    "git-protocol",
    "if-match",
    "if-modified-since",
    "if-none-match",
    "if-unmodified-since",
    "range",
    "traceparent",
    "tracestate",
    "user-agent",
];

/// The response headers that come back. `location` is here because redirects are returned rather
/// than followed (§8), and the rate-limit ones because a caller should be able to see what is
/// left of a bucket they share (§14).
const BACK: [&str; 15] = [
    "accept-ranges",
    "cache-control",
    "content-disposition",
    "content-encoding",
    "content-language",
    "content-length",
    "content-range",
    "content-type",
    "etag",
    "expires",
    "last-modified",
    "link",
    "location",
    "retry-after",
    "vary",
];

fn returned(name: &str) -> bool {
    BACK.contains(&name) || name.starts_with("x-ratelimit-") || name.starts_with("x-rate-limit-")
}

/// The client DOC makes every upstream call with. One per process, pooled and bounded, and it
/// never follows a redirect: a signed object-store URL is the caller's to fetch, and it carries
/// its own auth rather than DOC's.
pub fn outbound() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(CONNECT)
        .read_timeout(QUIET)
        .pool_max_idle_per_host(POOLED)
        .tcp_keepalive(Duration::from_secs(60))
        .user_agent("doc-proxy")
        .build()
        .map_err(|err| format!("no HTTP client for the proxy: {err}"))
}

/// Listens until told to stop. Every call is independent, so nothing here holds state and a
/// replica can be added or taken away without ceremony (§10).
pub async fn serve(working: Arc<Working>, mut stop: watch::Receiver<bool>) -> Result<(), String> {
    let client = outbound()?;
    let address = SocketAddr::from(([0, 0, 0, 0], working.configured.port));
    let listener = bound(address, &mut stop).await?;
    tracing::info!(%address, "the vendor proxy is listening");
    loop {
        if *stop.borrow() {
            break;
        }
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            _ = stop.changed() => break,
        };
        let stream = match accepted {
            Ok((stream, _)) => stream,
            Err(err) => {
                tracing::warn!(%err, "a caller could not be accepted");
                continue;
            }
        };
        let (working, client) = (working.clone(), client.clone());
        tokio::spawn(async move {
            // The handshake happens here rather than in the accept loop, so a caller that opens a
            // connection and then says nothing holds up nobody but itself.
            let ended = match working.tls.clone() {
                Some(config) => match TlsAcceptor::from(config).accept(stream).await {
                    Ok(stream) => held(TokioIo::new(stream), working, client).await,
                    Err(err) => {
                        tracing::debug!(%err, "a caller's TLS handshake did not finish");
                        return;
                    }
                },
                None => held(TokioIo::new(stream), working, client).await,
            };
            if let Err(err) = ended {
                tracing::debug!(%err, "a proxied connection ended");
            }
        });
    }
    tracing::info!("the vendor proxy has stopped listening");
    Ok(())
}

/// One connection, however it was wrapped, served until the caller is done with it.
async fn held<I>(
    io: I,
    working: Arc<Working>,
    client: reqwest::Client,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    auto::Builder::new(TokioExecutor::new())
        .serve_connection(
            io,
            service_fn(move |request| {
                let (working, client) = (working.clone(), client.clone());
                async move { Ok::<_, Infallible>(answer(working, client, request).await) }
            }),
        )
        .await
}

/// Takes the port, waiting for it if the last listener has not let go yet. A settings change
/// reloads the plugin, so the new listener asks for the port at about the moment the old one is
/// dropping it: a first refusal there is ordinary rather than fatal.
async fn bound(
    address: SocketAddr,
    stop: &mut watch::Receiver<bool>,
) -> Result<TcpListener, String> {
    const TRIES: usize = 30;
    const PAUSE: Duration = Duration::from_millis(100);
    let mut last = String::new();
    for _ in 0..TRIES {
        match TcpListener::bind(address).await {
            Ok(listener) => return Ok(listener),
            Err(err) => last = err.to_string(),
        }
        tokio::select! {
            _ = tokio::time::sleep(PAUSE) => {}
            _ = stop.changed() => break,
        }
    }
    Err(format!("the proxy could not listen on {address}: {last}"))
}

async fn answer(
    working: Arc<Working>,
    client: reqwest::Client,
    request: hyper::Request<Incoming>,
) -> Response<Body> {
    let path = request.uri().path().to_string();
    let query = request.uri().query().unwrap_or_default().to_string();
    // What the caller asked for, from the `:authority` of an HTTP/2 call or the `Host` of an
    // HTTP/1.1 one. Either way it is what they typed, which is what a `Link` has to point back at.
    let authority = request
        .uri()
        .authority()
        .map(|authority| authority.as_str().to_string())
        .or_else(|| {
            request.headers().get(HOST).and_then(|value| value.to_str().ok()).map(str::to_string)
        })
        .unwrap_or_default();
    let (named, upstream, addressed) =
        match super::named_by_host(&authority, &working.configured.zone) {
            // ADR-0015 §1: the host names the account, so the whole path belongs to the vendor
            // and the proxy keeps nothing of its own here (§7).
            Some(named) => (named, path, Addressed::Host(authority)),
            // Otherwise ADR-0014 §8's form, which is still the only one that works through core.
            None => {
                if !path.starts_with(VIA) {
                    let ways = working.configured.ways();
                    return match path.as_str() {
                        "/" => said(200, &ways),
                        _ => said(404, &ways),
                    };
                }
                let rest = &path[VIA.len()..];
                match rest.split_once('/') {
                    Some((named, rest)) => (named.to_string(), format!("/{rest}"), Addressed::Path),
                    None => (rest.to_string(), "/".to_string(), Addressed::Path),
                }
            }
        };

    // Nothing is proxied that cannot be written down (§12). This is the one refusal that leaves
    // no record, and it leaves none because nothing happened: no credential was used and the
    // vendor was never called. The plugin reports it as an error rather than swallowing it.
    if !working.recorder.room() {
        return said(503, "DOC cannot record calls at the moment, so it is not making any.");
    }
    // Past the staleness bound the proxy stops rather than serving a policy it can no longer
    // vouch for (§11). A replica that has not read the rules yet is starting, not stale.
    if !working.fresh().await {
        let starting = !working.ready().await;
        let why = match starting {
            true => "the proxy has not read its rules yet",
            false => "the proxy has not heard from the platform for too long",
        };
        let mut call = super::record::started(&working.replica);
        call.account_name = named.clone();
        call.method = request.method().to_string();
        call.path = upstream.clone();
        call.who = "unknown".to_string();
        call.outcome = FAILED.to_string();
        call.detail = why.to_string();
        working.recorder.write(call);
        return said(503, &format!("{}{why}.", "DOC cannot allow this yet: "));
    }

    let presented = request.headers().get(AUTHORIZATION).and_then(|value| value.to_str().ok());
    let verdict =
        working.decide(&named, presented, request.method().as_str(), &upstream, &query).await;
    let correlation = verdict.call.correlation.clone();
    let ledger = Arc::new(Ledger::new(verdict.call, working.recorder.clone()));
    let Some(going) = verdict.going else {
        drop(ledger);
        return said(verdict.status, &verdict.detail).with(CORRELATION, &correlation);
    };
    let here = working.configured.here(&addressed, &going.account);
    forward(client, request, going, ledger, correlation, &here).await
}

async fn forward(
    client: reqwest::Client,
    request: hyper::Request<Incoming>,
    going: Going,
    ledger: Arc<Ledger>,
    correlation: String,
    here: &str,
) -> Response<Body> {
    let Going { upstream, credential, path, query, .. } = going;
    let address = upstream.address(&path, &query, &credential);
    let method = request.method().clone();
    let empty = request.body().size_hint().exact() == Some(0);

    let mut outbound = client.request(method, address);
    for (name, value) in request.headers() {
        let name = name.as_str().to_ascii_lowercase();
        if SENT.contains(&name.as_str()) || upstream.extra.contains(&name) {
            outbound = outbound.header(name, value.clone());
        }
    }
    // The caller's own key is DOC's and stops here; the account's credential goes on instead.
    if let Some((name, value)) = upstream.header(&credential) {
        outbound = outbound.header(name, value);
    }
    outbound = outbound.header(CORRELATION, &correlation);
    if !empty {
        let counting = {
            let ledger = ledger.clone();
            BodyStream::new(request.into_body()).map(move |frame| {
                frame
                    .map(|frame| {
                        let data = frame.into_data().unwrap_or_default();
                        ledger.sent(data.len());
                        data
                    })
                    .map_err(std::io::Error::other)
            })
        };
        outbound = outbound.body(reqwest::Body::wrap_stream(counting));
    }

    let answered = match outbound.send().await {
        Ok(answered) => answered,
        Err(err) => {
            ledger.failed(&format!("the vendor could not be reached: {err}"));
            drop(ledger);
            return said(502, "DOC could not reach the vendor.").with(CORRELATION, &correlation);
        }
    };
    let status = answered.status();
    ledger.answered(status.as_u16(), vendor_call(answered.headers()));

    let (headers, body) = returning(answered, ledger, here, &upstream.base);
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response.with(CORRELATION, &correlation)
}

/// The answer's body, streamed back as it arrives, with the headers that are allowed to travel
/// and `Link` pointed back at the proxy so pagination does not send the caller to the vendor.
fn returning(
    answered: reqwest::Response,
    ledger: Arc<Ledger>,
    here: &str,
    base: &str,
) -> (HeaderMap, Body) {
    let mut headers = HeaderMap::new();
    for (name, value) in answered.headers() {
        let lower = name.as_str().to_ascii_lowercase();
        if !returned(&lower) {
            continue;
        }
        let value = match (lower.as_str(), here.is_empty()) {
            ("link", false) => rewritten(value, here, base),
            _ => Some(value.clone()),
        };
        if let (Ok(name), Some(value)) = (http::HeaderName::try_from(lower), value) {
            headers.insert(name, value);
        }
    }
    // The ledger rides along with the stream, so the record is written when the last of the body
    // has gone — or when the caller walks away mid-clone, which drops the stream and writes it
    // just the same. That is the only way to be sure every call leaves one.
    let counted = answered.bytes_stream().map(move |chunk| match chunk {
        Ok(data) => {
            ledger.received(data.len());
            Ok(Frame::data(data))
        }
        Err(err) => {
            ledger.cut_short(&format!("the answer stopped early: {err}"));
            Err(std::io::Error::other(err))
        }
    });
    (headers, BodyExt::boxed(StreamBody::new(counted)))
}

/// Absolute vendor URLs in a `Link` header pointed back at where the call came from, or
/// pagination breaks in every client that follows them (ADR-0015 §6).
fn rewritten(value: &HeaderValue, here: &str, base: &str) -> Option<HeaderValue> {
    let text = value.to_str().ok()?;
    HeaderValue::from_str(&text.replace(base, here)).ok()
}

/// The vendor's own ID for the call, whatever it calls it. Kept beside DOC's so the two logs can
/// be read against each other.
pub(super) fn vendor_call(headers: &HeaderMap) -> Option<String> {
    headers
        .iter()
        .find(|(name, _)| {
            let name = name.as_str();
            name.ends_with("request-id") || name.ends_with("requestid") || name == "x-trace-id"
        })
        .and_then(|(_, value)| value.to_str().ok())
        .map(|value| value.chars().take(120).collect())
}

/// A short answer of DOC's own, for everything that never reached the vendor.
fn said(status: u16, detail: &str) -> Response<Body> {
    let body = serde_json::json!({ "detail": detail }).to_string();
    let mut response = Response::new(
        Full::new(Bytes::from(body)).map_err(|never: Infallible| match never {}).boxed(),
    );
    *response.status_mut() =
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    response.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
}

trait Tagged {
    fn with(self, name: &str, value: &str) -> Self;
}

impl Tagged for Response<Body> {
    fn with(mut self, name: &str, value: &str) -> Self {
        if let (Ok(name), Ok(value)) =
            (http::HeaderName::try_from(name), HeaderValue::from_str(value))
        {
            self.headers_mut().insert(name, value);
        }
        self
    }
}
