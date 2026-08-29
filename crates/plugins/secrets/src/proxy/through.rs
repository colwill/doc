//! A plugin reaching a proxied account as itself through core, `discovery/via/<account>/<path…>`:
//! no key, any allowance naming it (ADR-0014 §6, §8). Answers are held whole, so small JSON calls
//! only. The working set, rules, denials and record are the listener's own.

use doc_plugin_sdk::{Request, Response};
use futures::StreamExt;
use http::header::CONTENT_TYPE;

use super::listen::{CORRELATION, vendor_call};
use super::record::{Ledger, started};
use super::{Going, Working};
use crate::Refusal;
use crate::store::FAILED;

/// The most of an answer that goes back through core, which carries 16 MB in all.
const LARGEST: usize = 15 * 1024 * 1024;

fn said(status: u16, detail: &str) -> Response {
    Refusal { status, detail: detail.to_string() }.response()
}

pub async fn call(
    working: &Working,
    client: &reqwest::Client,
    request: &Request,
    plugin: &str,
    named: &str,
    route: &[&str],
) -> Response {
    let subject = format!("plugin:{plugin}");
    let method = request.method.to_ascii_uppercase();
    let path = format!("/{}", route.join("/"));

    // The listener's two refusals: nothing unrecorded (§12), nothing stale (§11).
    if !working.recorder.room() {
        return said(503, "DOC cannot record calls at the moment, so it is not making any.");
    }
    if !working.fresh().await {
        let why = match working.ready().await {
            false => "the proxy has not read its rules yet",
            true => "the proxy has not heard from the platform for too long",
        };
        let mut call = started(&working.replica);
        call.account_name = named.to_string();
        call.method = method;
        call.path = path;
        call.who = subject;
        call.outcome = FAILED.to_string();
        call.detail = why.to_string();
        working.recorder.write(call);
        return said(503, &format!("DOC cannot allow this yet: {why}."));
    }

    let verdict = working.decide_as(named, &subject, &method, &path, &request.query).await;
    let correlation = verdict.call.correlation.clone();
    let ledger = Ledger::new(verdict.call, working.recorder.clone());
    match verdict.going {
        Some(going) => forward(client, request, going, &ledger, &correlation).await,
        None => said(verdict.status, &verdict.detail),
    }
}

/// Makes the call with the account's credential; the caller's ledger records every outcome.
async fn forward(
    client: &reqwest::Client,
    request: &Request,
    going: Going,
    ledger: &Ledger,
    correlation: &str,
) -> Response {
    let Going { upstream, credential, path, query, .. } = going;
    let Ok(method) = reqwest::Method::from_bytes(request.method.to_ascii_uppercase().as_bytes())
    else {
        ledger.failed("not an HTTP method");
        return said(400, "that is not an HTTP method");
    };
    let mut outbound = client
        .request(method, upstream.address(&path, &query, &credential))
        .header("accept", "application/json")
        .header(CORRELATION, correlation);
    if let Some((name, value)) = upstream.header(&credential) {
        outbound = outbound.header(name, value);
    }
    if !request.body.is_empty() {
        let kind = request.headers.get("content-type").map_or("application/json", String::as_str);
        ledger.sent(request.body.len());
        outbound = outbound.header("content-type", kind).body(request.body.clone());
    }

    let answered = match outbound.send().await {
        Ok(answered) => answered,
        Err(err) => {
            ledger.failed(&format!("the vendor could not be reached: {err}"));
            return said(502, "DOC could not reach the vendor.");
        }
    };
    let status = answered.status().as_u16();
    ledger.answered(status, vendor_call(answered.headers()));
    let kind = answered
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    let mut body: Vec<u8> = Vec::new();
    let mut stream = answered.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let data = match chunk {
            Ok(data) => data,
            Err(err) => {
                ledger.cut_short(&format!("the answer stopped early: {err}"));
                return said(502, "The vendor's answer stopped early.");
            }
        };
        ledger.received(data.len());
        if body.len() + data.len() > LARGEST {
            ledger.cut_short("the answer was larger than a call through core carries");
            return said(
                502,
                "The vendor's answer is larger than a call through core carries; ask for less, \
                 or reach the account through the proxy's own listener.",
            );
        }
        body.extend_from_slice(&data);
    }
    Response::new(status, &kind, body)
}
