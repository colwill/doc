//! Making a group one question at a time: what it is called and who may be in it, which plugins
//! it grants in, which permissions of theirs it holds, and who is in it to begin with. The answers
//! travel between steps in the form, so nothing is stored until the last step is taken.

use std::collections::BTreeMap;

use askama::Template;
use doc_permissions::MemberKind;
use doc_plugin_sdk::Backend;

use super::{Flash, Form, field, fields, group_page, member_named, render};
use crate::model::{GroupRecord, Refusal};
use crate::ops;
use crate::store::Store;

/// Every answer a step can give, besides the ticked plugins and permissions, which are lists.
const ANSWERS: [&str; 4] = ["name", "kind", "description", "members"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Details,
    Plugins,
    Permissions,
    Members,
}

pub const STEPS: [Step; 4] = [Step::Details, Step::Plugins, Step::Permissions, Step::Members];

impl Step {
    pub fn name(self) -> &'static str {
        match self {
            Self::Details => "details",
            Self::Plugins => "plugins",
            Self::Permissions => "permissions",
            Self::Members => "members",
        }
    }

    fn named(name: &str) -> Option<Self> {
        STEPS.into_iter().find(|step| step.name() == name)
    }
}

/// What has been answered so far, carried through the form as fields and hidden fields.
#[derive(Debug, Clone, Default)]
pub struct Answers {
    written: BTreeMap<String, String>,
    pub plugins: Vec<String>,
    pub permissions: Vec<String>,
}

impl Answers {
    fn of(form: &Form) -> Self {
        Self {
            written: ANSWERS
                .iter()
                .map(|name| ((*name).to_string(), field(form, name)))
                .filter(|(_, value)| !value.is_empty())
                .collect(),
            plugins: fields(form, "plugin"),
            permissions: fields(form, "permission"),
        }
    }

    pub fn get(&self, name: &str) -> &str {
        self.written.get(name).map_or("", String::as_str)
    }

    pub fn kind(&self) -> MemberKind {
        match self.get("kind") {
            "service" => MemberKind::Service,
            _ => MemberKind::User,
        }
    }

    /// The plugin the group is listed under: the first one ticked, in the order they are shown.
    pub fn home(&self) -> &str {
        self.plugins.first().map_or("core", String::as_str)
    }

    /// The people or accounts named in the last step, which are added once the group is made.
    pub fn members(&self) -> Vec<String> {
        self.get("members")
            .split(',')
            .flat_map(|name| name.split_whitespace())
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// Permissions the group would hold on plugins that are no longer ticked are dropped, so
    /// going back and unticking a plugin takes its permissions with it.
    fn only_ticked(&mut self) {
        let plugins = self.plugins.clone();
        self.permissions.retain(|held| {
            crate::model::plugin_of(held)
                .is_some_and(|plugin| plugins.iter().any(|ticked| ticked == plugin))
        });
    }

    /// What is wrong with a step's answers, field by field.
    fn problems(&self, step: Step) -> Vec<(String, String)> {
        let mut problems = Vec::new();
        match step {
            Step::Details => {
                if let Err(refusal) = ops::group_name(self.get("name")) {
                    problems.push(("name".into(), sentence(&refusal.detail)));
                }
                if let Err(refusal) = ops::described(self.get("description")) {
                    problems.push(("description".into(), sentence(&refusal.detail)));
                }
            }
            Step::Plugins => {
                if self.plugins.is_empty() {
                    problems.push((
                        "plugin".into(),
                        "Tick at least one plugin. The first is the one the group is listed under."
                            .into(),
                    ));
                }
            }
            Step::Permissions | Step::Members => {}
        }
        problems
    }

    fn first_unfinished(&self) -> Option<Step> {
        STEPS.into_iter().find(|step| !self.problems(*step).is_empty())
    }
}

fn sentence(text: &str) -> String {
    let mut chars = text.chars();
    let first = chars.next().map(|c| c.to_uppercase().collect::<String>()).unwrap_or_default();
    let text = format!("{first}{}", chars.as_str());
    match text.ends_with('.') {
        true => text,
        false => format!("{text}."),
    }
}

/// One plugin's permissions as the third step offers them, with which are ticked.
pub struct Offer {
    pub plugin: String,
    pub listed_under: bool,
    pub choices: Vec<(String, bool)>,
}

#[derive(Template)]
#[template(path = "group_wizard.html")]
pub struct WizardPage {
    pub flash: Flash,
    pub step: Step,
    pub number: usize,
    pub of: usize,
    pub answers: Answers,
    pub problems: Vec<(String, String)>,
    /// Every plugin a permission may name, for the second step.
    pub plugins: Vec<String>,
    /// What the third step offers, one fieldset for each plugin ticked.
    pub offers: Vec<Offer>,
}

impl WizardPage {
    fn at(&self, step: &str) -> bool {
        self.step.name() == step
    }

    fn problem(&self, name: &str) -> Option<&str> {
        self.problems.iter().find(|(field, _)| field == name).map(|(_, message)| message.as_str())
    }

    fn value(&self, name: &str) -> &str {
        self.answers.get(name)
    }

    fn ticked(&self, plugin: &str) -> bool {
        self.answers.plugins.iter().any(|held| held == plugin)
    }

    /// The answers a step does not show as fields, carried on as hidden ones. The plugins and
    /// permissions are lists, and are carried by the two below.
    fn carried(&self) -> Vec<(&str, &str)> {
        let asked: &[&str] = match self.step {
            Step::Details => &["name", "kind", "description"],
            Step::Members => &["members"],
            _ => &[],
        };
        self.answers
            .written
            .iter()
            .filter(|(name, _)| !asked.contains(&name.as_str()))
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect()
    }

    fn carried_plugins(&self) -> &[String] {
        match self.step {
            Step::Plugins => &[],
            _ => &self.answers.plugins,
        }
    }

    fn carried_permissions(&self) -> &[String] {
        match self.step {
            Step::Permissions => &[],
            _ => &self.answers.permissions,
        }
    }

    /// What the last step shows for checking, with the step that changes each.
    fn summary(&self) -> Vec<(&'static str, String, &'static str)> {
        let kind = match self.answers.kind() {
            MemberKind::User => "users",
            MemberKind::Service => "service accounts",
        };
        let permissions = match self.answers.permissions.is_empty() {
            true => format!("none, so it grants {}'s read-only default", self.answers.home()),
            false => self.answers.permissions.join(", "),
        };
        vec![
            ("Name", format!("{}/{}", self.answers.home(), self.answers.get("name")), "details"),
            ("Members are", kind.to_string(), "details"),
            ("Grants in", self.answers.plugins.join(", "), "plugins"),
            ("Permissions", permissions, "permissions"),
        ]
    }
}

/// The permissions each ticked plugin offers, minus memberships: groups do not nest.
async fn offers(store: &Store<'_>, answers: &Answers) -> Result<Vec<Offer>, Refusal> {
    let known = store.known().await?;
    let home = answers.home().to_string();
    Ok(answers
        .plugins
        .iter()
        .map(|plugin| Offer {
            listed_under: *plugin == home,
            choices: ops::choices(&known, &[], plugin, answers.kind(), false)
                .into_iter()
                .map(|choice| {
                    let held = answers.permissions.contains(&choice);
                    (choice, held)
                })
                .collect(),
            plugin: plugin.clone(),
        })
        .collect())
}

async fn page(
    store: &Store<'_>,
    answers: Answers,
    step: Step,
    problems: Vec<(String, String)>,
    flash: Flash,
) -> Result<String, Refusal> {
    let plugins = match step {
        Step::Plugins => store.known().await?.names(),
        _ => Vec::new(),
    };
    let offers = match step {
        Step::Permissions => offers(store, &answers).await?,
        _ => Vec::new(),
    };
    let number = STEPS.iter().position(|each| *each == step).map_or(1, |at| at + 1);
    render(&WizardPage { flash, step, number, of: STEPS.len(), answers, problems, plugins, offers })
}

/// The first step, with nothing answered yet.
pub async fn start(store: &Store<'_>) -> Result<String, Refusal> {
    page(store, Answers::default(), Step::Details, Vec::new(), Flash::default()).await
}

/// A step's answers: onwards when they are right, the same step with what is wrong when they are
/// not, or back, or to the step a "Change" link names.
pub async fn answer(backend: &Backend, store: &Store<'_>, form: &Form) -> Result<String, Refusal> {
    let mut answers = Answers::of(form);
    answers.only_ticked();
    let at = Step::named(&field(form, "step")).unwrap_or(Step::Details);
    let position = STEPS.iter().position(|step| *step == at).unwrap_or(0);
    let go = field(form, "go");
    if go == "back" {
        let back = STEPS.get(position.saturating_sub(1)).copied().unwrap_or(Step::Details);
        return page(store, answers, back, Vec::new(), Flash::default()).await;
    }
    if let Some(step) = Step::named(&go) {
        return page(store, answers, step, Vec::new(), Flash::default()).await;
    }
    let problems = answers.problems(at);
    if !problems.is_empty() {
        return page(store, answers, at, problems, Flash::default()).await;
    }
    match STEPS.get(position + 1) {
        Some(next) => page(store, answers, *next, Vec::new(), Flash::default()).await,
        None => create(backend, store, answers).await,
    }
}

/// The last step taken: the group is made, then everyone named in it is added, and its own page
/// is shown. A member who cannot be found stops the whole thing, so nothing is half done.
async fn create(backend: &Backend, store: &Store<'_>, answers: Answers) -> Result<String, Refusal> {
    if let Some(step) = answers.first_unfinished() {
        let problems = answers.problems(step);
        return page(store, answers, step, problems, Flash::default()).await;
    }
    let kind = answers.kind();
    let named = answers.members();
    let mut members = Vec::with_capacity(named.len());
    for name in &named {
        match member_named(store, kind, name).await {
            Ok(holder) => members.push(holder),
            Err(refusal) => {
                let problems = vec![("members".to_string(), sentence(&refusal.detail))];
                return page(store, answers, Step::Members, problems, Flash::default()).await;
            }
        }
    }
    let home = answers.home().to_string();
    let made: Result<GroupRecord, Refusal> = ops::create_group(
        backend,
        store,
        &home,
        answers.get("name"),
        kind,
        answers.get("description"),
        &answers.permissions,
    )
    .await;
    let group = match made {
        Ok(group) => group,
        Err(refusal) => {
            let step = match refusal.status {
                409 => Step::Details,
                _ => Step::Permissions,
            };
            let problems = vec![("name".to_string(), sentence(&refusal.detail))];
            return page(store, answers, step, problems, Flash::default()).await;
        }
    };
    let membership = crate::model::membership(&group.plugin, &group.name);
    for holder in &members {
        ops::grant(backend, store, holder, &membership).await?;
    }
    let notice = match members.len() {
        0 => format!(
            "Created {}/{}. Add members below when you are ready.",
            group.plugin, group.name
        ),
        1 => format!("Created {}/{} with one member.", group.plugin, group.name),
        many => format!("Created {}/{} with {many} members.", group.plugin, group.name),
    };
    group_page(store, &group.plugin, &group.name, Flash::done(notice)).await
}

/// A group's plugins and permissions as its own page offers them: every plugin it grants in, and
/// any the viewer has asked to see as well, each with its permissions ticked or not.
pub async fn editing(
    store: &Store<'_>,
    group: &GroupRecord,
    also: &[String],
) -> Result<Vec<Offer>, Refusal> {
    let known = store.known().await?;
    let mut plugins = group.plugins();
    for plugin in also {
        if known.exists(plugin).is_ok() && !plugins.contains(plugin) {
            plugins.push(plugin.clone());
        }
    }
    Ok(plugins
        .iter()
        .map(|plugin| Offer {
            listed_under: *plugin == group.plugin,
            choices: ops::choices(&known, &[], plugin, group.kind, false)
                .into_iter()
                .map(|choice| {
                    let held = group.permissions.contains(&choice);
                    (choice, held)
                })
                .collect(),
            plugin: plugin.clone(),
        })
        .collect())
}
