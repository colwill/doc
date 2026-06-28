//! A plugin's Settings, Features and Permissions (ADR-0007). These routes are core's, guarded by
//! the plugin's own `plugin:<plugin>:settings` permission rather than by access to the plugin:
//! using a plugin and configuring it are different jobs, so writing to one is no leave to change
//! its credentials. A platform administrator passes, as everywhere.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Path, State};
use doc_permissions::{Access, CORE, Scope};
use doc_plugin_protocol::{Feature, Manifest, PermissionKind, Setting};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::AppState;
use super::problem::Problem;
use crate::auth::Auth;
use crate::identity::Principal;
use crate::permissions;
use crate::plugins::access;
use crate::plugins::settings::{self, Current, SaveError};

/// The settings and features tabs in one answer, with the schema the page is built from.
#[derive(Debug, Serialize)]
pub struct SettingsPage {
    pub plugin: String,
    pub version: Option<String>,
    /// Whether this caller may change any of it, so a page shows the form or only the values.
    pub writes: bool,
    pub settings: Vec<Setting>,
    pub current: Vec<Current>,
    pub features: Vec<FeatureView>,
    /// What the plugin says a credential it holds by name means, when it holds any.
    pub named_secrets: Option<doc_plugin_protocol::NamedSecrets>,
    /// Those credentials by name, never by value.
    pub named: Vec<settings::NamedSecret>,
    /// Required settings nothing has set, which is why an unconfigured plugin serves nothing.
    pub missing: Vec<String>,
    /// True once the platform has a settings key, without which no secret can be stored.
    pub secrets_available: bool,
    /// Other plugins asking to join one of its requestable settings (DOC-SPEC §9.15).
    pub requests: Vec<Value>,
    /// What its secret settings may point at in Secret Storage instead (FEAT-SECRETS).
    pub store: Option<settings::StoreOffer>,
}

#[derive(Debug, Serialize)]
pub struct FeatureView {
    #[serde(flatten)]
    pub feature: Feature,
    pub enabled: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct SaveRequest {
    pub values: BTreeMap<String, Value>,
    /// Credentials a plugin holds by name: a value sets one, `null` takes it away (ADR-0007).
    pub named_secrets: BTreeMap<String, Option<String>>,
    /// Credentials held by name that point at a secret in Secret Storage instead (FEAT-SECRETS).
    pub named_from_store: BTreeMap<String, uuid::Uuid>,
}

impl SaveRequest {
    /// The credentials by name this save sets or clears, typed in or pointed at Secret Storage.
    fn named(&self) -> settings::NamedChanges {
        let mut named: settings::NamedChanges = self
            .named_secrets
            .iter()
            .map(|(name, value)| (name.clone(), value.clone().map(settings::Credential::Typed)))
            .collect();
        for (name, secret) in &self.named_from_store {
            named.insert(name.clone(), Some(settings::Credential::Stored(*secret)));
        }
        named
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct FeaturesRequest {
    pub features: BTreeMap<String, bool>,
}

/// Whoever is asking, and what they may do with this plugin's settings. `ro` sees them, `rw`
/// changes them, and a platform administrator does both.
pub(crate) async fn admitted(
    state: &AppState,
    principal: &Principal,
    plugin: &str,
    access: Access,
) -> Result<bool, Problem> {
    if permissions::holds(state, principal, CORE, Access::Write).await {
        return Ok(true);
    }
    let held = permissions::grants(state, principal).await;
    let Some(kind) = permissions::member_kind(principal) else { return Ok(false) };
    Ok(held.effective(kind, plugin).allows_settings(access))
}

async fn allow(
    state: &AppState,
    principal: &Principal,
    plugin: &str,
    access: Access,
) -> Result<(), Problem> {
    if admitted(state, principal, plugin, access).await? {
        return Ok(());
    }
    let scope = match access {
        Access::Read => Scope::Ro,
        Access::Write => Scope::Rw,
    };
    Err(Problem::forbidden(format!("needs plugin:{plugin}:settings:{scope}"))
        .with("plugin", plugin))
}

/// A plugin nobody has ever registered has no settings to show, so it is a 404 rather than an
/// empty page: the platform does not invent plugins.
async fn manifest(state: &AppState, plugin: &str) -> Result<Manifest, Problem> {
    settings::manifest_of(state, plugin).await.ok_or_else(|| Problem::not_found("plugin"))
}

pub async fn show(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<String>,
) -> Result<Json<SettingsPage>, Problem> {
    allow(&state, &auth.0, &id, Access::Read).await?;
    let manifest = manifest(&state, &id).await?;
    let resolved = settings::resolve(&state, &id, &manifest).await;
    let features = manifest
        .features
        .iter()
        .map(|feature| FeatureView {
            enabled: resolved.feature(&feature.name),
            feature: feature.clone(),
        })
        .collect();
    let writes = admitted(&state, &auth.0, &id, Access::Write).await?;
    let requests = access::listed(&state, &id, &manifest).await;
    let store = settings::store_offer(&state, &id, &manifest).await;
    Ok(Json(SettingsPage {
        plugin: id,
        version: Some(manifest.version.clone()).filter(|version| !version.is_empty()),
        writes,
        settings: manifest.settings.clone(),
        current: resolved.current,
        features,
        named_secrets: manifest.named_secrets.clone(),
        named: resolved.named_held,
        missing: resolved.missing,
        secrets_available: state.settings_keys.available(),
        requests,
        store,
    }))
}

/// An administrator's answer to another plugin asking to join one of this plugin's requestable
/// settings. Approving adds it through an ordinary save, checked and audited like any other.
pub async fn decide_access(
    State(state): State<AppState>,
    auth: Auth,
    Path((plugin, id, verdict)): Path<(String, uuid::Uuid, String)>,
) -> Result<Json<Value>, Problem> {
    let approve = match verdict.as_str() {
        "approve" => true,
        "deny" => false,
        _ => return Err(Problem::not_found("endpoint")),
    };
    allow(&state, &auth.0, &plugin, Access::Write).await?;
    let record = state
        .repos
        .plugins
        .access_request(id)
        .await?
        .filter(|record| record.target == plugin)
        .ok_or_else(|| Problem::not_found("access request"))?;
    if record.state != access::PENDING {
        return Err(Problem::conflict(format!("this request was already {}", record.state))
            .with("plugin", plugin.as_str()));
    }
    if approve {
        let manifest = manifest(&state, &plugin).await?;
        let Some(setting) = access::requestable(&manifest, &record.setting) else {
            return Err(Problem::conflict(format!(
                "{plugin} no longer lets other plugins ask to join `{}`",
                record.setting
            )));
        };
        let mut members = access::members(&state, &plugin, &manifest, &setting.key).await;
        if !members.contains(&record.requester) {
            members.push(record.requester.clone());
            let values = BTreeMap::from([(setting.key.clone(), json!(members))]);
            settings::save(&state, &plugin, &manifest, &values, &BTreeMap::new(), &auth.0)
                .await
                .map_err(|err| refused(&plugin, err))?;
        }
    }
    let decided = access::decided(&state, record, &auth.0, approve).await?;
    Ok(Json(json!({ "id": decided.id, "requester": decided.requester, "state": decided.state })))
}

pub async fn save(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<String>,
    Json(request): Json<SaveRequest>,
) -> Result<Json<Value>, Problem> {
    allow(&state, &auth.0, &id, Access::Write).await?;
    let manifest = manifest(&state, &id).await?;
    let saved = settings::save(&state, &id, &manifest, &request.values, &request.named(), &auth.0)
        .await
        .map_err(|err| refused(&id, err))?;
    Ok(Json(json!({ "plugin": id, "changed": saved.keys, "message": saved.message })))
}

/// **Test connection**: the plugin's opinion of these settings, with nothing stored. Changing
/// nothing, but it spends the plugin's credentials, so it needs write access like a save.
pub async fn check(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<String>,
    Json(request): Json<SaveRequest>,
) -> Result<Json<Value>, Problem> {
    allow(&state, &auth.0, &id, Access::Write).await?;
    let manifest = manifest(&state, &id).await?;
    let verdict = settings::test(&state, &id, &manifest, &request.values)
        .await
        .map_err(|err| refused(&id, err))?;
    Ok(Json(json!({
        "plugin": id,
        "ok": verdict.is_ok(),
        "problems": verdict.problems,
        "problem": verdict.problem,
        "message": verdict.message,
    })))
}

pub async fn set_features(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<String>,
    Json(request): Json<FeaturesRequest>,
) -> Result<Json<Value>, Problem> {
    allow(&state, &auth.0, &id, Access::Write).await?;
    let manifest = manifest(&state, &id).await?;
    let saved = settings::set_features(&state, &id, &manifest, &request.features, &auth.0)
        .await
        .map_err(|err| refused(&id, err))?;
    Ok(Json(json!({ "plugin": id, "changed": saved.features })))
}

/// **Enable** on the plugins page: the plugin's features that work from other plugins' features,
/// and those features too, in one go (FEAT-DORA). It changes several plugins' settings, so the
/// caller must be able to change every one of them before anything is switched.
pub async fn enable(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<String>,
) -> Result<Json<Value>, Problem> {
    allow(&state, &auth.0, &id, Access::Write).await?;
    let manifest = manifest(&state, &id).await?;
    let Some(offer) = settings::enabling(&state, &id, &manifest).await else {
        return Ok(Json(json!({ "plugin": id, "changed": [] })));
    };
    if let Some(blocked) = offer.blocked {
        return Err(Problem::conflict(format!("{id} {blocked}")).with("plugin", id.as_str()));
    }
    let mut theirs: BTreeMap<String, BTreeMap<String, bool>> = BTreeMap::new();
    for (other, feature) in &offer.with {
        allow(&state, &auth.0, other, Access::Write).await?;
        theirs.entry(other.clone()).or_default().insert(feature.clone(), true);
    }
    // Theirs first, so whatever this plugin starts at its next load finds the data flowing.
    let mut changed = Vec::new();
    for (other, wanted) in &theirs {
        let their_manifest = self::manifest(&state, other).await?;
        let saved = settings::set_features(&state, other, &their_manifest, wanted, &auth.0)
            .await
            .map_err(|err| refused(other, err))?;
        changed.extend(saved.features.iter().map(|feature| format!("{other}:{feature}")));
    }
    let wanted = offer.features.iter().map(|feature| (feature.clone(), true)).collect();
    let saved = settings::set_features(&state, &id, &manifest, &wanted, &auth.0)
        .await
        .map_err(|err| refused(&id, err))?;
    changed.extend(saved.features.iter().map(|feature| format!("{id}:{feature}")));
    Ok(Json(json!({ "plugin": id, "changed": changed })))
}

/// A save that failed against a field says which field, so the page can put it there.
fn refused(plugin: &str, err: SaveError) -> Problem {
    match err {
        SaveError::Problems(problems) => Problem::bad_request("some of these cannot be saved")
            .with("plugin", plugin)
            .with("problems", json!(problems)),
        SaveError::Refused(detail) => {
            Problem::bad_request(detail).with("plugin", plugin).with("refused", true)
        }
        SaveError::NoKey => Problem::internal(
            "this platform has no settings key, so a secret cannot be stored; run the bootstrap",
        ),
        SaveError::Storage(detail) => Problem::internal(detail),
    }
}

/// What each of the plugin's permissions allows, and who holds it: the Permissions tab. Reading it
/// needs the settings permission *and* read access to RBAC, since it names people.
pub async fn permissions_tab(
    State(state): State<AppState>,
    auth: Auth,
    Path(plugin): Path<String>,
) -> Result<Json<Value>, Problem> {
    let (state, principal, plugin) = (&state, &auth.0, plugin.as_str());
    allow(state, principal, plugin, Access::Read).await?;
    if !permissions::holds(state, principal, "rbac", Access::Read).await
        && !permissions::holds(state, principal, CORE, Access::Write).await
    {
        return Err(Problem::forbidden(
            "needs plugin:rbac:user:ro, since this names who holds what",
        )
        .with("plugin", plugin));
    }
    let manifest = manifest(state, plugin).await?;
    let described = |kind: PermissionKind, name: &str| {
        manifest
            .custom_permissions
            .iter()
            .find(|custom| custom.kind == kind && custom.name == name)
            .map(|custom| custom.description.clone())
            .unwrap_or_default()
    };
    let declared = state.repos.plugins.permissions(plugin).await?;
    let mut listed = Vec::new();
    for permission in &declared {
        let written = match permission.name.as_str() {
            "" => format!("plugin:{plugin}:{}", permission.kind),
            name => format!("plugin:{plugin}:{}:{name}", permission.kind),
        };
        let description = match permission.kind.as_str() {
            "user" => "Using the plugin's own pages and API.".to_string(),
            "service" => "The same, for a service account.".to_string(),
            "pluginuser" => described(PermissionKind::PluginUser, &permission.name),
            "pluginservice" => described(PermissionKind::PluginService, &permission.name),
            _ => String::new(),
        };
        listed.push(json!({
            "permission": written,
            "kind": permission.kind,
            "name": permission.name,
            "description": description,
            "holders": holders(state, &written).await,
        }));
    }
    let settings_permission = format!("plugin:{plugin}:settings");
    listed.push(json!({
        "permission": settings_permission,
        "kind": "settings",
        "name": "",
        "description": "Seeing and changing this plugin's settings and features.",
        "holders": holders(state, &settings_permission).await,
    }));
    Ok(Json(json!({ "plugin": plugin, "permissions": listed })))
}

/// Who holds one permission, from the permission provider. A provider that cannot answer leaves
/// the list out rather than failing the page: the permissions themselves are still worth showing.
async fn holders(state: &AppState, permission: &str) -> Value {
    let asked = permissions::ask_rbac(
        state.buses.services.as_ref(),
        "permission-holders",
        json!({ "permission": permission }),
    )
    .await;
    match asked {
        // Whatever the provider names them by, each holder is shown as one line, so a provider
        // that answers with records rather than labels cannot break the page.
        Ok(answer) => match answer.get("holders").and_then(Value::as_array) {
            Some(holders) => json!(
                holders
                    .iter()
                    .map(|holder| match holder {
                        Value::String(label) => label.clone(),
                        other => other
                            .get("label")
                            .or_else(|| other.get("name"))
                            .and_then(Value::as_str)
                            .map(str::to_string)
                            .unwrap_or_else(|| other.to_string()),
                    })
                    .collect::<Vec<_>>()
            ),
            None => Value::Null,
        },
        Err(err) => {
            tracing::debug!(permission, %err, "who holds this could not be read");
            Value::Null
        }
    }
}

#[cfg(test)]
mod tests {
    use doc_plugin_protocol::{Feature, Setting, SettingKind};
    use http::StatusCode;
    use serde_json::{Value, json};

    use crate::plugins::settings;
    use crate::testing::{ADMIN, get_as, plugin_host, post_json, put_json};

    /// `hello`, with a list another plugin may ask to join and one it may not.
    fn granting() -> doc_plugin_protocol::Manifest {
        let mut manifest = crate::testing::manifest("hello", "1.0.0");
        manifest.settings = vec![
            Setting::new("allowed", "Plugins allowed", SettingKind::List)
                .defaulting(json!(["kb"]))
                .requestable(),
            Setting::new("admins", "Admins", SettingKind::List),
        ];
        manifest
    }

    fn asked(setting: &str) -> doc_plugin_protocol::calls::AccessRequest {
        doc_plugin_protocol::calls::AccessRequest {
            plugin: "hello".into(),
            setting: setting.into(),
            reason: "To scan repositories.".into(),
        }
    }

    /// A plugin asks once however often it is refused; somebody who may only read the settings
    /// cannot decide; an administrator's approval adds it to the list, keeping what was there.
    #[tokio::test]
    async fn a_plugin_asks_once_and_an_administrator_approves_it() {
        let host = plugin_host();
        host.register(granting()).await;
        let ask = || crate::plugins::access::ask(&host.state, "insights", asked("allowed"));

        let first = ask().await.expect("asked");
        assert_eq!((first.state.as_str(), first.raised), ("pending", true));
        let again = ask().await.expect("asked again");
        assert_eq!((again.state.as_str(), again.raised, again.id), ("pending", false, first.id));

        let (_, page, _) = get_as(&host.app, "/api/v1/plugins/hello/settings", ADMIN).await;
        assert_eq!(page["requests"].as_array().map(Vec::len), Some(1), "{page}");
        assert_eq!(page["requests"][0]["requester"], "insights");
        assert_eq!(page["requests"][0]["setting_label"], "Plugins allowed");

        let id = first.id.expect("an id");
        let path = format!("/api/v1/plugins/hello/access-requests/{id}/approve");
        let reader = host.user_holding("reader", &["plugin:hello:settings:ro"]).await;
        let (status, body, _) = post_json(&host.app, &path, Some(&reader), json!({})).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

        let (status, body, _) = post_json(&host.app, &path, Some(ADMIN), json!({})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["state"], "approved");
        let (_, page, _) = get_as(&host.app, "/api/v1/plugins/hello/settings", ADMIN).await;
        let allowed = page["current"].as_array().and_then(|current| {
            current.iter().find(|held| held["key"] == "allowed").map(|held| held["value"].clone())
        });
        assert_eq!(allowed, Some(json!(["kb", "insights"])), "the default is kept");

        let (status, _, _) = post_json(&host.app, &path, Some(ADMIN), json!({})).await;
        assert_eq!(status, StatusCode::CONFLICT, "a request is decided once");
        assert_eq!(ask().await.expect("asked").state, "granted");
    }

    /// A denied request stays denied without telling anybody again, and nothing but a plugin
    /// joining a list declared requestable can be asked for.
    #[tokio::test]
    async fn only_requestable_lists_are_asked_for_and_a_denial_stands() {
        let host = plugin_host();
        host.register(granting()).await;
        let access = |requester: &'static str, request| {
            crate::plugins::access::ask(&host.state, requester, request)
        };
        let refused = access("insights", asked("admins")).await.expect_err("not requestable");
        assert_eq!(refused.kind, "not-requestable");
        let refused = access("hello", asked("allowed")).await.expect_err("itself");
        assert_eq!(refused.status, 400);
        let mut elsewhere = asked("allowed");
        elsewhere.plugin = "nowhere".into();
        assert_eq!(access("insights", elsewhere).await.expect_err("no plugin").status, 404);

        let first = access("insights", asked("allowed")).await.expect("asked");
        let id = first.id.expect("an id");
        let path = format!("/api/v1/plugins/hello/access-requests/{id}/deny");
        let (status, body, _) = post_json(&host.app, &path, Some(ADMIN), json!({})).await;
        assert_eq!((status, body["state"].as_str()), (StatusCode::OK, Some("denied")));
        let again = access("insights", asked("allowed")).await.expect("asked again");
        assert_eq!((again.state.as_str(), again.raised), ("denied", false));

        let (_, page, _) = get_as(&host.app, "/api/v1/plugins/hello/settings", ADMIN).await;
        let allowed = page["current"].as_array().and_then(|current| {
            current.iter().find(|held| held["key"] == "allowed").map(|held| held["value"].clone())
        });
        assert_eq!(allowed, Some(json!(["kb"])), "a denial changes nothing");
    }

    fn configurable() -> doc_plugin_protocol::Manifest {
        let mut manifest = crate::testing::manifest("kb", "1.0.0");
        manifest.settings = vec![
            Setting::text("base-url", "Base URL").required(),
            Setting::new("days", "Days to keep", SettingKind::Number).between(1.0, 90.0),
            Setting::secret("token", "API token"),
            Setting::text("area", "Area").one_of(&["one", "two"]),
        ];
        manifest.features = vec![Feature::new("sync", "Sync", "Syncs every night")];
        manifest
    }

    /// The whole journey a Settings page makes: what it shows before anything is set, a save that
    /// is refused field by field, one that is taken, and what a secret is ever said to be.
    #[tokio::test]
    async fn settings_are_checked_stored_and_never_read_back() {
        let host = plugin_host();
        host.register(configurable()).await;

        let (status, body, _) = get_as(&host.app, "/api/v1/plugins/kb/settings", ADMIN).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["settings"].as_array().map(Vec::len), Some(4));
        assert_eq!(body["missing"], json!(["base-url"]), "a required setting nobody set");
        assert_eq!(body["current"][0]["source"], "default");
        assert_eq!(body["features"][0]["enabled"], false, "off until somebody turns it on");

        let refused = put_json(
            &host.app,
            "/api/v1/plugins/kb/settings",
            ADMIN,
            json!({ "values": { "days": 900, "area": "three", "nope": "x" } }),
        )
        .await;
        assert_eq!(refused.0, StatusCode::BAD_REQUEST, "{}", refused.1);
        let problems = &refused.1["problems"];
        assert!(problems["days"].as_str().is_some_and(|said| said.contains("at most 90")));
        assert!(problems["area"].as_str().is_some_and(|said| said.contains("one, two")));
        assert!(problems["nope"].as_str().is_some_and(|said| said.contains("no setting")));

        let (status, body, _) = put_json(
            &host.app,
            "/api/v1/plugins/kb/settings",
            ADMIN,
            json!({ "values": { "base-url": "https://kb.example", "days": 30, "token": "s3cret" } }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["changed"].as_array().map(Vec::len), Some(3));

        let (_, body, _) = get_as(&host.app, "/api/v1/plugins/kb/settings", ADMIN).await;
        let by_key = |key: &str| {
            body["current"]
                .as_array()
                .expect("settings")
                .iter()
                .find(|current| current["key"] == key)
                .cloned()
                .expect("the setting")
        };
        assert_eq!(by_key("base-url")["value"], "https://kb.example");
        assert_eq!(by_key("days")["value"], 30.0);
        assert_eq!(by_key("token")["value"], Value::Null, "a secret never comes back");
        assert_eq!(by_key("token")["set"], true, "only that it is set");
        assert_eq!(body["missing"], json!([]), "nothing is missing now");
        assert!(!body.to_string().contains("s3cret"), "no answer carries a secret");

        // The plugin is told, and the change is audited without the secret's value.
        let told = host.plugin.settings_told();
        assert_eq!(told.len(), 1, "the plugin hears about it once");
        assert!(told[0].keys.contains(&"token".to_string()));
        let audit: Vec<_> = host
            .identity
            .audit_entries()
            .into_iter()
            .filter(|entry| entry.action == "plugin.settings.changed")
            .collect();
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].detail["values"]["token"], "changed", "changed, never what to");
        assert_eq!(audit[0].detail["values"]["base-url"], "https://kb.example");
    }

    /// A secret setting and a credential held by name point at Secret Storage (FEAT-SECRETS):
    /// only a secret the store shares with the plugin is taken, the value reaches that plugin and
    /// nothing else, and a change to the secret is passed on. Sealing is the store's alone, and
    /// what it seals opens only under the label it was sealed with.
    #[tokio::test]
    async fn a_secret_setting_points_at_the_store_and_only_its_plugin_is_given_the_value() {
        use doc_plugin_protocol::calls::{OpenRequest, SealRequest, SecretsChangedRequest};
        use doc_plugin_protocol::{Capability, NamedSecrets, Secret};

        use crate::plugins::api;
        use crate::plugins::client::Answer;

        let mut config = crate::config::Config::default();
        config.bootstrap.admins = vec!["tester".into()];
        config.plugins.ids = vec!["kb".into(), "secrets".into()];
        config.plugins.capabilities.insert("secrets".into(), vec![Capability::SecretStore]);
        let host = crate::testing::plugin_host_with(config);
        let mut store = crate::testing::manifest("secrets", "1.0.0");
        store.capabilities = vec![Capability::SecretStore];
        host.register(store).await;
        let mut kb = crate::testing::manifest("kb", "1.0.0");
        kb.settings = vec![Setting::secret("token", "API token")];
        kb.named_secrets = Some(NamedSecrets { label: "Source".into(), hint: String::new() });
        host.register(kb.clone()).await;

        let (shared, unshared) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        host.plugin.answer_with(Answer::json(
            StatusCode::OK,
            &json!({ "secrets": {
                shared.to_string(): { "label": "platform/kb-token", "value": "from-the-store" },
                unshared.to_string(): { "label": "payments/other", "problem": "not shared with kb" },
            } }),
        ));

        let path = "/api/v1/plugins/kb/settings";
        let refused = put_json(
            &host.app,
            path,
            ADMIN,
            json!({ "values": { "token": { "secret": unshared } } }),
        )
        .await;
        assert_eq!(refused.0, StatusCode::BAD_REQUEST, "{}", refused.1);
        assert_eq!(refused.1["problems"]["token"], "not shared with kb");

        let body = json!({
            "values": { "token": { "secret": shared } },
            "named_from_store": { "confluence": shared },
        });
        let (status, saved, _) = put_json(&host.app, path, ADMIN, body).await;
        assert_eq!(status, StatusCode::OK, "{saved}");
        let asked = host.plugin.requests();
        let (forwarded, caller) =
            asked.iter().rev().find(|(forwarded, _)| forwarded.path == "internal/resolve").unwrap();
        assert_eq!(caller.kind, "platform", "core asks the store as the platform");
        let payload: Value = serde_json::from_slice(&forwarded.body).unwrap();
        assert_eq!(payload["plugin"], "kb", "for the plugin the setting belongs to");

        let (_, page, _) = get_as(&host.app, path, ADMIN).await;
        assert!(!page.to_string().contains("from-the-store"), "no page carries the value");
        let token = &page["current"][0];
        assert_eq!((token["set"].clone(), token["value"].clone()), (json!(true), Value::Null));
        assert_eq!(token["from_store"]["secret"], shared.to_string());
        assert_eq!(token["from_store"]["label"], "platform/kb-token");
        assert_eq!(page["named"][0]["from_store"]["label"], "platform/kb-token");

        let given = settings::resolve(&host.state, "kb", &kb).await.for_plugin();
        assert_eq!(
            given.secrets.get("token").map(|value| value.expose().as_str()),
            Some("from-the-store")
        );
        assert_eq!(
            given.named.get("confluence").map(|value| value.expose().as_str()),
            Some("from-the-store")
        );
        let audited = host.identity.audit_entries();
        let changed = audited.iter().find(|entry| entry.action == "plugin.settings.changed");
        assert_eq!(
            changed.map(|entry| entry.detail["values"]["token"].clone()),
            Some(json!("from Secret Storage: platform/kb-token")),
            "recorded by name"
        );

        // Only the store may say its secrets changed; when it does, the plugin reads them again.
        let change = || SecretsChangedRequest { secrets: vec![shared], loaded: false };
        assert_eq!(
            api::secrets_changed(&host.state, "kb", change()).await.unwrap_err().status,
            403
        );
        let before = host.plugin.settings_told().len();
        let told = api::secrets_changed(&host.state, "secrets", change()).await.expect("told");
        assert_eq!(told.told, vec!["kb".to_string()]);
        let heard = host.plugin.settings_told();
        assert_eq!(heard.len(), before + 1);
        assert!(heard[before].keys.contains(&"token".to_string()));
        assert!(heard[before].keys.contains(&"named:confluence".to_string()));

        let sealing =
            SealRequest { label: "secret/1".into(), value: Secret::new("hunter2".into()) };
        assert_eq!(api::seal(&host.state, "kb", sealing.clone()).await.unwrap_err().status, 403);
        let sealed = api::seal(&host.state, "secrets", sealing).await.expect("sealed").sealed;
        assert!(!sealed.ciphertext.contains("hunter2"));
        let opening = |label: &str| OpenRequest { label: label.into(), sealed: sealed.clone() };
        let opened = api::open(&host.state, "secrets", opening("secret/1")).await.expect("opened");
        assert_eq!((opened.value.expose().as_str(), opened.stale), ("hunter2", false));
        let elsewhere = api::open(&host.state, "secrets", opening("secret/2")).await;
        assert_eq!(elsewhere.unwrap_err().status, 400, "bound to the label it was sealed with");
    }

    /// A `map` is taken as a form or a deployment writes it, `KEY=one,two` a line, or as an object,
    /// and stored as lists; a value given to no key is refused. A `choice` whose choices a route
    /// offers takes whatever is chosen, and the page is told the route.
    #[tokio::test]
    async fn a_map_is_written_as_lines_and_stored_as_lists() {
        let host = plugin_host();
        let mut manifest = crate::testing::manifest("kb", "1.0.0");
        manifest.settings = vec![
            Setting::new("projects", "Projects", SettingKind::Map)
                .choices_from("settings/projects"),
            Setting::new("board", "Board", SettingKind::Choice).choices_from("settings/boards"),
        ];
        host.register(manifest).await;
        let current = |body: &Value, key: &str| {
            body["current"].as_array().and_then(|current| {
                current.iter().find(|held| held["key"] == key).map(|held| held["value"].clone())
            })
        };

        let (_, page, _) = get_as(&host.app, "/api/v1/plugins/kb/settings", ADMIN).await;
        assert_eq!(page["settings"][0]["choices"], "settings/projects", "{page}");
        assert_eq!(current(&page, "projects"), Some(json!({})), "an empty map to start with");

        let path = "/api/v1/plugins/kb/settings";
        let written = "PAY = card-gateway, ledger\nLEDG\nPAY=ledger,refunds; OPS=pager";
        let (status, body, _) = put_json(
            &host.app,
            path,
            ADMIN,
            json!({ "values": { "projects": written, "board": "B-7" } }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (_, page, _) = get_as(&host.app, path, ADMIN).await;
        assert_eq!(
            current(&page, "projects"),
            Some(
                json!({ "LEDG": [], "OPS": ["pager"], "PAY": ["card-gateway", "ledger", "refunds"] })
            ),
            "a key written twice is one entry, each value once"
        );
        assert_eq!(current(&page, "board"), Some(json!("B-7")));

        let object = json!({ "PAY": ["ledger"], "OPS": "pager, on-call" });
        let (status, body, _) =
            put_json(&host.app, path, ADMIN, json!({ "values": { "projects": object } })).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (_, page, _) = get_as(&host.app, path, ADMIN).await;
        assert_eq!(
            current(&page, "projects"),
            Some(json!({ "OPS": ["pager", "on-call"], "PAY": ["ledger"] }))
        );

        let (status, body, _) =
            put_json(&host.app, path, ADMIN, json!({ "values": { "projects": "=ledger" } })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(
            body["problems"]["projects"]
                .as_str()
                .is_some_and(|said| said.contains("given to nothing"))
        );
    }

    /// Configuring a plugin is its own permission: writing to a plugin does not open its settings,
    /// and holding its settings permission does not need access to the plugin at all.
    #[tokio::test]
    async fn only_the_settings_permission_opens_the_settings() {
        let host = plugin_host();
        host.register(configurable()).await;

        let writer = host.user_holding("wanda", &["plugin:kb:user:rw"]).await;
        let (status, body, _) = get_as(&host.app, "/api/v1/plugins/kb/settings", &writer).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(body["detail"], "needs plugin:kb:settings:ro");

        let reader = host.user_holding("rhea", &["plugin:kb:settings:ro"]).await;
        let (status, body, _) = get_as(&host.app, "/api/v1/plugins/kb/settings", &reader).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["writes"], false, "reading is not changing");

        let (status, body, _) = put_json(
            &host.app,
            "/api/v1/plugins/kb/settings",
            &reader,
            json!({ "values": { "base-url": "https://kb.example" } }),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(body["detail"], "needs plugin:kb:settings:rw");
    }

    /// The plugin gets the last word on what it is being configured with, and a refusal against a
    /// field puts the message on that field with nothing stored.
    #[tokio::test]
    async fn the_plugin_may_refuse_settings_before_they_are_stored() {
        let host = plugin_host();
        host.register(configurable()).await;
        host.plugin.judge_settings_with(Some(doc_plugin_protocol::calls::SettingsVerdict::wrong(
            "token",
            "GitHub refused this token",
        )));

        let (status, body, _) = put_json(
            &host.app,
            "/api/v1/plugins/kb/settings",
            ADMIN,
            json!({ "values": { "base-url": "https://kb.example", "token": "wrong" } }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["problems"]["token"], "GitHub refused this token");

        let (_, body, _) = get_as(&host.app, "/api/v1/plugins/kb/settings", ADMIN).await;
        assert_eq!(body["missing"], json!(["base-url"]), "nothing at all was stored");
        assert_eq!(host.plugin.settings_told().len(), 0, "and the plugin was never told so");
    }

    /// A feature is a switch, and turning one off takes its schedules with it.
    #[tokio::test]
    async fn features_are_switched_and_their_schedules_follow() {
        let host = plugin_host();
        let mut manifest = configurable();
        manifest.schedules =
            vec![doc_plugin_protocol::Schedule::new("nightly", "0 2 * * *", "").of_feature("sync")];
        host.register(manifest).await;

        assert!(host.schedules().await.is_empty(), "the feature is off, so nothing is scheduled");

        let (status, body, _) = put_json(
            &host.app,
            "/api/v1/plugins/kb/features",
            ADMIN,
            json!({ "features": { "sync": true } }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(host.schedules().await, vec!["plugin.kb.nightly".to_string()]);

        let (status, body, _) = put_json(
            &host.app,
            "/api/v1/plugins/kb/features",
            ADMIN,
            json!({ "features": { "sync": false } }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(host.schedules().await.is_empty(), "off again, and the schedule with it");

        let (status, body, _) = put_json(
            &host.app,
            "/api/v1/plugins/kb/features",
            ADMIN,
            json!({ "features": { "nosuch": true } }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }

    /// **Enable** turns a plugin on with the features of other plugins it works from: only where
    /// one of them runs and is configured, never a feature that carries a warning, and only for
    /// somebody who may change every plugin it touches.
    #[tokio::test]
    async fn enabling_turns_on_what_a_feature_works_from() {
        let host = plugin_host();
        let mut dora = crate::testing::manifest("dora", "1.0.0");
        dora.features = vec![
            Feature::new("metrics", "Metrics", "").needs(&["github", "gitlab"], "delivery-data"),
            Feature::new("ranking", "Ranking", "")
                .needs(&["github"], "delivery-data")
                .warning("Not for judging people"),
        ];
        host.register(dora).await;
        let offered = |body: &Value| {
            body["plugins"]
                .as_array()
                .expect("plugins")
                .iter()
                .find(|plugin| plugin["id"] == "dora")
                .map(|plugin| plugin["enable"].clone())
                .expect("dora is listed")
        };

        let (_, body, _) = get_as(&host.app, "/api/v1/plugins", ADMIN).await;
        assert!(offered(&body)["blocked"].is_string(), "nothing can meet the need: {body}");
        let (status, body, _) =
            post_json(&host.app, "/api/v1/plugins/dora/enable", Some(ADMIN), json!({})).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");

        // One source runs but is not configured, the other runs and is.
        let mut gitlab = crate::testing::manifest("gitlab", "1.0.0");
        gitlab.settings = vec![Setting::text("base-url", "Where it is").required()];
        gitlab.features = vec![Feature::new("delivery-data", "Delivery data", "")];
        host.register(gitlab).await;
        let mut github = crate::testing::manifest("github", "1.0.0");
        github.features = vec![Feature::new("delivery-data", "Delivery data", "")];
        host.register(github).await;

        let (_, body, _) = get_as(&host.app, "/api/v1/plugins", ADMIN).await;
        let offer = offered(&body);
        assert_eq!(offer["features"], json!(["metrics"]), "a warned feature is never offered");
        assert_eq!(offer["with"], json!([{ "plugin": "github", "feature": "delivery-data" }]));
        assert_eq!(offer["blocked"], Value::Null);

        let dana = host.user_holding("dana", &["plugin:dora:settings:rw"]).await;
        let (status, body, _) =
            post_json(&host.app, "/api/v1/plugins/dora/enable", Some(&dana), json!({})).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(body["detail"], "needs plugin:github:settings:rw");
        let (_, body, _) = get_as(&host.app, "/api/v1/plugins/dora/settings", ADMIN).await;
        assert_eq!(body["features"][0]["enabled"], false, "nothing was switched");

        let (status, body, _) =
            post_json(&host.app, "/api/v1/plugins/dora/enable", Some(ADMIN), json!({})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["changed"], json!(["github:delivery-data", "dora:metrics"]));
        let (_, body, _) = get_as(&host.app, "/api/v1/plugins/dora/settings", ADMIN).await;
        assert_eq!(body["features"][0]["enabled"], true);
        assert_eq!(body["features"][1]["enabled"], false, "ranking stays off");
        let (_, body, _) = get_as(&host.app, "/api/v1/plugins/gitlab/settings", ADMIN).await;
        assert_eq!(body["features"][0]["enabled"], false, "an unconfigured source is left alone");

        let (_, body, _) = get_as(&host.app, "/api/v1/plugins", ADMIN).await;
        assert_eq!(offered(&body), Value::Null, "nothing is left to enable");
    }

    /// A setting the deployment sets in the environment is where that setting *starts*: the page
    /// shows the value, names the variable, and what is saved here wins from then on — until it
    /// is cleared, when the variable applies again. A plugin of its own, so the variable this
    /// sets cannot reach the other tests running beside it.
    #[tokio::test]
    async fn what_is_set_here_wins_over_the_environment_and_falls_back_to_it() {
        let host = plugin_host();
        let mut manifest = configurable();
        manifest.id = "envkb".into();
        host.register(manifest).await;
        settings::set_environment_for_test("DOC_ENVKB_BASE_URL", Some("https://from.env"));

        let base = |body: &Value| {
            body["current"]
                .as_array()
                .expect("settings")
                .iter()
                .find(|current| current["key"] == "base-url")
                .cloned()
                .expect("the setting")
        };

        let (_, body, _) = get_as(&host.app, "/api/v1/plugins/envkb/settings", ADMIN).await;
        let current = base(&body);
        assert_eq!(current["source"], "environment");
        assert_eq!(current["value"], "https://from.env", "the field shows what is in use");
        assert_eq!(current["environment"], "DOC_ENVKB_BASE_URL", "and says where it came from");
        assert_eq!(body["missing"], json!([]), "the environment has set it");

        let (status, body, _) = put_json(
            &host.app,
            "/api/v1/plugins/envkb/settings",
            ADMIN,
            json!({ "values": { "base-url": "https://set.here" } }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (_, body, _) = get_as(&host.app, "/api/v1/plugins/envkb/settings", ADMIN).await;
        let current = base(&body);
        assert_eq!(current["source"], "stored", "what is set here wins");
        assert_eq!(current["value"], "https://set.here");
        assert_eq!(
            current["environment"], "DOC_ENVKB_BASE_URL",
            "the variable is still named, as what clearing falls back to"
        );

        // Cleared here, the deployment's own value applies again.
        let (status, body, _) = put_json(
            &host.app,
            "/api/v1/plugins/envkb/settings",
            ADMIN,
            json!({ "values": { "base-url": null } }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (_, body, _) = get_as(&host.app, "/api/v1/plugins/envkb/settings", ADMIN).await;
        let current = base(&body);
        assert_eq!(current["source"], "environment");
        assert_eq!(current["value"], "https://from.env");

        settings::set_environment_for_test("DOC_ENVKB_BASE_URL", None);
    }
}
