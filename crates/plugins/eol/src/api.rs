//! The JSON routes: what a scope's services run and where each release stands, a product's whole
//! lifecycle, readiness for the delivery roadmap, and DOC's copy of endoflife.date in its own
//! format.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, NaiveDate, Utc};
use doc_plugin_sdk::{Backend, Request, Response};
use serde_json::{Value, json};

use crate::lifecycle::{self, Status, day};
use crate::scope::{self, Member, Scope};
use crate::store::Product;
use crate::view::{self, Judged};
use crate::{Refusal, faux, parameter};

/// The most services one readiness question may ask about.
const MAX_ASKED: usize = 100;
/// Concerns named in one readiness summary; the rest are counted.
const NAMED: usize = 2;

type Answer = Result<Value, Refusal>;

pub async fn handle(backend: &Backend, request: &Request) -> Response {
    let path = request.path.trim_end_matches('/').to_string();
    let query = request.query.as_str();
    let answer = match (request.method.as_str(), path.as_str()) {
        (_, "api/mcp") => return crate::mcp::handle(backend, request).await,
        ("GET", "api/usage") => usage(backend, query).await,
        ("GET", "api/repositories") => read_repositories(backend).await,
        ("GET", "api/readiness") => readiness(backend, query).await,
        // endoflife.date's own API, answered from DOC's copy, for teams' tools.
        ("GET", route) if route.starts_with("api/v1/") => {
            return crate::mirror::serve(backend, request, &route["api/v1/".len()..]).await;
        }
        ("GET", route) if route.starts_with("api/products/") => {
            let name = &route["api/products/".len()..];
            product(backend, &decoded(name)).await
        }
        _ => return Response::not_found(),
    };
    match answer {
        Ok(value) => Response::json(&value),
        Err(refusal) => refusal.response(),
    }
}

/// A route's segment as it was before it was percent-encoded.
pub fn decoded(text: &str) -> String {
    url::form_urlencoded::parse(format!("x={text}").as_bytes())
        .next()
        .map(|(_, value)| value.into_owned())
        .unwrap_or_default()
}

async fn usage(backend: &Backend, query: &str) -> Answer {
    let scope = Scope::from_query(query)?;
    let view = view::read(backend, &scope).await?;
    let services: Vec<Value> = view
        .members
        .iter()
        .map(|member| {
            let runs: Vec<Value> =
                view.of(&member.name).iter().map(|judged| judged.json()).collect();
            json!({
                "name": member.name,
                "title": member.title,
                "runs": runs,
                "worst": view.of(&member.name).iter().map(|judged| judged.status).min(),
                "problems": member.problems,
                "repositories": member.repositories,
            })
        })
        .collect();
    Ok(json!({
        "scope": { "kind": scope.kind(), "name": scope.name() },
        "today": view.today,
        "services": services,
        "faux": faux::said(),
    }))
}

/// What each repository connected to a service was last found to be built on, and when: the
/// products among the packages Repository Insights listed, and what its own files say where those
/// are read. It is what the schedule found, read from the Catalogue as this plugin, so it names
/// which services each repository belongs to only to somebody who can read the Catalogue too.
async fn read_repositories(backend: &Backend) -> Answer {
    match backend.ask("resources", "GET", "version", None, None).await {
        Ok((200, _)) => {}
        Ok(_) => {
            return Err(Refusal::forbidden(
                "which services each repository belongs to is the Catalogue's, and you cannot \
                 read the Catalogue",
            ));
        }
        Err(err) => {
            return Err(Refusal::unavailable(format!("the Catalogue could not be asked: {err}")));
        }
    }
    let files = backend.feature(crate::settings::REPOSITORIES);
    let kept = crate::repositories::all(backend)
        .await
        .map_err(|err| Refusal::unavailable(err.to_string()))?;
    let repositories: Vec<Value> = kept
        .values()
        .map(|scanned| {
            json!({
                "repository": scanned.repository,
                "services": scanned.services,
                "packages": scanned.packages,
                "commit": scanned.commit,
                "listed_at": scanned.listed_at,
                "uses": if files { json!(scanned.found) } else { json!([]) },
                "files": scanned.files.filter(|_| files),
                "pushed_at": scanned.pushed_at,
                "read_at": scanned.read_at.filter(|_| files),
                "problem": scanned.problem.clone().filter(|_| files),
            })
        })
        .collect();
    Ok(json!({ "repositories": repositories }))
}

/// A product's release cycles, and the services that run it that the caller can see; the
/// services are left out, rather than the product refused, when the Catalogue cannot be read.
pub async fn lifecycle_of(
    backend: &Backend,
    name: &str,
) -> Result<(Product, Vec<Judged>), Refusal> {
    let key = match lifecycle::is_custom(name) {
        true => name.to_string(),
        false => name.trim().to_ascii_lowercase(),
    };
    if key.is_empty() {
        return Err(Refusal::bad("name a product, such as nodejs"));
    }
    let found = lifecycle::products(backend, &BTreeSet::from([key.clone()])).await?;
    let product = found
        .get(&key)
        .cloned()
        .ok_or_else(|| Refusal::missing(format!("there is no product called {key}")))?;
    if product.releases.is_empty() {
        return Err(Refusal::missing(
            product.problem.unwrap_or_else(|| format!("{key} has no releases")),
        ));
    }
    let used_by = match view::read(backend, &Scope::All).await {
        Ok(view) => view.judged.into_iter().filter(|judged| judged.product == key).collect(),
        Err(_) => Vec::new(),
    };
    Ok((product, used_by))
}

async fn product(backend: &Backend, name: &str) -> Answer {
    let (product, used_by) = lifecycle_of(backend, name).await?;
    let today = lifecycle::today();
    let warn_days = crate::settings::Definitions::read(&backend.settings()).warn_days;
    let releases: Vec<Value> = product
        .releases
        .iter()
        .map(|release| {
            let mut value = json!(release);
            value["status"] = json!(release.status(today, warn_days));
            value
        })
        .collect();
    Ok(json!({
        "product": product.product,
        "label": product.label,
        "link": product.link,
        "category": product.category,
        "read_at": product.read_at,
        "problem": product.problem,
        "releases": releases,
        "used_by": used_by.iter().map(Judged::json).collect::<Vec<_>>(),
        "faux": faux::said(),
    }))
}

/// `until` as a day, `2026-10-15`, or an RFC 3339 time.
fn until(query: &str) -> Result<Option<NaiveDate>, Refusal> {
    let Some(text) = parameter(query, "until") else { return Ok(None) };
    if let Ok(at) = DateTime::parse_from_rfc3339(&text) {
        return Ok(Some(at.with_timezone(&Utc).date_naive()));
    }
    NaiveDate::parse_from_str(&text, "%Y-%m-%d")
        .map(Some)
        .map_err(|_| Refusal::bad("`until` is a day, such as 2026-10-15, or an RFC 3339 time"))
}

/// The services a readiness question names, each once.
pub fn asked(query: &str) -> Result<Vec<String>, Refusal> {
    let mut names: Vec<String> = url::form_urlencoded::parse(query.as_bytes())
        .filter(|(key, _)| key == "service")
        .flat_map(|(_, value)| {
            value.split(',').map(|name| name.trim().to_string()).collect::<Vec<_>>()
        })
        .filter(|name| !name.is_empty())
        .collect();
    names.sort();
    names.dedup();
    match names.len() {
        0 => Err(Refusal::bad("name at least one service with `service`")),
        count if count > MAX_ASKED => {
            Err(Refusal::bad(format!("ask about at most {MAX_ASKED} services at once")))
        }
        _ => Ok(names),
    }
}

/// One service's readiness: whether what it runs is still supported, today and on `until`.
fn ready(member: &Member, judged: &[Judged], today: NaiveDate, until: Option<NaiveDate>) -> Value {
    let href = format!("/p/eol/?{}", scope::encoded(&[("service", &member.name)]));
    if judged.is_empty() {
        return json!({
            "state": "unknown",
            "summary": "What it runs is not known: no repository connected to it has been \
                        scanned by Repository Insights, and its metadata names nothing",
            "href": href,
        });
    }
    let mut concerns = Vec::new();
    for each in judged {
        let Some(release) = &each.release else { continue };
        if release.ended_by(today) {
            concerns.push(match release.eol {
                Some(eol) => {
                    format!("{} has been past its end of life since {}", each.named(), day(eol))
                }
                None => format!("{} is past its end of life", each.named()),
            });
        } else if let (Some(until), Some(eol)) = (until, release.eol)
            && eol <= until
        {
            concerns.push(format!(
                "{} reaches its end of life on {}, before {}",
                each.named(),
                day(eol),
                day(until)
            ));
        }
    }
    let judged_count = judged.iter().filter(|each| each.status != Status::Unknown).count();
    if concerns.is_empty() {
        let summary = match (judged_count, until) {
            (0, _) => "None of what it runs names a version to judge".to_string(),
            (count, Some(until)) => format!(
                "{} supported past {}",
                match count {
                    1 => "The one product it names a version of is".to_string(),
                    count => format!("All {count} products it names versions of are"),
                },
                day(until)
            ),
            (count, None) => format!("All {count} products it names versions of are supported"),
        };
        let state = if judged_count == 0 { "unknown" } else { "ready" };
        return json!({ "state": state, "summary": summary, "href": href });
    }
    let more = concerns.len().saturating_sub(NAMED);
    let mut summary = concerns.into_iter().take(NAMED).collect::<Vec<_>>().join("; ");
    if more > 0 {
        summary.push_str(&format!("; and {more} more"));
    }
    json!({ "state": "warning", "summary": summary, "href": href })
}

async fn readiness(backend: &Backend, query: &str) -> Answer {
    let names = asked(query)?;
    let until = until(query)?;
    let mut every = scope::every(backend).await?;
    // Faux data is made up for any service asked about.
    if faux::on(backend) {
        let unknown: BTreeMap<String, String> = names
            .iter()
            .filter(|name| !every.iter().any(|member| &member.name == *name))
            .map(|name| (name.clone(), name.clone()))
            .collect();
        if !unknown.is_empty() {
            every.extend(faux::named(backend, &unknown).await?);
        }
    }
    let members: Vec<Option<Member>> = names
        .iter()
        .map(|name| every.iter().find(|member| member.name == *name).cloned())
        .collect();
    let wanted: BTreeSet<String> = members
        .iter()
        .flatten()
        .flat_map(|member| member.used.iter().map(|used| used.product.clone()))
        .collect();
    let products = lifecycle::products(backend, &wanted).await?;
    let today = lifecycle::today();
    let warn_days = crate::settings::Definitions::read(&backend.settings()).warn_days;
    let mut services = serde_json::Map::new();
    for (name, member) in names.iter().zip(members) {
        let answer = match member {
            None => json!({
                "state": "unknown",
                "summary": "The Catalogue has no such service, or you cannot see it",
            }),
            Some(member) => {
                let judged = view::judged_one(&member, &products, today, warn_days);
                ready(&member, &judged, today, until)
            }
        };
        services.insert(name.clone(), answer);
    }
    Ok(json!({
        "title": "End of life",
        "services": services,
        "faux": faux::said(),
    }))
}
