//! Set up DOC: an administrator's first run, a step a page. It asks what the platform is called and
//! which tools its teams use, suggests the plugins to keep on from that, and leads on to where
//! sign-in, each tool's connection and the first data are set up. Core keeps how far it has got.

use std::collections::BTreeSet;

use askama::Template;
use axum::extract::{Extension, Form, Path, Query, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use super::AppState;
use super::categories::{self, Section};
use super::csrf::Csrf;
use super::error::WebError;
use super::pages::Chrome;
use crate::backend::{BackendError, PluginRow, PluginSettings, SettingValue, Settings, Setup};
use crate::session::{self, Signed};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Name,
    Tools,
    Plugins,
    Organisation,
    SignIn,
    Connect,
    Data,
    Finish,
}

pub const STEPS: [Step; 8] = [
    Step::Name,
    Step::Tools,
    Step::Plugins,
    Step::Organisation,
    Step::SignIn,
    Step::Connect,
    Step::Data,
    Step::Finish,
];

impl Step {
    pub fn slug(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Tools => "tools",
            Self::Plugins => "plugins",
            Self::Organisation => "organisation",
            Self::SignIn => "sign-in",
            Self::Connect => "connect",
            Self::Data => "data",
            Self::Finish => "finish",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::Name => "Name this platform",
            Self::Tools => "The tools you use",
            Self::Plugins => "Choose plugins",
            Self::Organisation => "Your organisation",
            Self::SignIn => "How people sign in",
            Self::Connect => "Connect your tools",
            Self::Data => "Bring your data in",
            Self::Finish => "Finish",
        }
    }

    /// What it is for, as the overview lists it.
    pub fn summary(self) -> &'static str {
        match self {
            Self::Name => "What DOC calls itself beside the logo, such as DEV or ACME.",
            Self::Tools => "Which tools your teams already use, so DOC can suggest what fits.",
            Self::Plugins => "Keep on what you will use, and turn off what you will not.",
            Self::Organisation => "Give the organisation your own name, and add teams and people.",
            Self::SignIn => "Let people sign in with the accounts they already have.",
            Self::Connect => "Give DOC what it needs to read each of your tools.",
            Self::Data => "Fill the Catalogue and the Knowledge Base, and see your metrics.",
            Self::Finish => "Check what is done, and start using DOC.",
        }
    }

    fn parse(slug: &str) -> Option<Self> {
        STEPS.into_iter().find(|step| step.slug() == slug)
    }

    pub fn number(self) -> usize {
        STEPS.iter().position(|step| *step == self).unwrap_or(0) + 1
    }

    fn next(self) -> Option<Self> {
        STEPS.get(self.number()).copied()
    }

    pub fn href(self) -> String {
        format!("/setup/{}", self.slug())
    }
}

/// A tool an organisation may already use: what DOC does with it, and the plugins that read it.
pub struct Tool {
    pub id: &'static str,
    pub name: &'static str,
    pub gives: &'static str,
    pub group: &'static str,
    plugins: &'static [&'static str],
}

const TOOL_GROUPS: [&str; 4] =
    ["Code and delivery", "Planning and documentation", "Signing in", "AI"];

const TOOLS: [Tool; 14] = [
    Tool {
        id: "github",
        name: "GitHub",
        gives: "Sign-in, your teams and their members, your repositories in the Catalogue, and the \
                deployments and workflow runs DORA and CI/CD/CT metrics come from.",
        group: "Code and delivery",
        plugins: &["github", "insights", "dora", "cicd", "eol", "templates"],
    },
    Tool {
        id: "ghe",
        name: "GitHub Enterprise Server",
        gives: "The same as GitHub, from a server of your own.",
        group: "Code and delivery",
        plugins: &["ghe", "insights", "dora", "cicd", "eol", "templates"],
    },
    Tool {
        id: "kubernetes",
        name: "Kubernetes",
        gives: "What runs in each cluster, read with a read-only account: deployments for DORA, \
                rollouts for CI/CD/CT, and outages for Reliability.",
        group: "Code and delivery",
        plugins: &["kubernetes", "dora", "cicd", "reliability"],
    },
    Tool {
        id: "cloud",
        name: "Linode, AWS, Google Cloud or Azure",
        gives: "Machines and buckets people ask for themselves, kept in the Catalogue and torn \
                down when they expire.",
        group: "Code and delivery",
        plugins: &["infra"],
    },
    Tool {
        id: "grafana",
        name: "Grafana",
        gives: "Your dashboards, shown on the services they are about.",
        group: "Code and delivery",
        plugins: &["grafana", "secrets"],
    },
    Tool {
        id: "jira",
        name: "Jira Cloud",
        gives: "Each project's releases and their issues, on the delivery roadmap.",
        group: "Planning and documentation",
        plugins: &["jira", "roadmap"],
    },
    Tool {
        id: "jira-dc",
        name: "Jira Data Center or Server",
        gives: "The same as Jira Cloud, from a server of your own.",
        group: "Planning and documentation",
        plugins: &["jira-dc", "roadmap"],
    },
    Tool {
        id: "confluence",
        name: "Confluence",
        gives: "Spaces kept in step in the Knowledge Base, or moved over once by the Data Vacuum.",
        group: "Planning and documentation",
        plugins: &["kb", "vacuum"],
    },
    Tool {
        id: "backstage",
        name: "Backstage",
        gives: "Its catalogue moved into DOC by the Data Vacuum, and its tech radar imported as \
                it is.",
        group: "Planning and documentation",
        plugins: &["vacuum", "radar", "resources"],
    },
    Tool {
        id: "google",
        name: "Google Workspace",
        gives: "Sign-in with Google accounts, and Google Drive folders in the Knowledge Base.",
        group: "Planning and documentation",
        plugins: &["google", "kb"],
    },
    Tool {
        id: "entra",
        name: "Microsoft Entra ID",
        gives: "Sign-in with your Microsoft work accounts.",
        group: "Signing in",
        plugins: &["entra"],
    },
    Tool {
        id: "oidc",
        name: "Okta, Keycloak, Auth0 or another OpenID Connect provider",
        gives: "Sign-in through it.",
        group: "Signing in",
        plugins: &["oidc"],
    },
    Tool {
        id: "akamai",
        name: "Akamai",
        gives: "Sign-in through Akamai's OpenID Connect provider.",
        group: "Signing in",
        plugins: &["akamai"],
    },
    Tool {
        id: "claude",
        name: "Claude, with an Anthropic API key",
        gives: "The Data Vacuum reading your old tools for you, and the jobs Agent Smith runs.",
        group: "AI",
        plugins: &["vacuum", "agent"],
    },
];

fn tool(id: &str) -> Option<&'static Tool> {
    TOOLS.iter().find(|tool| tool.id == id)
}

/// Whether to keep a plugin on, as the plugins step suggests it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Advice {
    /// Core will not turn it off, for the reason given.
    Always(&'static str),
    /// Worth having with nothing outside DOC.
    Suggested,
    /// Worth having with one of the tools that lists it.
    ForTools,
    /// Worth having only when, as this says.
    When(&'static str),
    /// For seeing DOC with made-up data before the tools are connected.
    Trying,
    /// An example for whoever writes plugins.
    Example,
}

/// What the setup says of a plugin it knows: its name, what it does, and whether to keep it on.
struct Known {
    id: &'static str,
    name: &'static str,
    does: &'static str,
    advice: Advice,
}

const PERMISSIONS: Advice = Advice::Always("it decides who may do what");
const SIGNS_IN: Advice =
    Advice::Always("it signs people in, and does nothing until it is set up and chosen");

const KNOWN: [Known; 40] = [
    Known {
        id: "rbac",
        name: "Roles and permissions",
        does: "Who may do what: roles, and the rules that give them to people as they join.",
        advice: PERMISSIONS,
    },
    Known {
        id: "local",
        name: "DOC accounts",
        does: "Usernames and passwords kept in DOC, for people without a company account.",
        advice: SIGNS_IN,
    },
    Known {
        id: "github",
        name: "GitHub",
        does: "Sign-in, teams, repositories and delivery data from GitHub.",
        advice: SIGNS_IN,
    },
    Known {
        id: "ghe",
        name: "GitHub Enterprise",
        does: "The same from GitHub Enterprise Server.",
        advice: SIGNS_IN,
    },
    Known {
        id: "oidc",
        name: "Single sign-on",
        does: "Sign-in through any OpenID Connect provider.",
        advice: SIGNS_IN,
    },
    Known { id: "google", name: "Google", does: "Sign-in with Google accounts.", advice: SIGNS_IN },
    Known {
        id: "entra",
        name: "Microsoft Entra ID",
        does: "Sign-in with Microsoft work accounts.",
        advice: SIGNS_IN,
    },
    Known { id: "akamai", name: "Akamai", does: "Sign-in through Akamai.", advice: SIGNS_IN },
    Known {
        id: "secrets",
        name: "Secret Storage",
        does: "Every credential kept once, and shared only with the plugins that need it.",
        advice: Advice::Suggested,
    },
    Known {
        id: "notifications",
        name: "Notifications",
        does: "The bell in the header, where plugins tell people things.",
        advice: Advice::Suggested,
    },
    Known {
        id: "resources",
        name: "Catalogue",
        does: "Your services, teams and repositories, and how they connect. Most plugins build on it.",
        advice: Advice::Suggested,
    },
    Known {
        id: "service-map",
        name: "Service Map",
        does: "Maps of services and what surrounds them, drawn from the Catalogue.",
        advice: Advice::Suggested,
    },
    Known {
        id: "kb",
        name: "Knowledge Base",
        does: "Documentation from Markdown, MkDocs, GitHub, Confluence and Google Drive, found by \
               the service it is about.",
        advice: Advice::Suggested,
    },
    Known {
        id: "water",
        name: "Watercooler",
        does: "Discussions about anything in DOC, events, cards and kudos.",
        advice: Advice::Suggested,
    },
    Known {
        id: "calendar",
        name: "Calendar",
        does: "A calendar for each team, service and person, with feeds for calendar apps.",
        advice: Advice::Suggested,
    },
    Known {
        id: "calendar-events",
        name: "Calendar events",
        does: "The events on those calendars, and their reminders. Calendar needs it.",
        advice: Advice::Suggested,
    },
    Known {
        id: "process",
        name: "Process",
        does: "Recurring processes, such as a weekly handover, each with a checklist.",
        advice: Advice::Suggested,
    },
    Known {
        id: "rota",
        name: "Rota",
        does: "Who in each team is on, turn by turn, and the gaps holidays leave.",
        advice: Advice::Suggested,
    },
    Known {
        id: "automation",
        name: "Automation",
        does: "Schedules, events and webhooks that act for you.",
        advice: Advice::Suggested,
    },
    Known {
        id: "templates",
        name: "Software Templates",
        does: "New services made the same way every time.",
        advice: Advice::Suggested,
    },
    Known {
        id: "radar",
        name: "Tech Radar",
        does: "What the organisation uses, trials, assesses and holds.",
        advice: Advice::Suggested,
    },
    Known {
        id: "maturity",
        name: "Maturity Model",
        does: "What you expect of the things you run, and how far each of them meets it.",
        advice: Advice::Suggested,
    },
    Known {
        id: "architecture",
        name: "Architecture",
        does: "How services are built: their components, and the claims between them.",
        advice: Advice::Suggested,
    },
    Known {
        id: "flags",
        name: "Feature flags",
        does: "Flags for your services, kept here or read from Unleash, Flagsmith and others.",
        advice: Advice::Suggested,
    },
    Known {
        id: "eol",
        name: "End of life",
        does: "Where the languages and frameworks each service runs stand, from endoflife.date.",
        advice: Advice::Suggested,
    },
    Known {
        id: "reliability",
        name: "Reliability",
        does: "Each service's availability against its objective, and DOC's own.",
        advice: Advice::Suggested,
    },
    Known {
        id: "vacuum",
        name: "Data Vacuum",
        does: "Documentation and catalogue data brought in from Confluence, Backstage, Jira, \
               Markdown and MkDocs, written only once you approve it.",
        advice: Advice::Suggested,
    },
    Known {
        id: "insights",
        name: "Repository Insights",
        does: "What is in each repository, looked at again whenever a pull request is merged.",
        advice: Advice::ForTools,
    },
    Known {
        id: "dora",
        name: "DORA metrics",
        does: "Deployment frequency, lead time, change fail rate and recovery time.",
        advice: Advice::ForTools,
    },
    Known {
        id: "cicd",
        name: "CI/CD/CT metrics",
        does: "How often pipelines pass, how long they take, and how often only a re-run passes.",
        advice: Advice::ForTools,
    },
    Known {
        id: "roadmap",
        name: "Roadmap",
        does: "Every release planned in Jira, and whether its services are ready to ship.",
        advice: Advice::ForTools,
    },
    Known {
        id: "jira",
        name: "Jira",
        does: "Releases and their issues from Jira Cloud, for the roadmap.",
        advice: Advice::ForTools,
    },
    Known {
        id: "jira-dc",
        name: "Jira Data Center",
        does: "The same from Jira Data Center or Server.",
        advice: Advice::ForTools,
    },
    Known {
        id: "kubernetes",
        name: "Kubernetes",
        does: "What runs in your clusters, for DORA, CI/CD/CT and Reliability.",
        advice: Advice::ForTools,
    },
    Known {
        id: "grafana",
        name: "Grafana",
        does: "Grafana dashboards on the services they are about.",
        advice: Advice::ForTools,
    },
    Known {
        id: "infra",
        name: "Infra",
        does: "Machines and buckets people ask for themselves from your cloud vendors.",
        advice: Advice::ForTools,
    },
    Known {
        id: "agent",
        name: "Agent Smith",
        does: "An MCP server for your own agent, and jobs DOC runs with Claude.",
        advice: Advice::ForTools,
    },
    Known {
        id: "dns",
        name: "DNS",
        does: "DOC as the name server for its own domains.",
        advice: Advice::When("DOC should answer for your own domains"),
    },
    Known {
        id: "faux-data",
        name: "Faux data",
        does: "Made-up delivery, pipeline, reliability and release data, to see the metrics \
               before your tools are connected.",
        advice: Advice::Trying,
    },
    Known {
        id: "hello",
        name: "Hello",
        does: "An example plugin, for people writing their own.",
        advice: Advice::Example,
    },
];

fn known(id: &str) -> Option<&'static Known> {
    KNOWN.iter().find(|known| known.id == id)
}

/// The plain name of a plugin, where the setup knows one.
fn plugin_name(id: &str) -> String {
    known(id).map_or_else(|| id.to_string(), |known| known.name.to_string())
}

/// The tools that list a plugin, by name: all of them, or only those chosen.
fn tools_for(plugin: &str, chosen: Option<&[String]>) -> Vec<&'static str> {
    TOOLS
        .iter()
        .filter(|tool| tool.plugins.contains(&plugin))
        .filter(|tool| chosen.is_none_or(|chosen| chosen.iter().any(|id| id == tool.id)))
        .map(|tool| tool.name)
        .collect()
}

/// "A", "A or B", "A, B or C".
fn either(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [one] => (*one).to_string(),
        [rest @ .., last] => format!("{} or {last}", rest.join(", ")),
    }
}

/// "A", "A and B", "A, B and C".
fn both(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [one] => (*one).to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// Whether to suggest keeping a plugin on with these tools; one it does not know stays as it is.
fn suggested(plugin: &str, tools: &[String], on: bool) -> bool {
    let Some(known) = known(plugin) else { return on };
    match known.advice {
        Advice::Always(_) | Advice::Suggested => true,
        Advice::ForTools => !tools_for(plugin, Some(tools)).is_empty(),
        Advice::When(_) | Advice::Trying | Advice::Example => false,
    }
}

/// What the plugins step says of a plugin under its name.
fn advice(plugin: &str, tools: &[String]) -> String {
    let Some(known) = known(plugin) else { return String::new() };
    match known.advice {
        Advice::Always(why) => format!("Always on: {why}."),
        Advice::Suggested => "Suggested for everyone.".into(),
        Advice::ForTools => match tools_for(plugin, Some(tools)).as_slice() {
            [] => format!("For {}.", either(&tools_for(plugin, None))),
            chosen => format!("Suggested, as you use {}.", both(chosen)),
        },
        Advice::When(when) => format!("Turn it on if {when}."),
        Advice::Trying => "For trying DOC out with made-up data.".into(),
        Advice::Example => "An example, for people writing plugins.".into(),
    }
}

/// One plugin on the plugins step.
pub struct PluginChoice {
    pub id: String,
    pub name: String,
    pub does: String,
    pub advice: String,
    pub checked: bool,
    /// Why it cannot be turned on or off here, when it cannot.
    pub locked: Option<String>,
    /// What it is now, when that is worth saying.
    pub now: Option<String>,
}

fn locked(row: &PluginRow) -> Option<String> {
    if let Some(following) = &row.follows {
        return Some(format!("Turned on and off by the flag {}.", following.flag));
    }
    match known(&row.id).map(|known| known.advice) {
        Some(Advice::Always(why)) => Some(format!("Always on: {why}.")),
        _ => None,
    }
}

fn is_on(row: &PluginRow) -> bool {
    row.turned_off.is_none()
}

fn choices(rows: &[PluginRow], tools: &[String], chosen_before: bool) -> Vec<PluginChoice> {
    rows.iter()
        .map(|row| {
            let locked = locked(row);
            let checked = match (&locked, chosen_before) {
                (Some(_), _) | (None, true) => is_on(row),
                (None, false) => suggested(&row.id, tools, is_on(row)),
            };
            let now = match (row.turned_off.is_some(), row.state.as_deref()) {
                (true, _) => Some("Off now.".to_string()),
                (false, None) => Some("Not running in this deployment.".to_string()),
                (false, Some("error")) => Some("It has an error: see Admin › Plugins.".to_string()),
                _ => None,
            };
            PluginChoice {
                id: row.id.clone(),
                name: plugin_name(&row.id),
                does: known(&row.id).map(|known| known.does.to_string()).unwrap_or_default(),
                advice: advice(&row.id, tools),
                checked,
                locked,
                now,
            }
        })
        .collect()
}

/// What changed when the plugins step was saved, and what was refused.
#[derive(Default)]
struct Switched {
    on: usize,
    off: usize,
    refused: Vec<String>,
}

/// Turns each plugin that can be switched on or off as `wanted` says.
async fn switch(
    state: &AppState,
    signed: &Signed,
    rows: &[PluginRow],
    wanted: &BTreeSet<String>,
) -> Switched {
    let mut switched = Switched::default();
    for row in rows.iter().filter(|row| locked(row).is_none()) {
        let want = wanted.contains(&row.id);
        if want == is_on(row) {
            continue;
        }
        let action = if want { "turn-on" } else { "turn-off" };
        match state.backend.plugin_action(signed.token(), &row.id, action).await {
            Ok(()) if want => switched.on += 1,
            Ok(()) => switched.off += 1,
            Err(err) => {
                switched.refused.push(format!("{}: {}", plugin_name(&row.id), err.detail()))
            }
        }
    }
    switched
}

/// A plugin on that can read from a tool just set up, and what would be switched on for it.
pub struct Feed {
    pub id: String,
    pub name: String,
    /// What it reads, as "GitHub's delivery data".
    pub from: String,
    features: Vec<String>,
    sources: Vec<(String, String)>,
}

/// Each plugin on whose features read from another's, where a source it reads is set up.
fn feeds(rows: &[PluginRow], ready: &BTreeSet<String>) -> Vec<Feed> {
    rows.iter()
        .filter(|row| is_on(row))
        .filter_map(|row| {
            let offer = row.enable.as_ref().filter(|offer| offer.blocked.is_none())?;
            let sources: Vec<(String, String)> = offer
                .with
                .iter()
                .filter(|with| ready.contains(&with.plugin))
                .map(|with| (with.plugin.clone(), with.feature.clone()))
                .collect();
            if sources.is_empty() {
                return None;
            }
            let from: Vec<String> = sources
                .iter()
                .map(|(plugin, feature)| {
                    format!("{}'s {}", plugin_name(plugin), feature.replace('-', " "))
                })
                .collect();
            Some(Feed {
                id: row.id.clone(),
                name: plugin_name(&row.id),
                from: both(&from.iter().map(String::as_str).collect::<Vec<_>>()),
                features: offer.features.clone(),
                sources,
            })
        })
        .collect()
}

/// Switches on the sources' features first, so the plugin finds its data flowing when it reloads.
async fn feed(state: &AppState, token: &str, feed: &Feed) -> Result<(), String> {
    let refused =
        |plugin: &str, err: BackendError| format!("{}: {}", plugin_name(plugin), err.detail());
    for (plugin, feature) in &feed.sources {
        let wanted = std::iter::once((feature.clone(), true)).collect();
        state
            .backend
            .set_plugin_features(token, plugin, &wanted)
            .await
            .map_err(|err| refused(plugin, err))?;
    }
    let wanted = feed.features.iter().map(|feature| (feature.clone(), true)).collect();
    state
        .backend
        .set_plugin_features(token, &feed.id, &wanted)
        .await
        .map_err(|err| refused(&feed.id, err))?;
    Ok(())
}

/// A way of signing in, on the sign-in step.
pub struct SignInRow {
    pub id: String,
    pub name: String,
    pub running: bool,
    /// The organisation whose people sign in with it.
    pub organisation: Option<String>,
    /// Whether it has what it needs to work, where that can be told from its settings.
    pub ready: Option<bool>,
    /// One of the tools the organisation said it uses.
    pub suggested: bool,
}

/// Settings that say a sign-in plugin is ready, where it needs any.
fn sign_in_keys(id: &str) -> &'static [&'static str] {
    match id {
        "local" => &[],
        "github" | "ghe" => &["client-id"],
        _ => &["issuer"],
    }
}

/// How one tool is connected: the plugin whose settings take what it needs, and how to tell.
struct Wire {
    tool: &'static str,
    plugin: &'static str,
    /// Any of these set means it is set up.
    keys: &'static [&'static str],
    /// Or any credential held by name.
    named: bool,
    how: &'static str,
    more: Option<More>,
}

/// A second page worth opening for a tool, beside its plugin's settings.
#[derive(Clone, Copy)]
pub struct More {
    pub label: &'static str,
    pub href: &'static str,
}

const WIRES: [Wire; 12] = [
    Wire {
        tool: "github",
        plugin: "github",
        keys: &["token", "app-id"],
        named: false,
        how: "An access token, or a GitHub App, that can read your organisations, and the \
              organisations to read. Teams, members and repositories then sync every half hour, \
              and DORA and CI/CD/CT read its deployments and workflow runs.",
        more: None,
    },
    Wire {
        tool: "ghe",
        plugin: "ghe",
        keys: &["token", "app-id"],
        named: false,
        how: "Its address, then a token or a GitHub App and the organisations to read, as for \
              GitHub.",
        more: None,
    },
    Wire {
        tool: "kubernetes",
        plugin: "kubernetes",
        keys: &[],
        named: true,
        how: "Each cluster's kubeconfig, for a read-only service account, under Clusters, and \
              which environment each one is. The Kubernetes page shows the manifest that makes \
              the account.",
        more: Some(More { label: "Open Kubernetes", href: "/p/kubernetes/" }),
    },
    Wire {
        tool: "cloud",
        plugin: "infra",
        keys: &[],
        named: false,
        how: "Each vendor's credentials, and the templates that say what people may ask for.",
        more: Some(More { label: "Open Infra", href: "/p/infra/" }),
    },
    Wire {
        tool: "grafana",
        plugin: "grafana",
        keys: &["account"],
        named: false,
        how: "Grafana kept in Secret Storage as a vendor account, then chosen here with the \
              dashboards to show.",
        more: Some(More { label: "Open Secret Storage", href: "/p/secrets/" }),
    },
    Wire {
        tool: "jira",
        plugin: "jira",
        keys: &["base-url"],
        named: false,
        how: "Your site's address, an account's email and API token, and the projects whose \
              releases to read.",
        more: None,
    },
    Wire {
        tool: "jira-dc",
        plugin: "jira-dc",
        keys: &["base-url"],
        named: false,
        how: "Your server's address, a personal access token, and the projects whose releases \
              to read.",
        more: None,
    },
    Wire {
        tool: "confluence",
        plugin: "kb",
        keys: &[],
        named: true,
        how: "A credential under the Knowledge Base's Credentials: email:api-token for Cloud, or \
              a personal access token for Data Center. Spaces are added in the next step.",
        more: None,
    },
    Wire {
        tool: "google",
        plugin: "kb",
        keys: &[],
        named: true,
        how: "A Google service account's JSON key under the Knowledge Base's Credentials, with \
              your folders shared with that account. Sign-in is the step before.",
        more: None,
    },
    Wire {
        tool: "backstage",
        plugin: "vacuum",
        keys: &["backstage-url"],
        named: false,
        how: "Backstage's backend address and a static token it accepts, so the Data Vacuum can \
              read its catalogue.",
        more: None,
    },
    Wire {
        tool: "claude",
        plugin: "vacuum",
        keys: &["anthropic-api-key"],
        named: false,
        how: "An Anthropic API key, for the Data Vacuum to read your old tools with. Without one, \
              it gives your own agent the instructions instead.",
        more: None,
    },
    Wire {
        tool: "claude",
        plugin: "agent",
        keys: &["anthropic-api-key"],
        named: false,
        how: "The same key, for the jobs Agent Smith runs. Your own agent can use its MCP server \
              without one.",
        more: None,
    },
];

/// One tool to connect, on the connect step.
pub struct Connection {
    pub tool: String,
    pub plugin: String,
    pub plugin_name: String,
    pub how: String,
    /// The badge's modifier, and its word.
    pub badge: &'static str,
    pub word: &'static str,
    /// Whether the plugin can be set up now: on, and running.
    pub open: bool,
    pub more: Option<More>,
}

/// Whether a setting has been given a value, here or by the deployment.
fn given(value: &SettingValue) -> bool {
    let blank = match &value.value {
        Value::Null => true,
        Value::String(text) => text.trim().is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => map.is_empty(),
        _ => false,
    };
    value.source != "default" && (value.set || !blank)
}

fn ready(settings: &PluginSettings, keys: &[&str], named: bool) -> bool {
    let set =
        settings.current.iter().any(|value| keys.contains(&value.key.as_str()) && given(value));
    set || (named && !settings.named.is_empty())
}

/// The Catalogue's stored kinds that hold anything, as "3 services and 12 repositories".
fn catalogue_count(kinds: &Value) -> Option<String> {
    let counted: Vec<String> = kinds["kinds"]
        .as_array()?
        .iter()
        .filter(|kind| kind["from"] == "resources")
        .filter_map(|kind| {
            let count = kind["count"].as_u64().unwrap_or(0);
            let plural = kind["plural"].as_str()?.to_lowercase();
            (count > 0).then(|| format!("{count} {plural}"))
        })
        .collect();
    let named: Vec<&str> = counted.iter().map(String::as_str).collect();
    Some(match named.is_empty() {
        true => "nothing yet".to_string(),
        false => both(&named),
    })
}

/// The tools under one heading, on the tools step.
pub struct ToolGroup {
    pub name: &'static str,
    pub tools: Vec<&'static Tool>,
}

/// One stage in the contents down the side: the overview, then each step.
pub struct Stage {
    pub title: &'static str,
    pub href: String,
    pub current: bool,
    pub done: bool,
}

/// The overview's row for each step.
pub struct StepRow {
    pub step: Step,
    pub done: bool,
}

#[derive(Template)]
#[template(path = "setup.html")]
pub struct SetupPage {
    pub chrome: Chrome,
    pub stages: Vec<Stage>,
    /// The step open, or none for the overview.
    pub step: Option<Step>,
    pub rows: Vec<StepRow>,
    pub setup: Setup,
    pub notice: Option<String>,
    pub error: Option<String>,
    /// Why some of what the step tried was refused, each its own line.
    pub refused: Vec<String>,
    pub instance_name: String,
    pub tool_groups: Vec<ToolGroup>,
    pub plugin_sections: Vec<Section<PluginChoice>>,
    pub organisations: Vec<crate::backend::Organisation>,
    pub teams: usize,
    pub people: usize,
    pub sign_ins: Vec<SignInRow>,
    pub connections: Vec<Connection>,
    pub feeds: Vec<Feed>,
    /// The plugins on and running, so a step offers only the pages that will open.
    pub running: BTreeSet<String>,
    /// What the Catalogue holds, as "3 services and 12 repositories".
    pub catalogue: Option<String>,
    /// How many sources the Knowledge Base reads, as a sentence.
    pub sources: Option<String>,
}

impl SetupPage {
    fn at(&self, step: &str) -> bool {
        self.step.is_some_and(|open| open.slug() == step)
    }

    fn uses(&self, tool: &str) -> bool {
        self.setup.tools.iter().any(|chosen| chosen == tool)
    }

    fn has(&self, plugin: &str) -> bool {
        self.running.contains(plugin)
    }

    fn done_count(&self) -> usize {
        self.rows.iter().filter(|row| row.done && row.step != Step::Finish).count()
    }

    /// Where Start or Continue goes: the first step not done.
    fn onward(&self) -> String {
        self.rows.iter().find(|row| !row.done).map_or(Step::Finish, |row| row.step).href()
    }

    fn next_href(&self) -> String {
        self.step.and_then(Step::next).unwrap_or(Step::Finish).href()
    }

    fn number(&self) -> usize {
        self.step.map_or(0, Step::number)
    }

    fn of(&self) -> usize {
        STEPS.len()
    }
}

/// What the step before left to say, carried in the address it redirected to.
#[derive(Debug, Default, Deserialize)]
pub struct Saved {
    #[serde(default)]
    pub on: Option<usize>,
    #[serde(default)]
    pub off: Option<usize>,
    /// How many plugins were switched on to read from the tools just connected.
    #[serde(default)]
    pub fed: Option<usize>,
}

impl Saved {
    fn notice(&self) -> Option<String> {
        let count = |n: usize| match n {
            1 => "1 plugin".to_string(),
            n => format!("{n} plugins"),
        };
        if let Some(fed) = self.fed {
            return Some(format!("Switched on {} to read from what you connected.", count(fed)));
        }
        let (on, off) = (self.on?, self.off.unwrap_or(0));
        Some(match (on, off) {
            (0, 0) => "Plugins saved: nothing needed turning on or off.".to_string(),
            (on, 0) => format!("Plugins saved: turned on {}.", count(on)),
            (0, off) => format!("Plugins saved: turned off {}.", count(off)),
            (on, off) => format!("Plugins saved: turned on {} and off {}.", count(on), count(off)),
        })
    }
}

#[derive(Default)]
struct Flash {
    notice: Option<String>,
    error: Option<String>,
    refused: Vec<String>,
    /// What was typed, shown again when it was refused.
    instance_name: Option<String>,
}

async fn page(
    state: &AppState,
    signed: &Signed,
    csrf: &Csrf,
    step: Option<Step>,
    flash: Flash,
) -> Result<Response, WebError> {
    let token = signed.token();
    let mut setup = state.backend.setup(token).await?;
    if setup.started_at.is_none() {
        setup = state.backend.change_setup(token, &json!({ "started": true })).await?;
    }
    let access = session::access(state, signed).await;
    let running: BTreeSet<String> = access
        .plugins
        .iter()
        .filter(|(_, plugin)| plugin.running)
        .map(|(id, _)| id.clone())
        .collect();
    let title = step.map_or("Set up DOC", Step::title);
    let chrome = Chrome::new(title, "/setup")
        .headed("Set up DOC")
        .signed(signed, csrf)
        .with_plugins(state, signed)
        .await;
    let done = |step: Step| setup.done.iter().any(|done| done == step.slug());
    let mut stages = vec![Stage {
        title: "Overview",
        href: "/setup".into(),
        current: step.is_none(),
        done: false,
    }];
    stages.extend(STEPS.iter().map(|each| Stage {
        title: each.title(),
        href: each.href(),
        current: step == Some(*each),
        done: done(*each),
    }));
    let rows = STEPS.iter().map(|each| StepRow { step: *each, done: done(*each) }).collect();
    let mut view = SetupPage {
        chrome,
        stages,
        step,
        rows,
        setup: Setup::default(),
        notice: flash.notice,
        error: flash.error,
        refused: flash.refused,
        instance_name: String::new(),
        tool_groups: Vec::new(),
        plugin_sections: Vec::new(),
        organisations: Vec::new(),
        teams: 0,
        people: 0,
        sign_ins: Vec::new(),
        connections: Vec::new(),
        feeds: Vec::new(),
        running,
        catalogue: None,
        sources: None,
    };
    match step {
        Some(Step::Name) => {
            view.instance_name = match flash.instance_name {
                Some(typed) => typed,
                None => state.backend.settings(token).await?.instance_name,
            };
        }
        Some(Step::Tools) => {
            view.tool_groups = TOOL_GROUPS
                .iter()
                .map(|name| ToolGroup {
                    name,
                    tools: TOOLS.iter().filter(|tool| tool.group == *name).collect(),
                })
                .collect();
        }
        Some(Step::Plugins) => {
            let rows = state.backend.plugins(token).await?.plugins;
            let chosen_before = done(Step::Plugins);
            let items = choices(&rows, &setup.tools, chosen_before);
            view.plugin_sections = categories::sections(&access.categories, items, |item| &item.id);
        }
        Some(Step::Organisation) => {
            view.organisations = state.backend.organisations(token).await?;
            view.teams = state.backend.teams(token).await.map(|teams| teams.len()).unwrap_or(0);
            view.people = state.backend.users(token).await.map(|users| users.len()).unwrap_or(0);
        }
        Some(Step::SignIn) => {
            view.organisations = state.backend.organisations(token).await?;
            view.sign_ins = sign_ins(state, token, &view.organisations, &setup.tools).await?;
        }
        Some(Step::Connect) => {
            let (connections, ready) = connections(state, token, &setup.tools, &view.running).await;
            view.connections = connections;
            view.feeds = feeds(&state.backend.plugins(token).await?.plugins, &ready);
        }
        Some(Step::Data) => {
            if view.running.contains("resources") {
                view.catalogue = state
                    .backend
                    .plugin_get(token, "resources", "kinds")
                    .await
                    .ok()
                    .and_then(|kinds| catalogue_count(&kinds));
            }
            if view.running.contains("kb") {
                view.sources = state
                    .backend
                    .plugin_get(token, "kb", "sources")
                    .await
                    .ok()
                    .and_then(|sources| sources.as_array().map(Vec::len))
                    .map(|count| match count {
                        0 => "The Knowledge Base has no sources yet.".to_string(),
                        1 => "The Knowledge Base reads 1 source.".to_string(),
                        n => format!("The Knowledge Base reads {n} sources."),
                    });
            }
        }
        Some(Step::Finish) | None => {}
    }
    view.setup = setup;
    Ok(Html(view.render()?).into_response())
}

async fn sign_ins(
    state: &AppState,
    token: &str,
    organisations: &[crate::backend::Organisation],
    tools: &[String],
) -> Result<Vec<SignInRow>, WebError> {
    let Some(first) = organisations.first() else { return Ok(Vec::new()) };
    let (_, _, providers) = state.backend.organisation(token, &first.id).await?;
    let mut rows = Vec::with_capacity(providers.len());
    for provider in providers {
        let keys = sign_in_keys(&provider.id);
        let ready = match (keys.is_empty(), provider.running) {
            (true, _) => None,
            (false, false) => Some(false),
            (false, true) => match state.backend.plugin_settings(token, &provider.id).await {
                Ok(settings) => Some(ready(&settings, keys, false)),
                Err(_) => None,
            },
        };
        let organisation = provider.organisation.as_deref().and_then(|id| {
            organisations.iter().find(|organisation| organisation.id == id).map(|o| o.title.clone())
        });
        rows.push(SignInRow {
            suggested: !tools_for(&provider.id, Some(tools)).is_empty(),
            name: plugin_name(&provider.id),
            id: provider.id,
            running: provider.running,
            organisation,
            ready,
        });
    }
    Ok(rows)
}

/// Each tool in use to connect, and the plugins already set up to read one.
async fn connections(
    state: &AppState,
    token: &str,
    tools: &[String],
    running: &BTreeSet<String>,
) -> (Vec<Connection>, BTreeSet<String>) {
    let mut ready_plugins = BTreeSet::new();
    let mut read: std::collections::BTreeMap<&str, Option<PluginSettings>> = Default::default();
    let mut connections = Vec::new();
    for wire in WIRES.iter().filter(|wire| tools.iter().any(|chosen| chosen == wire.tool)) {
        let open = running.contains(wire.plugin);
        if open && !read.contains_key(wire.plugin) {
            let settings = state.backend.plugin_settings(token, wire.plugin).await.ok();
            read.insert(wire.plugin, settings);
        }
        let (badge, word) = match (open, read.get(wire.plugin).and_then(Option::as_ref)) {
            (false, _) => ("degraded", "Turned off or not running"),
            (true, None) => ("unknown", "Could not be read"),
            (true, Some(_)) if wire.keys.is_empty() && !wire.named => {
                ("unknown", "See its settings")
            }
            (true, Some(settings)) if ready(settings, wire.keys, wire.named) => {
                ready_plugins.insert(wire.plugin.to_string());
                ("up", "Set up")
            }
            (true, Some(_)) => ("unknown", "Not set up yet"),
        };
        connections.push(Connection {
            tool: tool(wire.tool).map_or(wire.tool, |tool| tool.name).to_string(),
            plugin: wire.plugin.to_string(),
            plugin_name: plugin_name(wire.plugin),
            how: wire.how.to_string(),
            badge,
            word,
            open,
            more: wire.more,
        });
    }
    (connections, ready_plugins)
}

pub async fn overview(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
) -> Result<Response, WebError> {
    page(&state, &signed, &csrf, None, Flash::default()).await
}

pub async fn show(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(slug): Path<String>,
    Query(saved): Query<Saved>,
) -> Result<Response, WebError> {
    let step = Step::parse(&slug).ok_or(WebError::NotFound)?;
    let flash = Flash { notice: saved.notice(), ..Flash::default() };
    page(&state, &signed, &csrf, Some(step), flash).await
}

/// A refusal worth showing on the step rather than as an error page.
fn actionable(err: &BackendError) -> bool {
    matches!(err.status(), Some(400 | 403 | 404 | 409))
}

/// Saves a step, marks it done and goes on; finishing goes to the accounts plugins need, then home.
pub async fn save(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(slug): Path<String>,
    Form(form): Form<Vec<(String, String)>>,
) -> Result<Response, WebError> {
    let step = Step::parse(&slug).ok_or(WebError::NotFound)?;
    let token = signed.token();
    let ticked = |name: &str| -> Vec<String> {
        form.iter().filter(|(key, _)| key == name).map(|(_, value)| value.clone()).collect()
    };
    let mut onward = step.next().map(Step::href);
    match step {
        Step::Name => {
            let typed = ticked("instance_name").concat().trim().to_string();
            let wanted = Settings { instance_name: typed.clone() };
            match state.backend.set_settings(token, &wanted).await {
                Ok(saved) => {
                    session::forget_access(&state).await;
                    super::pages::set_instance(&saved.instance_name);
                }
                Err(err) if actionable(&err) => {
                    let flash = Flash {
                        error: Some(err.detail()),
                        instance_name: Some(typed),
                        ..Flash::default()
                    };
                    return page(&state, &signed, &csrf, Some(step), flash).await;
                }
                Err(err) => return Err(err.into()),
            }
        }
        Step::Tools => {
            let tools: Vec<String> =
                ticked("tool").into_iter().filter(|id| tool(id).is_some()).collect();
            state.backend.change_setup(token, &json!({ "tools": tools })).await?;
        }
        Step::Plugins => {
            let rows = state.backend.plugins(token).await?.plugins;
            let wanted: BTreeSet<String> = ticked("plugin").into_iter().collect();
            let switched = switch(&state, &signed, &rows, &wanted).await;
            session::forget_access(&state).await;
            if !switched.refused.is_empty() {
                let flash = Flash {
                    error: Some("Some plugins could not be changed; the rest were.".into()),
                    refused: switched.refused,
                    ..Flash::default()
                };
                state.backend.change_setup(token, &json!({ "step": step.slug() })).await?;
                return page(&state, &signed, &csrf, Some(step), flash).await;
            }
            onward = onward.map(|href| format!("{href}?on={}&off={}", switched.on, switched.off));
        }
        Step::Connect => {
            let setup = state.backend.setup(token).await?;
            let running: BTreeSet<String> = session::access(&state, &signed)
                .await
                .plugins
                .into_iter()
                .filter(|(_, plugin)| plugin.running)
                .map(|(id, _)| id)
                .collect();
            let (_, ready) = connections(&state, token, &setup.tools, &running).await;
            let rows = state.backend.plugins(token).await?.plugins;
            let wanted: BTreeSet<String> = ticked("feed").into_iter().collect();
            let (mut fed, mut refused) = (0, Vec::new());
            for each in feeds(&rows, &ready).iter().filter(|each| wanted.contains(&each.id)) {
                match feed(&state, token, each).await {
                    Ok(()) => fed += 1,
                    Err(why) => refused.push(why),
                }
            }
            if !refused.is_empty() {
                let flash = Flash {
                    error: Some("Some plugins could not be switched on; the rest were.".into()),
                    refused,
                    ..Flash::default()
                };
                state.backend.change_setup(token, &json!({ "step": step.slug() })).await?;
                return page(&state, &signed, &csrf, Some(step), flash).await;
            }
            if fed > 0 {
                onward = onward.map(|href| format!("{href}?fed={fed}"));
            }
        }
        Step::Finish => {
            state
                .backend
                .change_setup(token, &json!({ "step": step.slug(), "finished": true }))
                .await?;
            return Ok(Redirect::to("/welcome?return_to=%2F").into_response());
        }
        Step::Organisation | Step::SignIn | Step::Data => {}
    }
    state.backend.change_setup(token, &json!({ "step": step.slug() })).await?;
    Ok(Redirect::to(&onward.unwrap_or_else(|| Step::Finish.href())).into_response())
}

/// Whether a platform administrator has yet to open the setup, which their first sign-in opens.
pub async fn unopened(state: &AppState, signed: &Signed) -> bool {
    match state.backend.setup(signed.token()).await {
        Ok(setup) => setup.started_at.is_none() && setup.finished_at.is_none(),
        Err(_) => false,
    }
}

/// The home page's reminder to an administrator while the setup is not finished.
pub struct Reminder {
    pub done: usize,
    pub of: usize,
    pub href: String,
}

pub async fn reminder(state: &AppState, signed: &Signed) -> Option<Reminder> {
    let setup = state.backend.setup(signed.token()).await.ok()?;
    if setup.finished_at.is_some() {
        return None;
    }
    let steps: Vec<Step> = STEPS.into_iter().filter(|step| *step != Step::Finish).collect();
    let done = steps.iter().filter(|step| setup.done.iter().any(|d| d == step.slug())).count();
    let next = steps.iter().find(|step| !setup.done.iter().any(|d| d == step.slug()));
    Some(Reminder {
        done,
        of: steps.len(),
        href: match (done, next) {
            (0, _) => "/setup".to_string(),
            (_, Some(step)) => step.href(),
            (_, None) => Step::Finish.href(),
        },
    })
}
