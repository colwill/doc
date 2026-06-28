//! The task API: the background queue, the cron schedules, and `/me/access`, which is the first
//! thing the immediate pool runs. Managing tasks needs `plugin:core:user:rw`; a single task is also
//! readable by whoever started it.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use doc_background_tasks::{Actor, NewTask, Task, TaskState};
use doc_cron_tasks::CronTask;
use doc_permissions::{Access, CORE, Scope};
use http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::AppState;
use super::problem::Problem;
use crate::auth::Auth;
use crate::identity::{AuditEntry, Principal};
use crate::permissions;

const MAX_LIMIT: i64 = 500;

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub state: Option<TaskState>,
    #[serde(default)]
    pub mine: bool,
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct StartTask {
    pub kind: String,
    #[serde(default)]
    pub payload: Value,
    #[serde(default)]
    pub max_attempts: Option<i32>,
}

#[derive(Debug, Deserialize)]
pub struct SetPaused {
    pub paused: bool,
}

#[derive(Debug, Serialize)]
pub struct PluginAccess {
    pub read: bool,
    pub write: bool,
    /// Whether the caller may see this plugin's settings, and whether they may change them
    /// (ADR-0007). Apart from `read` and `write`, which are about using the plugin.
    pub settings: bool,
    pub settings_write: bool,
    pub scope: Option<Scope>,
    pub custom: std::collections::BTreeMap<String, Scope>,
    /// Whether its pages can be opened now: an unloaded, loading or failed plugin offers none.
    pub running: bool,
    /// The plugin's navigation entries, offered only to a caller who can read it, while it runs.
    pub nav: Vec<doc_plugin_protocol::Nav>,
    /// What it adds to other plugins' resource pages, on the same terms.
    pub panels: Vec<doc_plugin_protocol::ResourcePanel>,
    /// The single facts about a resource it offers, for pinning, on the same terms.
    pub insights: Vec<doc_plugin_protocol::Insight>,
    /// What it offers for a person's dashboard, on the same terms.
    pub dashboard: Vec<doc_plugin_protocol::DashboardItem>,
    /// The providers it needs the caller to have linked an account with, on the same terms.
    pub links: Vec<String>,
    /// What automations can call by name, on the same terms (ADR-0012).
    pub operations: Vec<doc_plugin_protocol::Operation>,
}

pub(crate) fn actor(principal: &Principal) -> Actor {
    let (kind, id) = match principal {
        Principal::User(user) => ("user", user.id.to_string()),
        Principal::ServiceAccount(account) => ("service-account", account.id.to_string()),
        Principal::Plugin { id } => ("plugin", id.clone()),
    };
    Actor { kind: kind.into(), id: Some(id), label: Some(principal.label()) }
}

async fn manages_tasks(state: &AppState, principal: &Principal) -> Result<(), Problem> {
    match permissions::holds(state, principal, CORE, Access::Write).await {
        true => Ok(()),
        false => Err(Problem::forbidden("needs plugin:core:user:rw")),
    }
}

pub async fn list_tasks(
    State(state): State<AppState>,
    auth: Auth,
    Query(query): Query<ListQuery>,
) -> Result<Json<Vec<Task>>, Problem> {
    manages_tasks(&state, &auth.0).await?;
    let filter = doc_background_tasks::TaskFilter {
        kind: query.kind,
        state: query.state,
        owner: query.mine.then(|| actor(&auth.0)),
        limit: query.limit.unwrap_or(100).clamp(1, MAX_LIMIT),
    };
    let tasks = state.repos.tasks.list(filter).await.map_err(|err| task_problem(&err))?;
    Ok(Json(tasks))
}

/// Where the whole of a run has got to, when its first task queued more while it ran.
#[derive(Debug, Serialize)]
pub struct Chained {
    pub state: TaskState,
    pub tasks: usize,
    pub finished: usize,
    pub failed: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ShownTask {
    #[serde(flatten)]
    pub task: Task,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chained: Option<Chained>,
}

/// Running while any part is, then failed or cancelled if any part was, else succeeded.
fn chained(root: &Task, rest: &[Task]) -> Chained {
    let all: Vec<&Task> = std::iter::once(root).chain(rest).collect();
    let waiting = |task: &&Task| matches!(task.state, TaskState::Queued | TaskState::Running);
    let failed: Vec<&&Task> = all.iter().filter(|task| task.state == TaskState::Failed).collect();
    let state = match () {
        () if all.iter().any(waiting) => TaskState::Running,
        () if !failed.is_empty() => TaskState::Failed,
        () if all.iter().any(|task| task.state == TaskState::Cancelled) => TaskState::Cancelled,
        () => TaskState::Succeeded,
    };
    Chained {
        state,
        tasks: all.len(),
        finished: all.iter().filter(|task| !waiting(task)).count(),
        failed: failed.len(),
        error: failed.first().and_then(|task| task.error.clone()),
    }
}

/// Readable by whoever started it, and by anyone with `core` read access.
pub async fn show_task(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> Result<Json<ShownTask>, Problem> {
    let task = state
        .repos
        .tasks
        .get(id)
        .await
        .map_err(|err| task_problem(&err))?
        .ok_or_else(|| Problem::not_found("task"))?;
    let mine = task.started_by == actor(&auth.0);
    if !mine && !permissions::holds(&state, &auth.0, CORE, Access::Read).await {
        return Err(Problem::forbidden("needs plugin:core:user:ro, or having started this task"));
    }
    let rest = state.repos.tasks.chain(task.id).await.map_err(|err| task_problem(&err))?;
    let chained = (!rest.is_empty()).then(|| chained(&task, &rest));
    Ok(Json(ShownTask { task, chained }))
}

pub async fn start_task(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<StartTask>,
) -> Result<impl IntoResponse, Problem> {
    manages_tasks(&state, &auth.0).await?;
    if body.kind.trim().is_empty() {
        return Err(Problem::bad_request("a task needs a kind"));
    }
    let new = NewTask {
        kind: body.kind.trim().to_string(),
        payload: body.payload,
        max_attempts: body.max_attempts.unwrap_or(3).clamp(1, 20),
        started_by: actor(&auth.0),
        chain: None,
    };
    let task =
        doc_background_tasks::start(state.repos.tasks.as_ref(), state.buses.services.as_ref(), new)
            .await
            .map_err(|err| task_problem(&err))?;
    let _ = state
        .repos
        .identity
        .record_audit(
            AuditEntry::new("task.started")
                .by(&auth.0)
                .subject(task.id.to_string())
                .detail(json!({ "kind": task.kind })),
        )
        .await;
    announce(&state, "started", &task).await;
    Ok((StatusCode::ACCEPTED, Json(task)))
}

pub async fn cancel_task(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, Problem> {
    manages_tasks(&state, &auth.0).await?;
    if !state.repos.tasks.request_cancel(id).await.map_err(|err| task_problem(&err))? {
        return Err(Problem::conflict("this task is not running or queued"));
    }
    let _ = state
        .repos
        .identity
        .record_audit(AuditEntry::new("task.cancelled").by(&auth.0).subject(id.to_string()))
        .await;
    if let Ok(Some(task)) = state.repos.tasks.get(id).await {
        announce(&state, "cancelled", &task).await;
    }
    Ok(StatusCode::ACCEPTED)
}

pub async fn set_kind_paused(
    State(state): State<AppState>,
    auth: Auth,
    Path(kind): Path<String>,
    Json(body): Json<SetPaused>,
) -> Result<StatusCode, Problem> {
    manages_tasks(&state, &auth.0).await?;
    state
        .repos
        .tasks
        .set_kind_paused(&kind, body.paused)
        .await
        .map_err(|err| task_problem(&err))?;
    let action = if body.paused { "task-kind.paused" } else { "task-kind.resumed" };
    let _ =
        state.repos.identity.record_audit(AuditEntry::new(action).by(&auth.0).subject(&kind)).await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_cron(
    State(state): State<AppState>,
    auth: Auth,
) -> Result<Json<Vec<CronTask>>, Problem> {
    if !permissions::holds(&state, &auth.0, CORE, Access::Read).await {
        return Err(Problem::forbidden("needs plugin:core:user:ro"));
    }
    let tasks = state.repos.cron.list().await.map_err(|err| cron_problem(&err))?;
    Ok(Json(tasks))
}

pub async fn set_cron_paused(
    State(state): State<AppState>,
    auth: Auth,
    Path(name): Path<String>,
    Json(body): Json<SetPaused>,
) -> Result<StatusCode, Problem> {
    manages_tasks(&state, &auth.0).await?;
    if !state.repos.cron.set_paused(&name, body.paused).await.map_err(|err| cron_problem(&err))? {
        return Err(Problem::not_found("cron task"));
    }
    let action = if body.paused { "cron.paused" } else { "cron.resumed" };
    let _ =
        state.repos.identity.record_audit(AuditEntry::new(action).by(&auth.0).subject(&name)).await;
    Ok(StatusCode::NO_CONTENT)
}

/// The caller's access to every plugin in one call, so the frontend can build its navigation
/// without asking per plugin. The work runs in the immediate pool: the caller is waiting.
pub async fn my_access(State(state): State<AppState>, auth: Auth) -> Result<Json<Value>, Problem> {
    let pool = state.immediate.clone();
    let inner = state.clone();
    let answer = pool
        .run(async move { access_of(&inner, &auth.0).await.map_err(|err| err.to_string()) })
        .await
        .map_err(|err| Problem::unavailable(err.to_string()))?;
    Ok(Json(answer))
}

/// A principal's access to `core` and every known plugin, with the navigation and panels it may see.
pub async fn access_of(
    state: &AppState,
    principal: &Principal,
) -> Result<Value, crate::db::repositories::RepositoryError> {
    let mut offered: std::collections::BTreeMap<String, doc_plugin_protocol::Manifest> = state
        .plugins
        .list()
        .await
        .into_iter()
        .filter(|entry| state.plugins.offers(entry))
        .map(|entry| (entry.id, entry.manifest))
        .collect();
    let mut plugins: std::collections::BTreeSet<String> =
        state.repos.identity.list_plugins().await?.into_iter().collect();
    plugins.extend(offered.keys().cloned());
    let held = permissions::grants(state, principal).await;
    let admin = permissions::is_admin(&held, principal);
    let kind = permissions::member_kind(principal);
    let mut access = std::collections::BTreeMap::new();
    for plugin in std::iter::once(CORE.to_string()).chain(plugins) {
        let effective = match kind {
            Some(kind) => held.effective(kind, &plugin),
            None => Default::default(),
        };
        let read = admin || effective.allows(Access::Read);
        // The platform's own settings are an administrator's; a plugin's follow its own
        // permission, which nothing but that permission and being an admin gives.
        let settings = match plugin.as_str() {
            CORE => admin,
            _ => admin || effective.allows_settings(Access::Read),
        };
        let settings_write = match plugin.as_str() {
            CORE => admin,
            _ => admin || effective.allows_settings(Access::Write),
        };
        let manifest = offered.remove(&plugin);
        let running = manifest.is_some();
        let manifest = manifest.filter(|_| read).unwrap_or_default();
        access.insert(
            plugin,
            PluginAccess {
                read,
                write: admin || effective.allows(Access::Write),
                settings,
                settings_write,
                scope: effective.scope,
                custom: effective.custom,
                running,
                nav: manifest.nav,
                panels: manifest.resource_panels,
                insights: manifest.insights,
                dashboard: manifest.dashboard,
                links: manifest.linked_accounts,
                operations: manifest.operations,
            },
        );
    }
    let navigation = super::navigation::saved(state).await;
    let settings = super::settings::saved(state).await.unwrap_or_default();
    let linked = principal.as_user().map(|user| user.linked.clone()).unwrap_or_default();
    Ok(json!({
        "admin": admin, "plugins": access, "navigation": navigation, "linked": linked,
        "settings": settings, "categories": state.config.plugins.categories,
    }))
}

/// Answers `core.access` with the access of whoever a plugin's request is made for.
pub struct AccessService(pub AppState);

#[async_trait::async_trait]
impl doc_servicebus::ServiceHandler for AccessService {
    async fn handle(&self, request: doc_servicebus::Request) -> Result<Value, String> {
        let reference =
            request.principal.ok_or("core.access answers for the caller a plugin acts for")?;
        let principal = permissions::principal_of(&self.0, &reference).await?;
        access_of(&self.0, &principal).await.map_err(|err| err.to_string())
    }
}

pub(crate) async fn announce(state: &AppState, event: &str, task: &Task) {
    publish(state.buses.events.as_ref(), event, task, task.state.as_str(), None).await;
}

/// Who started it travels with the change, so the frontend shows it only to those who may see it.
async fn publish(
    events: &dyn doc_eventbus::EventBus,
    event: &str,
    task: &Task,
    state: &str,
    error: Option<&str>,
) {
    let name = format!("platform.task.{event}");
    let Ok(topic) = doc_eventbus::Topic::new(name.clone()) else { return };
    let detail = json!({
        "id": task.id,
        "kind": task.kind,
        "state": state,
        "started_by": task.started_by,
        "error": error,
    });
    let published = doc_eventbus::Event::new(topic, crate::fabric::SOURCE, detail);
    if let Err(err) = events.publish(published).await {
        tracing::warn!(%err, topic = %name, "could not announce a task change");
    }
}

/// Publishes `platform.task.finished` for every task a background pool finishes.
pub struct Announcer(pub std::sync::Arc<dyn doc_eventbus::EventBus>);

#[async_trait::async_trait]
impl doc_background_tasks::Finished for Announcer {
    async fn finished(&self, task: &Task, outcome: &doc_background_tasks::Outcome) {
        use doc_background_tasks::Outcome;
        let (state, error) = match outcome {
            Outcome::Succeeded(_) => ("succeeded", None),
            Outcome::Failed(reason) | Outcome::Retry(reason) => ("failed", Some(reason.as_str())),
            Outcome::Cancelled => ("cancelled", None),
        };
        publish(self.0.as_ref(), "finished", task, state, error).await;
    }
}

fn task_problem(err: &doc_background_tasks::TaskError) -> Problem {
    tracing::error!(%err, "a task store call failed");
    Problem::internal("the request could not be completed")
}

fn cron_problem(err: &doc_cron_tasks::CronError) -> Problem {
    tracing::error!(%err, "a cron store call failed");
    Problem::internal("the request could not be completed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::db::memory::{FakeHealth, FakeIdentity};
    use crate::db::repositories::{IdentityRepository, Repositories};
    use crate::fabric::Buses;
    use crate::identity::TokenOwner;
    use crate::secrets::TokenKind;
    use crate::testing::{get_as, patch_json, post_json};
    use axum::Router;
    use chrono::Utc;
    use doc_background_tasks::store::MemoryTasks;
    use doc_background_tasks::{NewTask, TaskStore};
    use doc_cron_tasks::{CronStore, MemoryCron};
    use std::sync::Arc;

    const ADMIN: &str = "doc_ses_admin";
    const PLAIN: &str = "doc_ses_plain";

    struct Harness {
        app: Router,
        identity: Arc<FakeIdentity>,
        tasks: Arc<MemoryTasks>,
        cron: Arc<MemoryCron>,
        admin: Actor,
        plain: Actor,
    }

    fn harness() -> Harness {
        let mut config = Config::default();
        config.bootstrap.admins = vec!["root".into()];
        let identity = FakeIdentity::empty();
        let tasks = MemoryTasks::new();
        let cron = MemoryCron::new();

        let root = identity.add_user("root");
        identity.give(ADMIN, TokenKind::Session, TokenOwner::User(root.id), None, false);
        let ada = identity.add_user("ada");
        identity.give(PLAIN, TokenKind::Session, TokenOwner::User(ada.id), None, false);

        let repos = Repositories {
            health: FakeHealth::up(),
            identity: identity.clone(),
            teams: identity.clone(),
            plugins: crate::db::memory::FakePlugins::empty(),
            plugin_status: crate::db::memory::FakePluginStatus::empty(),
            tasks: tasks.clone(),
            cron: cron.clone(),
            status_history: crate::db::memory::FakeStatusHistory::empty(),
            data: crate::data::memory::MemoryData::new(),
        };
        let app = super::super::router(AppState::new(config, repos, Buses::in_memory()));
        let owner = |id: Uuid, label: &str| Actor {
            kind: "user".into(),
            id: Some(id.to_string()),
            label: Some(label.into()),
        };
        Harness {
            app,
            identity,
            tasks,
            cron,
            admin: owner(root.id, "root"),
            plain: owner(ada.id, "ada"),
        }
    }

    impl Harness {
        async fn seed(&self, kind: &str, owner: Actor) -> Uuid {
            let task = NewTask::new(kind, json!({})).by(owner);
            self.tasks.create(task).await.expect("seeded").id
        }
    }

    #[tokio::test]
    async fn listing_tasks_needs_platform_write() {
        let app = harness();
        app.seed("core.sleep", app.admin.clone()).await;
        let (status, _, _) = get_as(&app.app, "/api/v1/tasks", PLAIN).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "an ordinary user cannot list every task");
        let (status, body, _) = get_as(&app.app, "/api/v1/tasks", ADMIN).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_array().expect("tasks").len(), 1);
    }

    #[tokio::test]
    async fn a_task_is_readable_by_whoever_started_it_and_by_nobody_else() {
        let app = harness();
        let id = app.seed("core.sleep", app.plain.clone()).await;

        let (status, body, _) = get_as(&app.app, &format!("/api/v1/tasks/{id}"), PLAIN).await;
        assert_eq!(status, StatusCode::OK, "the caller who started it may read it");
        assert_eq!(body["kind"], "core.sleep");

        let other = app.identity.add_user("mallory");
        app.identity.give(
            "doc_ses_mallory",
            TokenKind::Session,
            TokenOwner::User(other.id),
            None,
            false,
        );
        let (status, _, _) =
            get_as(&app.app, &format!("/api/v1/tasks/{id}"), "doc_ses_mallory").await;
        assert_eq!(status, StatusCode::FORBIDDEN, "a stranger may not read it");

        let (status, _, _) = get_as(&app.app, &format!("/api/v1/tasks/{id}"), ADMIN).await;
        assert_eq!(status, StatusCode::OK, "core read access sees any task");
    }

    #[tokio::test]
    async fn an_unknown_task_is_not_found() {
        let app = harness();
        let (status, _, _) =
            get_as(&app.app, &format!("/api/v1/tasks/{}", Uuid::now_v7()), ADMIN).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn starting_a_task_queues_it() {
        let app = harness();
        let (status, body, _) = post_json(
            &app.app,
            "/api/v1/tasks",
            Some(ADMIN),
            json!({ "kind": "core.sleep", "payload": { "seconds": 1 } }),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(body["state"], "queued");
        assert_eq!(body["kind"], "core.sleep");
        assert_eq!(body["started_by"]["label"], "root");

        let (status, _, _) =
            post_json(&app.app, "/api/v1/tasks", Some(PLAIN), json!({ "kind": "core.sleep" }))
                .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "starting a task needs plugin:core:user:rw");
    }

    #[tokio::test]
    async fn a_task_needs_a_kind() {
        let app = harness();
        let (status, _, _) =
            post_json(&app.app, "/api/v1/tasks", Some(ADMIN), json!({ "kind": "  " })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn cancelling_a_queued_task_cancels_it_outright() {
        let app = harness();
        let id = app.seed("core.sleep", app.admin.clone()).await;
        let (status, _, _) =
            post_json(&app.app, &format!("/api/v1/tasks/{id}/cancel"), Some(ADMIN), json!({}))
                .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let task = app.tasks.get(id).await.expect("read").expect("task");
        assert_eq!(task.state, TaskState::Cancelled, "a queued task never starts");
    }

    #[tokio::test]
    async fn cancelling_a_running_task_only_asks_it_to_stop() {
        let app = harness();
        let id = app.seed("core.sleep", app.admin.clone()).await;
        app.tasks.claim(id, std::time::Duration::from_secs(60)).await.expect("claimed");
        let (status, _, _) =
            post_json(&app.app, &format!("/api/v1/tasks/{id}/cancel"), Some(ADMIN), json!({}))
                .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let task = app.tasks.get(id).await.expect("read").expect("task");
        assert_eq!(task.state, TaskState::Running, "it is still running until it gives up");
        assert!(task.cancel_requested, "but it has been asked to stop");
    }

    /// The path a crashed worker leaves behind: the row says running, but nothing is renewing it.
    #[tokio::test]
    async fn a_task_whose_lease_ran_out_can_be_taken_again() {
        let app = harness();
        let id = app.seed("core.sleep", app.admin.clone()).await;
        let lease = std::time::Duration::from_secs(20);
        assert!(app.tasks.claim(id, lease).await.expect("claimed").is_some());
        assert!(
            app.tasks.claim(id, lease).await.expect("read").is_none(),
            "a task someone is still holding is not handed out twice"
        );
        app.tasks.strand(id);
        let again = app.tasks.claim(id, lease).await.expect("read").expect("reclaimed");
        assert_eq!(again.attempts, 2, "the second run is counted");
    }

    #[tokio::test]
    async fn cancelling_a_finished_task_is_a_conflict() {
        let app = harness();
        let id = app.seed("core.sleep", app.admin.clone()).await;
        app.tasks
            .finish(id, doc_background_tasks::Outcome::Succeeded(json!({})))
            .await
            .expect("finished");
        let (status, _, _) =
            post_json(&app.app, &format!("/api/v1/tasks/{id}/cancel"), Some(ADMIN), json!({}))
                .await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn cancelling_needs_platform_write() {
        let app = harness();
        let id = app.seed("core.sleep", app.plain.clone()).await;
        let (status, _, _) =
            post_json(&app.app, &format!("/api/v1/tasks/{id}/cancel"), Some(PLAIN), json!({}))
                .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_kind_of_task_can_be_paused_and_resumed() {
        let app = harness();
        let (status, _, _) =
            patch_json(&app.app, "/api/v1/task-kinds/core.sleep", ADMIN, json!({ "paused": true }))
                .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(app.tasks.paused_kinds().await.expect("paused"), ["core.sleep"]);

        let (status, _, _) = patch_json(
            &app.app,
            "/api/v1/task-kinds/core.sleep",
            ADMIN,
            json!({ "paused": false }),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(app.tasks.paused_kinds().await.expect("paused").is_empty());

        let (status, _, _) =
            patch_json(&app.app, "/api/v1/task-kinds/core.sleep", PLAIN, json!({ "paused": true }))
                .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_cron_task_can_be_paused_and_resumed() {
        let app = harness();
        app.cron
            .upsert("core.expire-tokens", "*/15 * * * *", Some("expiry"), Utc::now())
            .await
            .expect("registered");

        let (status, body, _) = get_as(&app.app, "/api/v1/cron", ADMIN).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body[0]["name"], "core.expire-tokens");
        assert_eq!(body[0]["paused"], false);

        let (status, _, _) = patch_json(
            &app.app,
            "/api/v1/cron/core.expire-tokens",
            ADMIN,
            json!({ "paused": true }),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let task = app.cron.get("core.expire-tokens").await.expect("read").expect("task");
        assert!(task.paused);

        let (status, _, _) = patch_json(
            &app.app,
            "/api/v1/cron/core.expire-tokens",
            ADMIN,
            json!({ "paused": false }),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(!app.cron.get("core.expire-tokens").await.expect("read").expect("task").paused);
    }

    #[tokio::test]
    async fn pausing_an_unknown_cron_task_is_not_found() {
        let app = harness();
        let (status, _, _) =
            patch_json(&app.app, "/api/v1/cron/nope", ADMIN, json!({ "paused": true })).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn listing_can_be_filtered() {
        let app = harness();
        app.seed("core.sleep", app.admin.clone()).await;
        let other = app.seed("core.other", app.plain.clone()).await;
        app.tasks.finish(other, doc_background_tasks::Outcome::Failed("no".into())).await.ok();

        let (_, body, _) = get_as(&app.app, "/api/v1/tasks?kind=core.other", ADMIN).await;
        assert_eq!(body.as_array().expect("tasks").len(), 1);
        let (_, body, _) = get_as(&app.app, "/api/v1/tasks?state=failed", ADMIN).await;
        assert_eq!(body[0]["id"], other.to_string());
        let (_, body, _) = get_as(&app.app, "/api/v1/tasks?mine=true", ADMIN).await;
        assert_eq!(body.as_array().expect("tasks").len(), 1, "only what this caller started");
        assert_eq!(body[0]["kind"], "core.sleep");
    }

    #[tokio::test]
    async fn me_access_reports_every_plugin_in_one_call() {
        let app = harness();
        app.identity.register_plugin("rbac").await.expect("registered");
        app.identity.register_plugin("kb").await.expect("registered");

        let (status, body, _) = get_as(&app.app, "/api/v1/me/access", PLAIN).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["admin"], false);
        let plugins = body["plugins"].as_object().expect("plugins");
        assert_eq!(plugins.len(), 3, "core plus both registered plugins");
        assert_eq!(body["plugins"]["core"]["read"], false, "an ordinary user holds nothing yet");
        assert_eq!(body["plugins"]["kb"]["write"], false);
    }

    #[tokio::test]
    async fn me_access_shows_an_admin_reaching_everything() {
        let app = harness();
        app.identity.register_plugin("kb").await.expect("registered");
        let (status, body, _) = get_as(&app.app, "/api/v1/me/access", ADMIN).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["admin"], true);
        assert_eq!(body["plugins"]["core"]["write"], true);
        assert_eq!(body["plugins"]["kb"]["write"], true, "a platform admin passes every check");
    }

    #[tokio::test]
    async fn me_access_needs_only_authentication() {
        let app = harness();
        let (status, _, _) = get_as(&app.app, "/api/v1/me/access", "doc_ses_nosuch").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}
