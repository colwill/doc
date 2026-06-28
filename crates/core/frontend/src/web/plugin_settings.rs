//! A plugin's Settings, Features and Permissions tabs (ADR-0007). The form is built from what the
//! plugin declares, so no page here knows anything about any particular plugin, and every check is
//! the backend's: a tab somebody may not see is left out of the page *and* refused on its own
//! route, so hiding a link is never what keeps anybody out.

use std::collections::{BTreeMap, BTreeSet};

use askama::Template;
use axum::extract::{Extension, Form, Path, Query, State};
use axum::response::Html;
use serde_json::{Map, Value};

use super::AppState;
use super::csrf::Csrf;
use super::error::WebError;
use super::pages::Chrome;
use crate::backend::{
    AccessRequestView, BackendError, FeatureView, NamedSecret, NamedSecrets, PluginPermission,
    PluginSettings, SettingChoice, SettingChoices, SettingSchema, SettingValue, StoreOffer,
};
use crate::session::Signed;

/// One field on the Settings tab: what the plugin declared, what it is set to now, and what its
/// choices route offered when the page was drawn.
pub struct Field {
    pub schema: SettingSchema,
    pub current: SettingValue,
    pub offered: Option<SettingChoices>,
    /// Why the choices could not be read, so the field is typed into instead.
    pub unoffered: Option<String>,
}

/// One row of a `map` on the page: a key and the values ticked for it, or a blank row to add one.
pub struct MapRow {
    pub key: String,
    pub values: Vec<String>,
}

impl MapRow {
    pub fn has(&self, value: &str) -> bool {
        self.values.iter().any(|held| held == value)
    }
}

/// Blank rows under a `map`'s own, for adding without anything but the form.
const BLANK_ROWS: usize = 3;

impl Field {
    pub fn label(&self) -> String {
        match self.schema.label.is_empty() {
            true => self.schema.key.clone(),
            false => self.schema.label.clone(),
        }
    }

    pub fn id(&self) -> String {
        format!("setting-{}", self.schema.key)
    }

    pub fn name(&self) -> String {
        format!("setting-{}", self.schema.key)
    }

    pub fn is(&self, kind: &str) -> bool {
        self.schema.kind == kind
    }

    /// The value in use is the deployment's, with nothing set here.
    pub fn from_environment(&self) -> bool {
        self.current.source == "environment"
    }

    /// Set here, over a value the deployment also gives.
    pub fn overrides_environment(&self) -> bool {
        self.current.source == "stored" && self.current.environment.is_some()
    }

    /// What the page says under a field about the deployment's own value, if it has one.
    pub fn environment_note(&self) -> Option<String> {
        let variable = self.current.environment.as_ref()?;
        Some(match self.current.source.as_str() {
            "stored" => format!(
                "Set here, over {variable} from the deployment. Clear it to use that again."
            ),
            _ => format!("From {variable} in the deployment. Saving something here replaces it."),
        })
    }

    /// What goes in the box: text as it is, a list one line each, a map `KEY=one,two` a line, a
    /// number as written.
    pub fn text(&self) -> String {
        shown(&self.current.value)
    }

    /// The choices to show: what the route offered, or the declared `one_of`.
    pub fn options(&self) -> Vec<SettingChoice> {
        match &self.offered {
            Some(offered) => offered.choices.clone(),
            None => self
                .schema
                .one_of
                .iter()
                .map(|choice| SettingChoice {
                    value: choice.clone(),
                    label: choice.clone(),
                    hint: String::new(),
                })
                .collect(),
        }
    }

    /// Whether a choice is set to something no longer offered, kept rather than lost.
    pub fn chose_unoffered(&self) -> bool {
        let chosen = self.text();
        !chosen.is_empty() && !self.options().iter().any(|choice| choice.value == chosen)
    }

    /// A list's lines as they are held.
    fn lines(&self) -> Vec<String> {
        self.current
            .value
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    }

    /// Whether a list's box for this choice is ticked.
    pub fn ticked(&self, value: &str) -> bool {
        self.lines().iter().any(|line| line == value)
    }

    /// What a list holds that is no longer offered: kept ticked, and said so, rather than lost.
    pub fn held_unoffered(&self) -> Vec<String> {
        let offered = self.options();
        self.lines()
            .into_iter()
            .filter(|line| !offered.iter().any(|choice| &choice.value == line))
            .collect()
    }

    /// A map's rows as held, then blank ones to add with.
    pub fn rows(&self) -> Vec<MapRow> {
        let mut rows: Vec<MapRow> = self
            .current
            .value
            .as_object()
            .into_iter()
            .flatten()
            .map(|(key, values)| MapRow {
                key: key.clone(),
                values: values
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect(),
            })
            .collect();
        rows.extend((0..BLANK_ROWS).map(|_| MapRow { key: String::new(), values: Vec::new() }));
        rows
    }

    /// A map's keys to pick from: those offered, then any held that no longer are.
    pub fn keys(&self) -> Vec<SettingChoice> {
        let mut keys = self.options();
        for row in self.rows() {
            if !row.key.is_empty() && !keys.iter().any(|choice| choice.value == row.key) {
                keys.push(SettingChoice {
                    value: row.key.clone(),
                    label: row.key,
                    hint: "not offered now".into(),
                });
            }
        }
        keys
    }

    /// The values any key of a map can be given: those offered, then any held that no longer are.
    pub fn values(&self) -> Vec<SettingChoice> {
        let mut values =
            self.offered.as_ref().map(|offered| offered.values.clone()).unwrap_or_default();
        for row in self.rows() {
            for value in row.values {
                if !values.iter().any(|choice| choice.value == value) {
                    values.push(SettingChoice {
                        label: value.clone(),
                        value,
                        hint: "not offered now".into(),
                    });
                }
            }
        }
        values
    }

    /// What a row's folded list of values says before it is opened, by what they are called.
    pub fn summary(&self, row: &MapRow) -> String {
        let offered = self.values();
        let labels: Vec<String> = row
            .values
            .iter()
            .map(|value| {
                offered
                    .iter()
                    .find(|choice| &choice.value == value)
                    .map_or_else(|| value.clone(), |choice| choice.label.clone())
            })
            .collect();
        match labels.len() {
            0 => "None chosen".to_string(),
            1..=3 => labels.join(", "),
            many => format!("{many} chosen"),
        }
    }

    /// What a map's columns are called.
    pub fn keys_label(&self) -> String {
        self.offered
            .as_ref()
            .map(|offered| offered.choices_label.clone())
            .filter(|label| !label.is_empty())
            .unwrap_or_else(|| "Key".into())
    }

    pub fn values_label(&self) -> String {
        self.offered
            .as_ref()
            .map(|offered| offered.values_label.clone())
            .filter(|label| !label.is_empty())
            .unwrap_or_else(|| "Values".into())
    }

    pub fn checked(&self) -> bool {
        self.current.value.as_bool().unwrap_or_default()
    }

    /// What a secret's row says, since its value is never shown.
    pub fn secret_state(&self) -> String {
        if let Some(pointed) = &self.current.from_store {
            let named = pointed.label.clone().unwrap_or_else(|| "a secret".to_string());
            return match &pointed.problem {
                Some(problem) => {
                    format!("From Secret Storage: {named}, which gives it nothing: {problem}.")
                }
                None => format!("From Secret Storage: {named}."),
            };
        }
        if self.current.unreadable {
            return "Set, but encrypted under a key this platform no longer has. Set it again."
                .to_string();
        }
        if self.from_environment() {
            return "Set by the deployment.".to_string();
        }
        match (self.current.set, &self.current.updated_at, &self.current.updated_by) {
            (false, _, _) => "Not set.".to_string(),
            (true, Some(at), Some(by)) => {
                format!("Set, changed by {by} on {}", super::admin::when(at))
            }
            (true, Some(at), None) => format!("Set, changed on {}", super::admin::when(at)),
            (true, None, _) => "Set.".to_string(),
        }
    }

    /// Whether a choice field is at this option, for the `selected` mark.
    pub fn chose(&self, choice: &str) -> bool {
        self.text() == choice
    }

    /// Whether a secret is held at all, so it can be cleared: typed in, pointed at Secret
    /// Storage, or sealed under a key that is gone.
    pub fn held(&self) -> bool {
        self.current.set || self.current.unreadable || self.current.from_store.is_some()
    }

    /// Whether a secret setting points at this secret in Secret Storage, for the `selected` mark.
    pub fn points_at(&self, secret: &str) -> bool {
        self.current.from_store.as_ref().is_some_and(|pointed| pointed.secret == secret)
    }
}

/// The fields under one heading, in the order the plugin declared them.
pub struct Group {
    pub name: String,
    pub fields: Vec<Field>,
}

/// What a heading says, which for the settings a plugin put under no heading of its own is the
/// one DOC gives them.
fn label_of(group: &str) -> &str {
    match group.is_empty() {
        true => "General",
        false => group,
    }
}

/// What a heading is known by in an address: lowercase, with anything else a single dash.
fn slug(label: &str) -> String {
    let mut id = String::from("group-");
    for letter in label.to_ascii_lowercase().chars() {
        match letter.is_ascii_alphanumeric() {
            true => id.push(letter),
            false if !id.ends_with('-') => id.push('-'),
            false => {}
        }
    }
    id.trim_end_matches('-').to_string()
}

impl Group {
    pub fn label(&self) -> &str {
        label_of(&self.name)
    }

    pub fn id(&self) -> String {
        slug(self.label())
    }
}

/// The two pages a plugin's settings have that are not settings of its own.
const REQUESTS: &str = "requests";
const CREDENTIALS: &str = "credentials";

/// One page of a plugin's settings: a heading it grouped some of them under, the requests other
/// plugins have made to join one, or the credentials it holds by name. A plugin with only one of
/// these shows it on its own; with more than one, each is a page of its own and the rest sit
/// beside it, because a page of every vendor's settings at once is a page nobody can find
/// anything in.
pub struct Section {
    pub id: String,
    pub title: String,
    /// `settings`, `requests` or `credentials`.
    pub kind: &'static str,
    /// The settings this page shows, for a `settings` section.
    pub group: Option<Group>,
}

impl Section {
    pub fn is(&self, kind: &str) -> bool {
        self.kind == kind
    }
}

/// The pages a plugin's settings come to, in the order they are read: what other plugins are
/// waiting on first, then each heading, then the credentials it holds by name.
fn sections(groups: Vec<Group>, requests: usize, named: Option<&NamedSecrets>) -> Vec<Section> {
    let mut sections = Vec::new();
    if requests > 0 {
        sections.push(Section {
            id: REQUESTS.to_string(),
            title: format!("Requests from other plugins ({requests})"),
            kind: REQUESTS,
            group: None,
        });
    }
    for group in groups {
        sections.push(Section {
            id: group.id(),
            title: group.label().to_string(),
            kind: "settings",
            group: Some(group),
        });
    }
    if let Some(named) = named {
        let title = match named.label.is_empty() {
            true => "Credentials".to_string(),
            false => named.label.clone(),
        };
        sections.push(Section {
            id: CREDENTIALS.to_string(),
            title,
            kind: CREDENTIALS,
            group: None,
        });
    }
    sections
}

/// The settings one page drew, which is all a save from it may change: a checkbox nobody was
/// shown must not be stored as off because the form it was not on said nothing about it. An
/// unknown section, or none, means the whole form, which is what a plugin with one page sends.
fn drawn(settings: &PluginSettings, section: Option<&str>) -> Option<BTreeSet<String>> {
    let section = section?;
    let keys: BTreeSet<String> = settings
        .settings
        .iter()
        .filter(|schema| slug(label_of(&schema.group)) == section)
        .map(|schema| schema.key.clone())
        .collect();
    match keys.is_empty() && section != REQUESTS && section != CREDENTIALS {
        true => None,
        false => Some(keys),
    }
}

impl SettingsTab {
    fn when(at: &chrono::DateTime<chrono::Utc>) -> String {
        super::admin::when(at)
    }

    /// The page being shown, or nothing at all where the plugin declares none.
    pub fn here(&self) -> Option<&Section> {
        self.sections.get(self.at)
    }

    pub fn earlier(&self) -> Option<&Section> {
        self.at.checked_sub(1).and_then(|at| self.sections.get(at))
    }

    pub fn later(&self) -> Option<&Section> {
        self.sections.get(self.at + 1)
    }

    /// Whether there is more than one page, which is what decides the shape of the whole tab.
    pub fn paged(&self) -> bool {
        self.sections.len() > 1
    }

    /// Where one of the pages is, which the contents and both ends of the pagination link to.
    pub fn href(&self, section: &Section) -> String {
        format!("/plugins/{}/settings?section={}", self.plugin, section.id)
    }
}

#[derive(Template)]
#[template(path = "plugin_settings.html")]
pub struct SettingsTab {
    pub chrome: Chrome,
    pub plugin: String,
    pub writes: bool,
    pub tabs: super::admin::Tabs,
    /// Every page of this tab, for the contents beside it and the pages either side.
    pub sections: Vec<Section>,
    /// Which of them is being shown.
    pub at: usize,
    pub missing: Vec<String>,
    pub secrets_available: bool,
    /// For a plugin that holds credentials by name: what to call them, and the ones it holds.
    pub named_secrets: Option<NamedSecrets>,
    pub named: Vec<NamedSecret>,
    pub notice: Option<String>,
    pub error: Option<String>,
    /// A problem against one field, which is where the plugin's own refusals land.
    pub problems: BTreeMap<String, String>,
    /// Other plugins asking to join one of this plugin's settings: waiting ones first.
    pub requests: Vec<AccessRequestView>,
    /// The secrets in Secret Storage its secret settings may point at instead (FEAT-SECRETS).
    pub store: Option<StoreOffer>,
}

#[derive(Template)]
#[template(path = "plugin_features.html")]
pub struct FeaturesTab {
    pub chrome: Chrome,
    pub plugin: String,
    pub writes: bool,
    pub tabs: super::admin::Tabs,
    pub features: Vec<FeatureView>,
    pub notice: Option<String>,
    pub error: Option<String>,
}

#[derive(Template)]
#[template(path = "plugin_permissions.html")]
pub struct PermissionsTab {
    pub chrome: Chrome,
    pub plugin: String,
    pub tabs: super::admin::Tabs,
    pub permissions: Vec<PluginPermission>,
}

fn fields(
    settings: &PluginSettings,
    offered: &mut BTreeMap<String, Result<SettingChoices, String>>,
) -> Vec<Field> {
    settings
        .settings
        .iter()
        .map(|schema| {
            let current = settings
                .current
                .iter()
                .find(|current| current.key == schema.key)
                .cloned()
                .unwrap_or_else(|| SettingValue {
                    key: schema.key.clone(),
                    source: "default".into(),
                    value: Value::Null,
                    set: false,
                    unreadable: false,
                    environment: None,
                    updated_at: None,
                    updated_by: None,
                    from_store: None,
                });
            let (offered, unoffered) = match offered.remove(&schema.key) {
                Some(Ok(offered)) => (Some(offered), None),
                Some(Err(why)) => (None, Some(why)),
                None => (None, None),
            };
            Field { schema: schema.clone(), current, offered, unoffered }
        })
        .collect()
}

/// Settings under their declared headings, ungrouped ones first, each group in the order its
/// first setting appears.
fn grouped(fields: Vec<Field>) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    for field in fields {
        let name = field.schema.group.clone();
        match groups.iter_mut().find(|group| group.name == name) {
            Some(group) => group.fields.push(field),
            None => groups.push(Group { name, fields: vec![field] }),
        }
    }
    groups
}

async fn chrome(state: &AppState, signed: &Signed, csrf: &Csrf, plugin: &str) -> Chrome {
    Chrome::new(plugin.to_string(), "/plugins")
        .signed(signed, csrf)
        .with_plugins(state, signed)
        .await
        .about_plugin(plugin)
}

/// What the page says about whatever was just tried: nothing at all, something that went well,
/// a refusal, or a problem against each field the plugin named.
#[derive(Default)]
struct Said {
    notice: Option<String>,
    error: Option<String>,
    problems: BTreeMap<String, String>,
}

impl Said {
    fn well(notice: impl Into<String>) -> Self {
        Self { notice: Some(notice.into()), ..Self::default() }
    }

    fn refused(error: Option<String>, problems: BTreeMap<String, String>) -> Self {
        Self { notice: None, error, problems }
    }
}

async fn settings_page(
    state: &AppState,
    signed: &Signed,
    csrf: &Csrf,
    plugin: &str,
    section: Option<&str>,
    said: Said,
) -> Result<Html<String>, WebError> {
    let settings = state.backend.plugin_settings(signed.token(), plugin).await?;
    let mut offered = offered(state, signed, plugin, &settings).await;
    let requests = {
        let mut requests = settings.requests.clone();
        requests.sort_by_key(|request| request.state != "pending");
        requests
    };
    let sections = sections(
        grouped(fields(&settings, &mut offered)),
        requests.len(),
        settings.named_secrets.as_ref(),
    );
    // A page nobody asked for by name, or one this plugin does not have, is the first one.
    let at = section
        .and_then(|section| sections.iter().position(|page| page.id == section))
        .unwrap_or_default();
    Ok(Html(
        SettingsTab {
            chrome: chrome(state, signed, csrf, plugin).await,
            plugin: plugin.to_string(),
            writes: settings.writes,
            tabs: super::admin::tabs_from(
                true,
                Some(&settings),
                super::admin::writes_core(state, signed).await,
            ),
            sections,
            at,
            missing: settings.missing.clone(),
            secrets_available: settings.secrets_available,
            named_secrets: settings.named_secrets.clone(),
            named: settings.named.clone(),
            notice: said.notice,
            error: said.error,
            problems: said.problems,
            requests,
            store: settings.store.clone(),
        }
        .render()?,
    ))
}

/// What each setting with a choices route offers, asked of the plugin as whoever is looking; a
/// route that cannot be asked leaves its field to be typed into, and says why.
async fn offered(
    state: &AppState,
    signed: &Signed,
    plugin: &str,
    settings: &PluginSettings,
) -> BTreeMap<String, Result<SettingChoices, String>> {
    let mut offered = BTreeMap::new();
    for schema in &settings.settings {
        let Some(route) = &schema.choices else { continue };
        let asked = match state.backend.plugin_get(signed.token(), plugin, route).await {
            Ok(answer) => serde_json::from_value::<SettingChoices>(answer)
                .map_err(|err| format!("{plugin} answered with something else ({err})")),
            Err(err) => Err(err.detail()),
        };
        offered.insert(schema.key.clone(), asked);
    }
    offered
}

/// **Approve** or **Deny** on a request another plugin made: answered with the page again.
pub async fn decide(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path((id, request, verdict)): Path<(String, String, String)>,
) -> Result<Html<String>, WebError> {
    let decided =
        state.backend.decide_access_request(signed.token(), &id, &request, &verdict).await;
    let (notice, error) = match decided {
        Ok(decided) => {
            let requester = decided["requester"].as_str().unwrap_or("The plugin").to_string();
            let said = match decided["state"].as_str() {
                Some("approved") => format!("{requester} was added. It carries on by itself."),
                _ => format!("{requester}'s request was denied. It will not be asked again."),
            };
            (Some(said), None)
        }
        Err(err) if actionable(&err) => (None, Some(err.detail())),
        Err(err) => return Err(err.into()),
    };
    let said = Said { notice, error, problems: BTreeMap::new() };
    settings_page(&state, &signed, &csrf, &id, Some(REQUESTS), said).await
}

/// Which page of the tab to show, by the id its contents entry links to.
#[derive(serde::Deserialize)]
pub struct Which {
    #[serde(default)]
    section: Option<String>,
}

pub async fn show(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<String>,
    Query(which): Query<Which>,
) -> Result<Html<String>, WebError> {
    settings_page(&state, &signed, &csrf, &id, which.section.as_deref(), Said::default()).await
}

/// What the form said, turned into the values the API takes. A secret left empty is left alone —
/// clearing one is asked for on purpose — and everything else is sent as typed for the backend to
/// check against what the plugin declared.
fn wanted(
    settings: &PluginSettings,
    form: &BTreeMap<String, String>,
    drawn: Option<&BTreeSet<String>>,
) -> Map<String, Value> {
    let mut values = Map::new();
    for schema in &settings.settings {
        // Only what the page that sent this actually showed; the rest is somebody else's page.
        if drawn.is_some_and(|keys| !keys.contains(&schema.key)) {
            continue;
        }
        let current = settings.current.iter().find(|current| current.key == schema.key);
        let from_environment = current.is_some_and(|current| current.source == "environment");
        // A map drawn as rows, or a list drawn as boxes, says it was, so nothing ticked is an
        // empty value rather than nothing sent.
        let drawn = match schema.kind.as_str() {
            "map" if form.contains_key(&format!("map-{}", schema.key)) => {
                Some(map_rows(&schema.key, form))
            }
            "list" if form.contains_key(&format!("list-{}", schema.key)) => Some(
                form.get(&format!("setting-{}", schema.key))
                    .map(|ticked| {
                        ticked
                            .lines()
                            .filter(|line| !line.trim().is_empty())
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default(),
            ),
            _ => None,
        };
        let typed = drawn.as_ref().or_else(|| form.get(&format!("setting-{}", schema.key)));
        let clearing = form.contains_key(&format!("clear-{}", schema.key));
        match schema.kind.as_str() {
            // A checkbox says nothing at all when it is off.
            "boolean" => {
                let on = typed.is_some_and(|value| value != "false" && value != "off");
                // A box left unticked on a setting the deployment turns on is a deliberate `false`
                // stored over it, which is the point of being able to override one.
                values.insert(schema.key.clone(), Value::Bool(on));
            }
            // What is typed wins; otherwise a secret chosen in Secret Storage, when it is not
            // the one already pointed at.
            "secret" => {
                let chosen = form
                    .get(&format!("store-{}", schema.key))
                    .map(|chosen| chosen.trim())
                    .filter(|chosen| !chosen.is_empty())
                    .filter(|chosen| {
                        current
                            .and_then(|current| current.from_store.as_ref())
                            .is_none_or(|pointed| pointed.secret != *chosen)
                    });
                match (clearing, typed.map(String::as_str), chosen) {
                    (true, _, _) => {
                        values.insert(schema.key.clone(), Value::Null);
                    }
                    (false, Some(secret), _) if !secret.is_empty() => {
                        values.insert(schema.key.clone(), Value::String(secret.to_string()));
                    }
                    (false, _, Some(chosen)) => {
                        values.insert(schema.key.clone(), serde_json::json!({ "secret": chosen }));
                    }
                    _ => {}
                }
            }
            _ => match typed {
                // Typed as it stands: unchanged from what the deployment gave is not worth
                // storing, since an override is a decision and should read as one.
                Some(typed)
                    if from_environment
                        && current.is_some_and(|current| same(&current.value, typed)) => {}
                Some(typed) => {
                    values.insert(schema.key.clone(), Value::String(typed.clone()));
                }
                None => {}
            },
        }
    }
    values
}

/// What a form asks of the credentials held by name: the ones typed or cleared, and the ones
/// pointed at a secret in Secret Storage.
type NamedWanted = (BTreeMap<String, Option<String>>, BTreeMap<String, String>);

/// The credentials a form adds or takes away by name: `named-<name>` sets one, and the tick
/// `clear-named-<name>` takes one away. A new one comes as `new-credential-name` and a value,
/// typed in `new-credential-value` or chosen from Secret Storage in `new-credential-secret`,
/// which comes back in the second map. The name is always somebody's to give, whichever way the
/// value comes, so half of a new one is a problem against `new-credential` rather than nothing.
fn wanted_named(
    settings: &PluginSettings,
    form: &BTreeMap<String, String>,
) -> Result<NamedWanted, BTreeMap<String, String>> {
    let mut wanted = BTreeMap::new();
    let mut from_store = BTreeMap::new();
    if settings.named_secrets.is_none() {
        return Ok((wanted, from_store));
    }
    for held in &settings.named {
        if form.contains_key(&format!("clear-named-{}", held.name)) {
            wanted.insert(held.name.clone(), None);
            continue;
        }
        if let Some(value) = form.get(&format!("named-{}", held.name)).filter(|v| !v.is_empty()) {
            wanted.insert(held.name.clone(), Some(value.clone()));
        }
    }
    let named = form.get("new-credential-name").map(|name| name.trim().to_ascii_lowercase());
    let named = named.filter(|name| !name.is_empty());
    let value = form.get("new-credential-value").filter(|value| !value.is_empty());
    let secret = form.get("new-credential-secret").filter(|secret| !secret.trim().is_empty());
    let problem = match (named, value, secret) {
        (None, None, None) => None,
        (Some(name), Some(value), None) => {
            wanted.insert(name, Some(value.clone()));
            None
        }
        (Some(name), None, Some(secret)) => {
            from_store.insert(name, secret.trim().to_string());
            None
        }
        (None, _, _) => Some("give it a name as well as its value"),
        (Some(_), None, None) => Some("type its value or choose a secret from Secret Storage"),
        (Some(_), Some(_), Some(_)) => {
            Some("type its value or choose a secret from Secret Storage, not both")
        }
    };
    match problem {
        None => Ok((wanted, from_store)),
        Some(problem) => Err(BTreeMap::from([("new-credential".to_string(), problem.to_string())])),
    }
}

fn lines(items: &Value) -> Vec<&str> {
    items.as_array().into_iter().flatten().filter_map(Value::as_str).collect()
}

/// A value as a box shows it and a form sends it back.
fn shown(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        Value::Array(_) => lines(value).join("\n"),
        Value::Object(entries) => entries
            .iter()
            .map(|(key, values)| match lines(values).is_empty() {
                true => key.clone(),
                false => format!("{key}={}", lines(values).join(",")),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Bool(on) => on.to_string(),
        Value::Null => String::new(),
    }
}

/// Whether what was typed is what is already in use, so a save leaves it as the deployment's.
fn same(value: &Value, typed: &str) -> bool {
    !matches!(value, Value::Bool(_) | Value::Null) && shown(value) == typed.trim()
}

/// A form's fields, a name sent more than once — ticked boxes — as its values a line each.
fn joined(form: Vec<(String, String)>) -> BTreeMap<String, String> {
    let mut joined: BTreeMap<String, String> = BTreeMap::new();
    for (name, value) in form {
        joined
            .entry(name)
            .and_modify(|held| {
                held.push('\n');
                held.push_str(&value);
            })
            .or_insert(value);
    }
    joined
}

/// A map drawn as rows, `map-<key>-<row>-key` and its ticked `map-<key>-<row>-value`s, written
/// back as the `KEY=one,two` lines the backend takes; a row with no key is left out.
fn map_rows(key: &str, form: &BTreeMap<String, String>) -> String {
    let mut lines = Vec::new();
    for row in 0.. {
        let Some(chosen) = form.get(&format!("map-{key}-{row}-key")) else { break };
        let chosen = chosen.trim();
        if chosen.is_empty() {
            continue;
        }
        let values: Vec<&str> = form
            .get(&format!("map-{key}-{row}-value"))
            .map(|values| values.lines().map(str::trim).filter(|value| !value.is_empty()).collect())
            .unwrap_or_default();
        lines.push(format!("{chosen}={}", values.join(",")));
    }
    lines.join("\n")
}

/// The problems the backend put against each field, so each lands on its own box.
fn problems_of(err: &BackendError) -> BTreeMap<String, String> {
    err.extension("problems")
        .and_then(|problems| serde_json::from_value(problems).ok())
        .unwrap_or_default()
}

pub async fn save(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<String>,
    Form(form): Form<Vec<(String, String)>>,
) -> Result<Html<String>, WebError> {
    let form = joined(form);
    let settings = state.backend.plugin_settings(signed.token(), &id).await?;
    let section = form.get("section").map(String::as_str);
    let drawn = drawn(&settings, section);
    let values = wanted(&settings, &form, drawn.as_ref());
    let (named, from_store) = match wanted_named(&settings, &form) {
        Ok(wanted) => wanted,
        Err(problems) => {
            let said = Said::refused(None, problems);
            return settings_page(&state, &signed, &csrf, &id, section, said).await;
        }
    };
    let saving =
        state.backend.save_plugin_settings(signed.token(), &id, &values, &named, &from_store);
    match saving.await {
        Ok(saved) => {
            let changed = saved["changed"].as_array().map(Vec::len).unwrap_or_default();
            let said = saved["message"].as_str().map(str::to_string);
            let notice = match (changed, said) {
                (0, _) => "Nothing was changed.".to_string(),
                (_, Some(said)) => format!("Saved. {said}"),
                (1, None) => "Saved one setting.".to_string(),
                (many, None) => format!("Saved {many} settings."),
            };
            settings_page(&state, &signed, &csrf, &id, section, Said::well(notice)).await
        }
        Err(err) if actionable(&err) => {
            let problems = problems_of(&err);
            let error = match problems.is_empty() {
                true => Some(err.detail()),
                false => None,
            };
            settings_page(&state, &signed, &csrf, &id, section, Said::refused(error, problems))
                .await
        }
        Err(err) => Err(err.into()),
    }
}

/// **Test connection**: asks the plugin what it thinks of what is in the boxes, storing nothing.
pub async fn test(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<String>,
    Form(form): Form<Vec<(String, String)>>,
) -> Result<Html<String>, WebError> {
    let form = joined(form);
    let settings = state.backend.plugin_settings(signed.token(), &id).await?;
    let section = form.get("section").map(String::as_str);
    let values = wanted(&settings, &form, drawn(&settings, section).as_ref());
    match state.backend.check_plugin_settings(signed.token(), &id, &values).await {
        Ok(verdict) => {
            let ok = verdict["ok"].as_bool().unwrap_or_default();
            let said = verdict["message"].as_str().unwrap_or("These settings are usable.");
            let problems: BTreeMap<String, String> =
                serde_json::from_value(verdict["problems"].clone()).unwrap_or_default();
            let error = verdict["problem"].as_str().map(str::to_string);
            let notice = ok.then(|| said.to_string());
            let said = Said { notice, error, problems };
            settings_page(&state, &signed, &csrf, &id, section, said).await
        }
        Err(err) if actionable(&err) => {
            let problems = problems_of(&err);
            let error = problems.is_empty().then(|| err.detail());
            settings_page(&state, &signed, &csrf, &id, section, Said::refused(error, problems))
                .await
        }
        Err(err) => Err(err.into()),
    }
}

async fn features_page(
    state: &AppState,
    signed: &Signed,
    csrf: &Csrf,
    plugin: &str,
    notice: Option<String>,
    error: Option<String>,
) -> Result<Html<String>, WebError> {
    let settings = state.backend.plugin_settings(signed.token(), plugin).await?;
    Ok(Html(
        FeaturesTab {
            chrome: chrome(state, signed, csrf, plugin).await,
            plugin: plugin.to_string(),
            writes: settings.writes,
            tabs: super::admin::tabs_from(
                true,
                Some(&settings),
                super::admin::writes_core(state, signed).await,
            ),
            features: settings.features.clone(),
            notice,
            error,
        }
        .render()?,
    ))
}

pub async fn features(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<String>,
) -> Result<Html<String>, WebError> {
    features_page(&state, &signed, &csrf, &id, None, None).await
}

pub async fn set_features(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<String>,
    Form(form): Form<BTreeMap<String, String>>,
) -> Result<Html<String>, WebError> {
    let settings = state.backend.plugin_settings(signed.token(), &id).await?;
    let wanted: BTreeMap<String, bool> = settings
        .features
        .iter()
        .map(|feature| {
            (feature.name.clone(), form.contains_key(&format!("feature-{}", feature.name)))
        })
        .collect();
    match state.backend.set_plugin_features(signed.token(), &id, &wanted).await {
        Ok(saved) => {
            let changed = saved["changed"].as_array().map(Vec::len).unwrap_or_default();
            let notice = match changed {
                0 => "Nothing was changed.".to_string(),
                1 => "One feature was switched.".to_string(),
                many => format!("{many} features were switched."),
            };
            features_page(&state, &signed, &csrf, &id, Some(notice), None).await
        }
        Err(err) if actionable(&err) => {
            features_page(&state, &signed, &csrf, &id, None, Some(err.detail())).await
        }
        Err(err) => Err(err.into()),
    }
}

pub async fn permissions(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<String>,
) -> Result<Html<String>, WebError> {
    let held = state.backend.plugin_permission_holders(signed.token(), &id).await?;
    Ok(Html(
        PermissionsTab {
            chrome: chrome(&state, &signed, &csrf, &id).await,
            tabs: super::admin::tabs(&state, &signed, &id).await,
            plugin: id,
            permissions: held.permissions,
        }
        .render()?,
    ))
}

fn actionable(err: &BackendError) -> bool {
    matches!(err.status(), Some(400 | 403 | 404 | 409 | 500))
}
