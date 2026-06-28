//! The `core.*` collections every plugin can read and none can write. They are made from core's own
//! records when asked for, then filtered, sorted and paged like any other collection.

use std::sync::LazyLock;

use chrono::{DateTime, Utc};
use doc_background_tasks::store::TaskFilter;
use doc_plugin_protocol::data::{Collection, Declaration, Field, ListOf};
use serde_json::{Value, json};

use super::query::Record;
use super::store::DataError;
use super::values;
use crate::api::AppState;

/// The most recent tasks of the reading plugin that `core.tasks` holds.
pub const TASKS: i64 = 10_000;
/// How much history one read of `core.status-history` covers: a check a minute for each
/// component, so a day is a few thousand records. Plugin state changes are far rarer.
const STATUS_SPAN_HOURS: i64 = 24;
const PLUGIN_SPAN_DAYS: i64 = 31;

pub static DECLARATION: LazyLock<Declaration> = LazyLock::new(|| {
    Declaration::default()
        .collection(
            "users",
            Collection::new()
                .field("id", Field::uuid().key())
                .field(
                    "provider",
                    Field::text().describe(
                        "the provider of the account the user was first known by, if they have one",
                    ),
                )
                .field("login", Field::text())
                .field(
                    "organisation_id",
                    Field::uuid().describe("The one organisation they belong to"),
                )
                .field("name", Field::text())
                .field("email", Field::text())
                .field("first_name", Field::text())
                .field("surname", Field::text())
                .field("disabled", Field::boolean())
                .field("first_signed_in_at", Field::timestamp())
                .field("last_signed_in_at", Field::timestamp())
                .field("created_at", Field::timestamp())
                .index(&["login"])
                .index(&["provider", "login"])
                .index(&["created_at"]),
        )
        .collection(
            "identities",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("user_id", Field::uuid())
                .field("provider", Field::text())
                .field("external_id", Field::text())
                .field("login", Field::text())
                .field("source", Field::text())
                .field("name", Field::text().describe("What the provider last reported"))
                .field("email", Field::text())
                .field("first_name", Field::text())
                .field("surname", Field::text())
                .field("reported_at", Field::timestamp())
                .field("created_at", Field::timestamp())
                .field("last_used_at", Field::timestamp())
                .index(&["user_id"])
                .index(&["provider", "external_id"])
                .index(&["provider", "login"]),
        )
        .collection(
            "service-accounts",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("name", Field::text())
                .field("description", Field::text())
                .field("owner_id", Field::uuid())
                .field("owner_team_id", Field::uuid())
                .field("disabled", Field::boolean())
                .field("created_at", Field::timestamp())
                .index(&["name"])
                .index(&["owner_id"])
                .index(&["owner_team_id"]),
        )
        .collection(
            "organisations",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("name", Field::text())
                .field("title", Field::text())
                .field("description", Field::text())
                .field("created_at", Field::timestamp())
                .index(&["name"]),
        )
        .collection(
            "teams",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("organisation_id", Field::uuid())
                .field("parent_id", Field::uuid())
                .field("name", Field::text())
                .field("title", Field::text())
                .field("description", Field::text())
                .field("email", Field::text())
                .field("default", Field::boolean())
                .field("provider", Field::text())
                .field("external_id", Field::text())
                .field("lead_id", Field::uuid().describe("One of its members, who leads it"))
                .field("created_at", Field::timestamp())
                .index(&["organisation_id", "name"])
                .index(&["parent_id"])
                .index(&["provider", "external_id"]),
        )
        .collection(
            "team-members",
            Collection::new()
                .field("id", Field::text().key().describe("`<team_id>:<user_id>`"))
                .field("team_id", Field::uuid())
                .field("user_id", Field::uuid())
                .field("source", Field::text())
                .field("provider", Field::text())
                .field(
                    "position",
                    Field::text().describe("The name of the position they hold here"),
                )
                .field("created_at", Field::timestamp())
                .index(&["team_id"])
                .index(&["user_id"]),
        )
        .collection(
            "positions",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("organisation_id", Field::uuid())
                .field("name", Field::text())
                .field("title", Field::text())
                .field("description", Field::text())
                .field("responsibilities", Field::list(ListOf::Text))
                .field("created_at", Field::timestamp())
                .index(&["organisation_id", "name"]),
        )
        .collection(
            "team-positions",
            Collection::new()
                .field("id", Field::text().key().describe("`<team_id>:<name>`"))
                .field("team_id", Field::uuid())
                .field("name", Field::text())
                .field("title", Field::text())
                .field("description", Field::text())
                .field("responsibilities", Field::list(ListOf::Text))
                .field(
                    "hidden",
                    Field::boolean().describe("Hidden in this team, so held by no one"),
                )
                .field(
                    "origin",
                    Field::text().describe("`organisation`, or the team that made it its own"),
                )
                .index(&["team_id"]),
        )
        .collection(
            "plugins",
            Collection::new()
                .field("id", Field::text().key())
                .field("display_name", Field::text())
                .field("version", Field::text())
                .field("classification", Field::text())
                .field("state", Field::text())
                .field("registered_at", Field::timestamp())
                .field("last_seen_at", Field::timestamp())
                .field(
                    "mcp",
                    Field::boolean().describe("Whether it serves MCP at api/mcp (FEAT-AGENT)"),
                )
                .field(
                    "telemetry",
                    Field::boolean().describe(
                        "Whether it takes DOC's telemetry somewhere that keeps it, which is what                          lets the week DOC keeps be raised",
                    ),
                )
                .index(&["state"]),
        )
        .collection(
            "plugin-status",
            Collection::new()
                .field("plugin", Field::text().key())
                .field("version", Field::text())
                .field("state", Field::text())
                .field("error", Field::text())
                .field("since", Field::timestamp())
                .field("last_error", Field::text())
                .field("last_error_at", Field::timestamp())
                .index(&["state"]),
        )
        .collection(
            // Every check the status probes made, kept 30 days (T34): read a day at a time, from a
            // lower bound on `at`.
            "status-history",
            Collection::new()
                .field("id", Field::text().key().describe("`<name>@<at>`"))
                .field("at", Field::timestamp())
                .field("kind", Field::text())
                .field("name", Field::text())
                .field("state", Field::text().describe("up, degraded, down or unknown"))
                .field("detail", Field::text())
                .field("latency_ms", Field::integer())
                .field(
                    "nodes",
                    Field::json().describe("A bus's nodes, each with its role and replication lag"),
                )
                .index(&["at"])
                .index(&["name", "at"]),
        )
        .collection(
            // Every state each plugin's registrations entered, kept 30 days (T24): read up to a
            // month at a time, from a lower bound on `at`.
            "plugin-status-history",
            Collection::new()
                .field("id", Field::text().key().describe("`<plugin>@<at>@<state>`"))
                .field("plugin", Field::text())
                .field("version", Field::text())
                .field("state", Field::text().describe("A lifecycle state, or removed"))
                .field("error", Field::text())
                .field("at", Field::timestamp())
                .index(&["at"])
                .index(&["plugin", "at"]),
        )
        .collection(
            "plugin-permissions",
            Collection::new()
                .field("id", Field::text().key())
                .field("plugin", Field::text())
                .field("kind", Field::text())
                .field("name", Field::text())
                .index(&["plugin", "kind"]),
        )
        .collection(
            // Everything the platform and its plugins announce goes through the Event Bus, so
            // what it has seen is the list of what there is to subscribe to (T71).
            "topics",
            Collection::new()
                .field("topic", Field::text().key())
                .field("retained", Field::integer().describe("Events the bus still holds"))
                .field("published", Field::integer().describe("Events ever published on it")),
        )
        .collection(
            "tasks",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("state", Field::text())
                .field("payload", Field::json())
                .field("result", Field::json())
                .field("error", Field::text())
                .field("attempts", Field::integer())
                .field("created_at", Field::timestamp())
                .field("finished_at", Field::timestamp())
                .index(&["created_at"])
                .index(&["state", "created_at"]),
        )
});

/// Every team's positions as it has them, inherited and changed on the way down (FEAT-TEAMS).
async fn team_positions(state: &AppState) -> Result<Vec<Value>, DataError> {
    let repos = &state.repos;
    let teams = repos.teams.teams().await.map_err(|err| DataError::Unavailable(err.to_string()))?;
    let positions =
        repos.teams.positions(None).await.map_err(|err| DataError::Unavailable(err.to_string()))?;
    let changes = repos
        .teams
        .team_positions(None)
        .await
        .map_err(|err| DataError::Unavailable(err.to_string()))?;
    let mut records = Vec::new();
    for team in &teams {
        let chain: Vec<_> = crate::teams::chain(&teams, team.id)
            .into_iter()
            .map(|above| {
                let theirs = changes.iter().filter(|change| change.team_id == above.id);
                (above, theirs.cloned().collect::<Vec<_>>())
            })
            .collect();
        let organisation: Vec<_> = positions
            .iter()
            .filter(|position| position.organisation_id == team.organisation_id)
            .cloned()
            .collect();
        for position in crate::teams::effective(&organisation, &chain) {
            records.push(json!({
                "id": format!("{}:{}", team.id, position.name), "team_id": team.id,
                "name": position.name, "title": position.title,
                "description": position.description,
                "responsibilities": position.responsibilities, "hidden": position.hidden,
                "origin": position.origin,
            }));
        }
    }
    Ok(records)
}

fn at(value: Option<DateTime<Utc>>) -> Value {
    value.map_or(Value::Null, |at| Value::String(values::timestamp(at)))
}

fn object(value: Value) -> Record {
    match value {
        Value::Object(record) => record,
        _ => Record::new(),
    }
}

/// The time a history is read over, from the `at` bounds a read gives: a lower bound is required,
/// and the upper one is at most `span` after it.
fn window(
    filter: Option<&Value>,
    name: &str,
    span: chrono::Duration,
) -> Result<(DateTime<Utc>, DateTime<Utc>), DataError> {
    let bound = |op: &str| {
        filter
            .and_then(|filter| filter.get("at"))
            .and_then(|at| at.get(op))
            .and_then(Value::as_str)
            .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
            .map(|at| at.with_timezone(&Utc))
    };
    let Some(from) = bound("gte").or_else(|| bound("gt")) else {
        return Err(DataError::BadQuery(format!(
            "core.{name} is read from a lower bound on `at`, such as {{\"at\": {{\"gte\": \"2026-09-01T00:00:00Z\"}}}}"
        )));
    };
    let to = bound("lt").or_else(|| bound("lte")).unwrap_or(from + span);
    if to - from > span {
        let days = span.num_days();
        let unit = if days == 1 { "day" } else { "days" };
        return Err(DataError::BadQuery(format!(
            "core.{name} is read at most {days} {unit} at a time"
        )));
    }
    // One past an inclusive upper bound, so a check at exactly `lte` is read.
    Ok((from, to + chrono::Duration::microseconds(1)))
}

/// Every record of `core.<name>` that `reader` may see; a history only in the window `filter` asks for.
pub async fn records(
    state: &AppState,
    reader: &str,
    name: &str,
    filter: Option<&Value>,
) -> Result<Vec<Record>, DataError> {
    let unavailable = |err: &dyn std::fmt::Display| DataError::Unavailable(err.to_string());
    let repos = &state.repos;
    let records = match name {
        "status-history" => {
            let (from, to) = window(filter, name, chrono::Duration::hours(STATUS_SPAN_HOURS))?;
            repos
                .status_history
                .between(from, to)
                .await
                .map_err(|err| unavailable(&err))?
                .into_iter()
                .map(|check| {
                    json!({
                        "id": format!("{}@{}", check.name, values::timestamp(check.checked_at)),
                        "at": at(Some(check.checked_at)), "kind": check.kind, "name": check.name,
                        "state": check.state.as_str(), "detail": check.detail,
                        "latency_ms": check.latency_ms, "nodes": check.nodes,
                    })
                })
                .collect()
        }
        "plugin-status-history" => {
            let (from, to) = window(filter, name, chrono::Duration::days(PLUGIN_SPAN_DAYS))?;
            repos
                .plugin_status
                .changes(from, to)
                .await
                .map_err(|err| unavailable(&err))?
                .into_iter()
                .map(|(plugin, change)| {
                    let when = values::timestamp(change.at);
                    json!({
                        "id": format!("{plugin}@{when}@{}", change.state), "plugin": plugin,
                        "version": change.version, "state": change.state, "error": change.error,
                        "at": when,
                    })
                })
                .collect()
        }
        "users" => {
            // Oldest first, so the first seen for each user is the account they came with.
            let identities = repos.identity.identities(None).await.map_err(|err| DataError::Unavailable(err.to_string()))?;
            let mut home = std::collections::HashMap::new();
            for identity in identities {
                home.entry(identity.user_id).or_insert(identity.provider);
            }
            repos
                .identity
                .list_users()
                .await
                .map_err(|err| unavailable(&err))?
                .into_iter()
                .map(|listed| {
                    let user = listed.user;
                    json!({
                        "id": user.id, "provider": home.get(&user.id), "login": user.login,
                        "organisation_id": user.organisation_id,
                        "name": user.name, "email": user.email, "first_name": user.first_name,
                        "surname": user.surname, "disabled": user.disabled,
                        "first_signed_in_at": at(user.first_signed_in_at),
                        "last_signed_in_at": at(user.last_signed_in_at),
                        "created_at": at(Some(listed.created_at)),
                    })
                })
                .collect()
        }
        "identities" => repos
            .identity
            .identities(None)
            .await
            .map_err(|err| unavailable(&err))?
            .into_iter()
            .map(|identity| {
                json!({
                    "id": identity.id, "user_id": identity.user_id, "provider": identity.provider,
                    "external_id": identity.external_id, "login": identity.login,
                    "source": identity.source, "name": identity.name, "email": identity.email,
                    "first_name": identity.first_name, "surname": identity.surname,
                    "reported_at": at(identity.reported_at),
                    "created_at": at(Some(identity.created_at)),
                    "last_used_at": at(identity.last_used_at),
                })
            })
            .collect(),
        "service-accounts" => repos
            .identity
            .list_service_accounts(None)
            .await
            .map_err(|err| unavailable(&err))?
            .into_iter()
            .map(|account| {
                json!({
                    "id": account.id, "name": account.name, "description": account.description,
                    "owner_id": account.owner_id, "owner_team_id": account.owner_team_id,
                    "disabled": account.disabled, "created_at": at(Some(account.created_at)),
                })
            })
            .collect(),
        "organisations" => repos
            .teams
            .organisations()
            .await
            .map_err(|err| unavailable(&err))?
            .into_iter()
            .map(|organisation| {
                json!({
                    "id": organisation.id, "name": organisation.name, "title": organisation.title,
                    "description": organisation.description,
                    "created_at": at(Some(organisation.created_at)),
                })
            })
            .collect(),
        "teams" => repos
            .teams
            .teams()
            .await
            .map_err(|err| unavailable(&err))?
            .into_iter()
            .map(|team| {
                json!({
                    "id": team.id, "organisation_id": team.organisation_id,
                    "parent_id": team.parent_id, "name": team.name, "title": team.title,
                    "description": team.description, "email": team.email,
                    "default": team.is_default,
                    "provider": team.provider, "external_id": team.external_id,
                    "lead_id": team.lead_id, "created_at": at(Some(team.created_at)),
                })
            })
            .collect(),
        "team-members" => repos
            .teams
            .members(None)
            .await
            .map_err(|err| unavailable(&err))?
            .into_iter()
            .map(|held| {
                json!({
                    "id": format!("{}:{}", held.team_id, held.user_id),
                    "team_id": held.team_id, "user_id": held.user_id, "source": held.source,
                    "provider": held.provider, "position": held.position,
                    "created_at": at(Some(held.created_at)),
                })
            })
            .collect(),
        "positions" => repos
            .teams
            .positions(None)
            .await
            .map_err(|err| unavailable(&err))?
            .into_iter()
            .map(|position| {
                json!({
                    "id": position.id, "organisation_id": position.organisation_id,
                    "name": position.name, "title": position.title,
                    "description": position.description,
                    "responsibilities": position.responsibilities,
                    "created_at": at(Some(position.created_at)),
                })
            })
            .collect(),
        "team-positions" => team_positions(state).await?,
        "plugins" => repos
            .plugins
            .records()
            .await
            .map_err(|err| unavailable(&err))?
            .into_iter()
            .map(|plugin| {
                let named = plugin.manifest.pointer("/nav/0/label").cloned().unwrap_or_default();
                let text = |value: String| if value.is_empty() { Value::Null } else { Value::String(value) };
                // An MCP server is a read route called `mcp`, which is how every plugin serving
                // one declares it (DOC-SPEC §4.1).
                let mcp = plugin.manifest["read_routes"]
                    .as_array()
                    .is_some_and(|routes| routes.iter().any(|route| route == "mcp"));
                // Whoever keeps DOC's telemetry beyond the week DOC keeps itself, so what needs
                // to know whether it may keep more than a week can ask.
                let telemetry = plugin.manifest["capabilities"]
                    .as_array()
                    .is_some_and(|held| held.iter().any(|one| one == "telemetry-sink"));
                json!({
                    "id": plugin.id, "display_name": named, "version": text(plugin.version),
                    "classification": text(plugin.classification), "state": text(plugin.state),
                    "registered_at": at(plugin.registered_at), "last_seen_at": at(plugin.last_seen_at),
                    "mcp": mcp, "telemetry": telemetry,
                })
            })
            .collect(),
        "plugin-status" => repos
            .plugin_status
            .current()
            .await
            .map_err(|err| unavailable(&err))?
            .into_iter()
            .map(|status| {
                json!({
                    "plugin": status.plugin, "version": status.version,
                    "state": status.state.map(|state| state.as_str()), "error": status.error,
                    "since": at(Some(status.since)), "last_error": status.last_error,
                    "last_error_at": at(status.last_error_at),
                })
            })
            .collect(),
        "plugin-permissions" => {
            let mut permissions = Vec::new();
            for record in repos.plugins.records().await.map_err(|err| unavailable(&err))? {
                let id = record.id;
                let declared = repos.plugins.permissions(&id).await.map_err(|err| DataError::Unavailable(err.to_string()))?;
                permissions.extend(declared.into_iter().map(|permission| {
                    json!({
                        "id": format!("{id}:{}:{}", permission.kind, permission.name),
                        "plugin": id, "kind": permission.kind, "name": permission.name,
                    })
                }));
            }
            permissions
        }
        "topics" => state
            .buses
            .events
            .topics()
            .await
            .map_err(|err| unavailable(&err))?
            .into_iter()
            .map(|report| {
                json!({
                    "topic": report.topic.as_str(),
                    "retained": report.retained,
                    "published": report.published,
                })
            })
            .collect(),
        "tasks" => {
            let filter = TaskFilter { kind: Some(format!("plugin.{reader}.run")), limit: TASKS, ..TaskFilter::default() };
            repos
                .tasks
                .list(filter)
                .await
                .map_err(|err| unavailable(&err))?
                .into_iter()
                .map(|task| {
                    json!({
                        "id": task.id, "state": task.state.as_str(), "payload": task.payload,
                        "result": task.result, "error": task.error, "attempts": task.attempts,
                        "created_at": at(Some(task.created_at)), "finished_at": at(task.finished_at),
                    })
                })
                .collect()
        }
        other => return Err(DataError::NoCollection(format!("there is no collection core.{other}"))),
    };
    Ok(records.into_iter().map(object).collect())
}
