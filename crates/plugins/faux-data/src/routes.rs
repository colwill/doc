//! The faux data contract (DOC-SPEC §9.2): `discovery/faux/<what>` routes, asked by a plugin as
//! itself, each answering in the shape of the live contract it stands in for. Only a plugin whose
//! toggle is on is answered, so turning one off stops its faux data even before it notices.

use chrono::{DateTime, Duration, Utc};
use doc_plugin_sdk::{Backend, Request, Response};
use serde_json::{Map, Value, json};

use crate::settings::Config;
use crate::{delivery, docs, estate, lifecycles, pipelines, releases, reliability};

/// The longest period asked for at once.
const MAX_DAYS: i64 = 800;
/// Runs in one page of pipeline data.
const PAGE: usize = 2_000;
/// The most services, subjects or products asked about at once.
const MAX_ASKED: usize = 500;

struct Refused(u16, String);

impl Refused {
    fn bad(detail: impl Into<String>) -> Self {
        Self(400, detail.into())
    }
}

fn all(query: &str, name: &str) -> Result<Vec<String>, Refused> {
    let mut found: Vec<String> = url::form_urlencoded::parse(query.as_bytes())
        .filter(|(key, _)| key == name)
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect();
    found.sort();
    found.dedup();
    match found.len() > MAX_ASKED {
        true => Err(Refused::bad(format!("ask about at most {MAX_ASKED} at once"))),
        false => Ok(found),
    }
}

fn one(query: &str, name: &str) -> Result<String, Refused> {
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Refused::bad(format!("name the `{name}`")))
}

fn moment(query: &str, name: &str) -> Result<Option<DateTime<Utc>>, Refused> {
    match one(query, name) {
        Err(_) => Ok(None),
        Ok(text) => DateTime::parse_from_rfc3339(&text)
            .map(|at| Some(at.with_timezone(&Utc)))
            .map_err(|_| Refused::bad(format!("`{name}` is an RFC 3339 time"))),
    }
}

/// `from` and `to`, the last 30 days unless they say, and at most `MAX_DAYS` apart.
fn period(query: &str) -> Result<(DateTime<Utc>, DateTime<Utc>), Refused> {
    let to = moment(query, "to")?.unwrap_or_else(Utc::now);
    let from = moment(query, "from")?.unwrap_or(to - Duration::days(30));
    if from >= to || to - from > Duration::days(MAX_DAYS) {
        return Err(Refused::bad(format!(
            "`from` comes before `to`, and at most {MAX_DAYS} days before it"
        )));
    }
    Ok((from, to))
}

fn answered(config: &Config, route: &str, query: &str) -> Result<Value, Refused> {
    match route {
        "estate" => Ok(estate::answer(config, &all(query, "service")?)),
        // Each service as `name=Title`, so a page's own titles are kept; a bare name is titled.
        "docs" => {
            let asked: Vec<(String, String)> = all(query, "service")?
                .into_iter()
                .map(|service| match service.split_once('=') {
                    Some((name, title)) => (name.trim().to_string(), title.trim().to_string()),
                    None => {
                        let title = service.replace('-', " ");
                        let mut letters = title.chars();
                        let title = letters
                            .next()
                            .map(|first| first.to_uppercase().chain(letters).collect())
                            .unwrap_or_default();
                        (service, title)
                    }
                })
                .collect();
            Ok(docs::answer(config, &asked))
        }
        "pipelines" => {
            let repository = one(query, "repository")?;
            let (from, to) = period(query)?;
            let after = match one(query, "after") {
                Ok(after) => {
                    after.parse::<usize>().map_err(|_| Refused::bad("`after` is a number"))?
                }
                Err(_) => 0,
            };
            let mut runs = pipelines::runs(config, &repository, from, to);
            // Like a query's filter: only the default branch's runs, for a plugin counting no more.
            if one(query, "default_branch").is_ok_and(|only| only == "true") {
                runs.retain(|run| run.default_branch);
            }
            let page: Vec<&pipelines::Run> = runs.iter().skip(after).take(PAGE).collect();
            let next = (after + PAGE < runs.len()).then_some(after + PAGE);
            Ok(json!({ "runs": page, "next": next }))
        }
        "delivery" => {
            let repository = one(query, "repository")?;
            let (from, to) = period(query)?;
            Ok(json!(delivery::delivered(config, &repository, from, to)))
        }
        "reliability" => {
            let (from, to) = period(query)?;
            Ok(reliability::held(config, &all(query, "subject")?, from, to))
        }
        "lifecycles" => {
            let mut asked = all(query, "product")?;
            if asked.is_empty() {
                asked = lifecycles::names();
            }
            let products: Map<String, Value> = asked
                .into_iter()
                .filter_map(|product| Some((product.clone(), lifecycles::product(&product)?)))
                .collect();
            Ok(json!({ "products": products }))
        }
        "releases" => Ok(releases::held(config, &all(query, "service")?)),
        other => Err(Refused(404, format!("faux-data makes up no {other}"))),
    }
}

pub async fn handle(backend: &Backend, request: &Request, config: &Config) -> Response {
    let asking = backend.caller().and_then(|caller| caller.id.clone()).unwrap_or_default();
    if !config.serving.contains(&asking) {
        let detail = format!(
            "faux-data is not providing faux data to {asking}: its toggle is off on faux-data's \
             Settings page"
        );
        return Response::problem(403, "forbidden", &detail);
    }
    let route = request.path.trim_end_matches('/').trim_start_matches("discovery/faux/");
    match answered(config, route, &request.query) {
        Ok(value) => Response::json(&value),
        Err(Refused(status, detail)) => {
            let kind = if status == 404 { "not-found" } else { "bad-request" };
            Response::problem(status, kind, &detail)
        }
    }
}
