//! What this plugin is configured with, declared to the platform and read back from it
//! (ADR-0007): the OAuth app, what the sync reads organisations with, which DOC organisation its
//! teams are in, and where GitHub is. Every one of them is a field on the plugin's **Settings**
//! page, and every one of them is still seeded by the variable that used to be its only source —
//! `DOC_GITHUB_*` for `github`, `DOC_GHE_*` for `ghe` — so a deployment that sets them keeps
//! working and can now be changed from DOC. Secrets may come from files, as Docker secrets do.
//! Each part is optional: with none of them the plugin still hands out archive links for public
//! repositories. Delivery and pipeline data read with the sync's token or App, so they need one too.

use base64::Engine;
use doc_plugin_sdk::protocol::Secret;
use doc_plugin_sdk::{Backend, Feature, Setting, SettingKind};
use serde_json::json;
use url::Url;

const SCOPES: &str = "read:user user:email read:org";
const PUBLIC_URL: &str = "http://127.0.0.1:8081";
pub const SYNC_SCHEDULE: &str = "*/30 * * * *";
/// The plugins given archive links unless a deployment says otherwise: the Knowledge Base, which
/// imports a repository's documentation, Software Templates, which reads a skeleton from one, and
/// Repository Insights, which scans one with ccc.
const ARCHIVE_PLUGINS: [&str; 4] = ["kb", "templates", "insights", "eol"];

/// Signing people in, and keeping organisations and teams in step: each off until somebody turns
/// it on, since a platform may want the other, or neither.
pub const SIGN_IN: &str = "sign-in";
pub const SYNC: &str = "sync";
/// Deployments, deployment workflow runs and merged pull requests, for DORA (ADR-0007 §2).
pub const DELIVERY: &str = "delivery-data";
pub const DELIVERY_SCHEDULE: &str = "23 * * * *";
/// Every GitHub Actions workflow run, for CI/CD/CT metrics.
pub const PIPELINES: &str = "pipeline-data";
pub const PIPELINE_SCHEDULE: &str = "53 * * * *";

/// The setting keys, which are also what the environment names: `orgs` is `DOC_GITHUB_ORGS`.
pub const URL: &str = "url";
pub const ORGS: &str = "orgs";
pub const CLIENT_ID: &str = "client-id";
pub const CLIENT_SECRET: &str = "client-secret";
pub const SCOPES_KEY: &str = "scopes";
pub const TOKEN: &str = "token";
pub const APP_ID: &str = "app-id";
pub const APP_KEY: &str = "app-key";
pub const SYNC_SCHEDULE_KEY: &str = "sync-schedule";
pub const DOC_ORGANISATION: &str = "doc-organisation";
pub const ARCHIVE_PLUGINS_KEY: &str = "archive-plugins";
pub const DELIVERY_REPOSITORIES: &str = "delivery-repositories";
pub const DELIVERY_DAYS: &str = "delivery-days";
pub const DEPLOY_WORKFLOWS: &str = "deploy-workflows";
pub const PRODUCTION: &str = "production-environments";
pub const WEBHOOK_SECRET: &str = "webhook-secret";
pub const DELIVERY_SCHEDULE_KEY: &str = "delivery-schedule";
pub const PIPELINE_DAYS: &str = "pipeline-days";
pub const PIPELINE_SCHEDULE_KEY: &str = "pipeline-schedule";

/// What the platform builds this plugin's Settings page from. `enterprise` is the `ghe` build,
/// which has no default URL: there is no one GitHub Enterprise to guess at.
pub fn declared(enterprise: bool) -> Vec<Setting> {
    let where_it_is = match enterprise {
        true => Setting::new(URL, "GitHub Enterprise URL", SettingKind::Url)
            .hinted("Where your GitHub Enterprise Server is, such as https://github.acme.example.")
            .required(),
        false => Setting::new(URL, "GitHub URL", SettingKind::Url)
            .hinted("Where GitHub is. Leave it at github.com unless you are being proxied.")
            .defaulting(json!("https://github.com")),
    };
    vec![
        where_it_is.grouped("Connection"),
        Setting::new(ORGS, "GitHub organisations", SettingKind::List)
            .hinted(
                "Whose members may sign in, and whose teams and repositories are synced. \
                 One to a line, or separated by commas.",
            )
            .grouped("Connection"),
        Setting::text(CLIENT_ID, "OAuth client ID")
            .hinted("From the OAuth app you registered for DOC.")
            .grouped("Sign-in")
            .of_feature(SIGN_IN),
        Setting::secret(CLIENT_SECRET, "OAuth client secret")
            .grouped("Sign-in")
            .of_feature(SIGN_IN),
        Setting::text(SCOPES_KEY, "Scopes")
            .hinted("What DOC asks GitHub for when somebody signs in.")
            .defaulting(json!(SCOPES))
            .grouped("Sign-in")
            .of_feature(SIGN_IN),
        Setting::secret(TOKEN, "Access token")
            .hinted("What the sync reads organisations with, unless a GitHub App is used instead.")
            .grouped("Sync")
            .of_feature(SYNC),
        Setting::text(APP_ID, "GitHub App ID")
            .hinted("Use a GitHub App instead of a token: its ID, and its private key below.")
            .grouped("Sync")
            .of_feature(SYNC),
        Setting::secret(APP_KEY, "GitHub App private key")
            .hinted("The PEM GitHub gave you when you made the App's key.")
            .grouped("Sync")
            .of_feature(SYNC),
        Setting::new(SYNC_SCHEDULE_KEY, "How often to sync", SettingKind::Cron)
            .hinted("A cron expression in UTC.")
            .defaulting(json!(SYNC_SCHEDULE))
            .grouped("Sync")
            .of_feature(SYNC),
        Setting::text(DOC_ORGANISATION, "DOC organisation")
            .hinted(
                "Which DOC organisation the synced teams belong to. \
                 Empty means the one that signs in with this provider.",
            )
            .grouped("Sync")
            .of_feature(SYNC),
        Setting::new(ARCHIVE_PLUGINS_KEY, "Plugins given archive links", SettingKind::List)
            .hinted(
                "Which plugins may ask this one for a repository archive link. A plugin left out \
                 asks to be added, and whoever may change these settings approves or denies it.",
            )
            .defaulting(json!(ARCHIVE_PLUGINS))
            .requestable()
            .grouped("Archive links"),
        Setting::new(DELIVERY_REPOSITORIES, "Repositories", SettingKind::List)
            .hinted(
                "Whose delivery and pipeline data is read: owner/name for one, topic:<name> for \
                 those with a topic. Empty means every repository in the organisations above.",
            )
            .grouped("Repositories"),
        Setting::secret(WEBHOOK_SECRET, "Webhook secret")
            .hinted(
                "Optional. With it, point an organisation webhook at this plugin's \
                 public/webhooks route for deployment_status, workflow_run and pull_request, and \
                 each repository is read again as soon as something happens in it.",
            )
            .grouped("Repositories"),
        Setting::new(PRODUCTION, "Production environments", SettingKind::List)
            .hinted("Deployments to these environments are the ones that count.")
            .defaulting(json!(["production", "prod"]))
            .grouped("Delivery data")
            .of_feature(DELIVERY),
        Setting::new(DEPLOY_WORKFLOWS, "Workflows that deploy", SettingKind::List)
            .hinted(
                "For repositories that deploy from Actions without GitHub's deployments: a \
                 workflow's name or file, such as deploy.yml, or owner/name:deploy.yml for one \
                 repository. Their successful runs on the default branch count as deployments.",
            )
            .defaulting(json!(["deploy"]))
            .grouped("Delivery data")
            .of_feature(DELIVERY),
        Setting::new(DELIVERY_DAYS, "Days to read back", SettingKind::Number)
            .hinted("How far back the first read goes. Each read after it starts where the last one ended.")
            .defaulting(json!(90))
            .between(1.0, 400.0)
            .grouped("Delivery data")
            .of_feature(DELIVERY),
        Setting::new(DELIVERY_SCHEDULE_KEY, "How often to read", SettingKind::Cron)
            .hinted("A cron expression in UTC. Webhooks make this a safety net rather than the clock.")
            .defaulting(json!(DELIVERY_SCHEDULE))
            .grouped("Delivery data")
            .of_feature(DELIVERY),
        Setting::new(PIPELINE_DAYS, "Days to read back", SettingKind::Number)
            .hinted("How far back the first read of workflow runs goes. Each read after it starts where the last one ended.")
            .defaulting(json!(90))
            .between(1.0, 400.0)
            .grouped("Pipeline data")
            .of_feature(PIPELINES),
        Setting::new(PIPELINE_SCHEDULE_KEY, "How often to read", SettingKind::Cron)
            .hinted("A cron expression in UTC. A webhook for workflow_run reads a repository as soon as a run ends.")
            .defaulting(json!(PIPELINE_SCHEDULE))
            .grouped("Pipeline data")
            .of_feature(PIPELINES),
    ]
}

pub fn features() -> Vec<Feature> {
    vec![
        Feature::new(
            SIGN_IN,
            "Sign in with GitHub",
            "Offers GitHub on the sign-in page, for the members of the organisations above. \
             It needs an OAuth app: a client ID and secret.",
        ),
        Feature::new(
            SYNC,
            "Organisation sync",
            "Brings each organisation's teams and members into the platform, and its \
             repositories into the Catalogue, on the schedule below.",
        ),
        Feature::new(
            DELIVERY,
            "Delivery data",
            "Reads each repository's production deployments, deployment workflow runs and merged \
             pull requests, and the commits each deployment shipped, for DORA metrics. It reads \
             with the sync's token or GitHub App.",
        ),
        Feature::new(
            PIPELINES,
            "Pipeline data",
            "Reads every GitHub Actions workflow run of each repository — how it ended, how long \
             it took and how many attempts it needed — for CI/CD/CT metrics. It reads with the \
             sync's token or GitHub App.",
        ),
    ]
}

/// What the sync reads each organisation with: a token, or a GitHub App installed in it.
#[derive(Debug, Clone)]
pub enum Access {
    Token(Secret<String>),
    App { id: String, key: AppKey },
}

/// The App's private key as DER, which GitHub hands out as PKCS#1 and some tools turn into PKCS#8.
#[derive(Debug, Clone)]
pub struct AppKey {
    pub der: Secret<Vec<u8>>,
    pub pkcs8: bool,
}

impl AppKey {
    pub fn from_pem(pem: &str) -> Result<Self, String> {
        let body: String = pem.lines().filter(|line| !line.starts_with("-----")).collect();
        let der = base64::engine::general_purpose::STANDARD
            .decode(body.trim())
            .map_err(|err| format!("the App's private key is not PEM: {err}"))?;
        Ok(Self { der: Secret::new(der), pkcs8: pem.contains("BEGIN PRIVATE KEY") })
    }
}

/// The OAuth app people sign in with.
#[derive(Debug, Clone)]
pub struct OAuth {
    pub client_id: String,
    pub client_secret: Secret<String>,
    pub scopes: String,
    pub redirect: Url,
}

#[derive(Debug, Clone)]
pub struct Settings {
    /// None turns sign-in off: either the feature is off, or there is no OAuth app to use.
    pub oauth: Option<OAuth>,
    /// Where people sign in, such as `https://github.com/`; both URLs end in `/` so paths join.
    pub web: Url,
    pub api: Url,
    /// Whose members may sign in, and what the sync reads. Empty when sign-in is off.
    pub organisations: Vec<String>,
    /// None leaves the sync unable to run, though people can still sign in.
    pub access: Option<Access>,
    /// The plugins that may ask for archive links.
    pub archive_plugins: Vec<String>,
    /// The DOC organisation the synced teams are in. None means the one that signs in with this
    /// provider, which core works out for itself (T68).
    pub doc_organisation: Option<String>,
    /// Whether an administrator has turned the sync on.
    pub syncs: bool,
    pub delivery: Delivery,
    pub pipelines: Pipelines,
}

/// Which workflow runs are read: every run of the repositories delivery data reads.
#[derive(Debug, Clone)]
pub struct Pipelines {
    pub on: bool,
    pub days: i64,
}

/// What delivery data is read, and how it is told apart from everything else GitHub holds.
#[derive(Debug, Clone)]
pub struct Delivery {
    pub on: bool,
    /// `owner/name` entries and `topic:<name>` entries; empty for every repository. Pipeline data
    /// reads the same ones.
    pub repositories: Vec<String>,
    pub production: Vec<String>,
    pub workflows: Vec<String>,
    pub days: i64,
    pub webhook_secret: Option<Secret<String>>,
}

impl Delivery {
    /// Whether a repository is one whose deployments are read.
    pub fn reads(&self, full_name: &str, topics: &[String]) -> bool {
        self.repositories.is_empty()
            || self.repositories.iter().any(|wanted| match wanted.strip_prefix("topic:") {
                Some(topic) => topics.iter().any(|held| held.eq_ignore_ascii_case(topic.trim())),
                None => wanted.eq_ignore_ascii_case(full_name),
            })
    }

    /// Whether a workflow deploys `repository`: its name, its file or the file without `.yml`,
    /// listed for every repository or for this one.
    pub fn deploys(&self, repository: &str, name: &str, path: &str) -> bool {
        let file = path.rsplit('/').next().unwrap_or(path);
        let stem = file.rsplit_once('.').map_or(file, |(stem, _)| stem);
        self.workflows.iter().any(|entry| {
            let wanted = match entry.split_once(':') {
                Some((only, wanted)) if only.eq_ignore_ascii_case(repository) => wanted,
                Some(_) => return false,
                None => entry.as_str(),
            };
            [name, file, stem].iter().any(|held| held.eq_ignore_ascii_case(wanted.trim()))
        })
    }
}

fn url(name: &str, value: &str) -> Result<Url, String> {
    Url::parse(value).map_err(|err| format!("{name} is not a URL: {err}"))
}

/// The variable a setting is also named by, for a message a person can act on: the page says
/// which one seeds a field, and a refusal should speak the same language.
fn named(plugin: &str, key: &str) -> String {
    format!(
        "{} ({})",
        key,
        format!("DOC_{}_{}", plugin, key).to_ascii_uppercase().replace('-', "_")
    )
}

impl Settings {
    /// Read from what core hands the plugin: what an administrator set on the Settings page, or
    /// the deployment's variable where nothing is set, or the declared default.
    pub fn read(backend: &Backend, plugin: &str, enterprise: bool) -> Result<Self, String> {
        let settings = backend.settings();
        let name = |key: &str| named(plugin, key);

        let web = match (settings.some_text(URL), enterprise) {
            (Some(value), _) => url(&name(URL), &format!("{}/", value.trim_end_matches('/')))?,
            (None, true) => return Err(format!("{} is not set", name(URL))),
            (None, false) => url(&name(URL), "https://github.com/")?,
        };
        let api = match web.host_str() {
            Some("github.com") => url(&name(URL), "https://api.github.com/")?,
            _ => url(&name(URL), &format!("{web}api/v3/"))?,
        };
        let organisations = settings.list(ORGS);

        let signs_in = settings.feature(SIGN_IN);
        let client_id = settings.some_text(CLIENT_ID);
        let client_secret = settings.secret(CLIENT_SECRET);
        let oauth = match (signs_in, client_id, client_secret) {
            (false, _, _) | (true, None, None) => None,
            (true, Some(_), None) => return Err(format!("{} is not set", name(CLIENT_SECRET))),
            (true, None, Some(_)) => return Err(format!("{} is not set", name(CLIENT_ID))),
            (true, Some(client_id), Some(client_secret)) => {
                if organisations.is_empty() {
                    return Err(format!(
                        "{} names no organisation, and sign-in is only for their members",
                        name(ORGS)
                    ));
                }
                let public = std::env::var("DOC_PUBLIC_URL")
                    .ok()
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| PUBLIC_URL.into());
                let redirect = format!("{}/auth/{plugin}/callback", public.trim_end_matches('/'));
                Some(OAuth {
                    client_id,
                    client_secret,
                    scopes: settings.some_text(SCOPES_KEY).unwrap_or_else(|| SCOPES.into()),
                    redirect: url("DOC_PUBLIC_URL", &redirect)?,
                })
            }
        };

        let access = match (settings.some_text(APP_ID), settings.secret(APP_KEY)) {
            (Some(id), Some(pem)) => Some(Access::App { id, key: AppKey::from_pem(pem.expose())? }),
            (Some(_), None) => return Err(format!("{} is not set", name(APP_KEY))),
            (None, _) => settings.secret(TOKEN).map(Access::Token),
        };

        let archive_plugins = match settings.list(ARCHIVE_PLUGINS_KEY) {
            listed if listed.is_empty() => ARCHIVE_PLUGINS.map(str::to_string).to_vec(),
            listed => listed,
        };

        Ok(Self {
            oauth,
            web,
            api,
            organisations,
            access,
            archive_plugins,
            doc_organisation: settings.some_text(DOC_ORGANISATION),
            syncs: settings.feature(SYNC),
            delivery: Delivery {
                on: settings.feature(DELIVERY),
                repositories: settings.list(DELIVERY_REPOSITORIES),
                production: settings.list(PRODUCTION),
                workflows: settings.list(DEPLOY_WORKFLOWS),
                days: settings.integer(DELIVERY_DAYS).unwrap_or(90).clamp(1, 400),
                webhook_secret: settings.secret(WEBHOOK_SECRET),
            },
            pipelines: Pipelines {
                on: settings.feature(PIPELINES),
                days: settings.integer(PIPELINE_DAYS).unwrap_or(90).clamp(1, 400),
            },
        })
    }

    pub fn allows(&self, organisations: &[String]) -> bool {
        organisations
            .iter()
            .any(|org| self.organisations.iter().any(|allowed| allowed.eq_ignore_ascii_case(org)))
    }

    /// What the sync needs before it can run, in words for the plugin's page and its task.
    pub fn sync_problem(&self) -> Option<String> {
        if !self.syncs {
            return Some("the Organisation sync feature is off".into());
        }
        if self.organisations.is_empty() {
            return Some("no GitHub organisation is set".into());
        }
        if self.access.is_none() {
            return Some("there is no access token or GitHub App to read GitHub with".into());
        }
        None
    }

    /// What reading delivery data needs before it can run, in the same words.
    pub fn delivery_problem(&self) -> Option<String> {
        if !self.delivery.on {
            return Some("the Delivery data feature is off".into());
        }
        self.reading_problem()
    }

    /// What reading workflow runs needs before it can run.
    pub fn pipeline_problem(&self) -> Option<String> {
        if !self.pipelines.on {
            return Some("the Pipeline data feature is off".into());
        }
        self.reading_problem()
    }

    fn reading_problem(&self) -> Option<String> {
        if self.organisations.is_empty() {
            return Some("no GitHub organisation is set".into());
        }
        if self.access.is_none() {
            return Some(
                "there is no access token or GitHub App to read repositories with: set one under \
                 Sync, even with the Organisation sync feature off"
                    .into(),
            );
        }
        None
    }
}
