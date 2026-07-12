//! Adding a source one question at a time: what kind it is, where its pages come from, which space
//! they fill, what they document and how often to sync, then a check of every answer. The answers
//! travel between steps in the form, so nothing is kept until the source is added. Markdown files
//! are uploaded at the last step and imported at once, into an upload source for their space.

use std::collections::BTreeMap;

use askama::Template;
use doc_plugin_sdk::{Backend, Request};
use serde_json::json;

use super::{Flash, Form, field, render, sentence, sources_page, writer};
use crate::Refusal;
use crate::api;
use crate::imports::{self, checked_resource, slug};
use crate::store::Store;

/// Every answer a step can give, carried by the steps after it. `owners` is the one that can be
/// answered more than once, and is carried as its values joined by commas.
const ANSWERS: [&str; 15] = [
    "kind",
    "repository",
    "ref",
    "path",
    "flavour",
    "url",
    "space_key",
    "credential",
    "folder",
    "space",
    "name",
    "owners",
    "resource",
    "schedule_choice",
    "schedule",
];

/// The kinds of source, with what the first step says of each.
pub const KINDS: [(&str, &str, &str); 5] = [
    (
        "github",
        "A GitHub repository",
        "Markdown or an MkDocs site in a repository, synced on a schedule. Public repositories need no credentials.",
    ),
    (
        "confluence",
        "A Confluence space",
        "Pages from Confluence Cloud or Data Center, read with a credential.",
    ),
    (
        "drive",
        "A Google Drive folder",
        "Docs, Markdown and text files in a folder or shared drive.",
    ),
    (
        "markdown",
        "Markdown files",
        "Files from your computer, imported once. Upload them again to update them.",
    ),
    (
        "git",
        "A git repository (.zip)",
        "A repository zipped on your computer, or downloaded as a zip from any git host: its Markdown, and the images and files it links to. Upload it again to update it.",
    ),
];

/// How often to sync, as offered; `custom` takes a cron schedule of the person's own.
pub const SCHEDULES: [(&str, &str, &str); 5] = [
    ("none", "Only when asked", ""),
    ("hourly", "Every hour", "0 * * * *"),
    ("daily", "Every day at 03:00 UTC", "0 3 * * *"),
    ("weekly", "Every Monday at 03:00 UTC", "0 3 * * 1"),
    ("custom", "A cron schedule of my own", ""),
];

/// Markdown uploaded from the browser must fit in what the frontend passes on.
const UPLOADABLE: [&str; 5] = [".md", ".markdown", ".txt", ".yml", ".yaml"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Kind,
    Details,
    Space,
    Owners,
    Extras,
    Check,
}

impl Step {
    pub fn name(self) -> &'static str {
        match self {
            Self::Kind => "kind",
            Self::Details => "details",
            Self::Space => "space",
            Self::Owners => "owners",
            Self::Extras => "extras",
            Self::Check => "check",
        }
    }

    fn named(name: &str) -> Option<Self> {
        [Self::Kind, Self::Details, Self::Space, Self::Owners, Self::Extras, Self::Check]
            .into_iter()
            .find(|step| step.name() == name)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Answers(BTreeMap<String, String>);

impl Answers {
    /// An answer given more than once — the owners, as checkboxes of one name — is held as one
    /// string, so every step can carry it hidden the way it carries the rest.
    fn of(form: &Form) -> Self {
        Self(
            ANSWERS
                .iter()
                .filter_map(|name| {
                    let given: Vec<String> = form
                        .iter()
                        .filter(|(key, _)| key == name)
                        .flat_map(|(_, value)| value.split(','))
                        .map(|value| value.trim().to_string())
                        .filter(|value| !value.is_empty())
                        .collect();
                    (!given.is_empty()).then(|| ((*name).to_string(), given.join(",")))
                })
                .collect(),
        )
    }

    /// Who the answers say looks after the space.
    pub fn owners(&self) -> Vec<String> {
        self.get("owners")
            .split(',')
            .filter(|owner| !owner.is_empty())
            .map(str::to_string)
            .collect()
    }

    pub fn get(&self, name: &str) -> &str {
        self.0.get(name).map_or("", String::as_str)
    }

    pub fn kind(&self) -> &str {
        self.get("kind")
    }

    /// Whether its pages are uploaded at the last step rather than read from somewhere.
    pub fn uploads(&self) -> bool {
        matches!(self.kind(), "markdown" | "git")
    }

    pub fn git(&self) -> bool {
        self.kind() == "git"
    }

    fn steps(&self) -> Vec<Step> {
        let details = (!self.uploads()).then_some(Step::Details);
        [
            Some(Step::Kind),
            details,
            Some(Step::Space),
            Some(Step::Owners),
            Some(Step::Extras),
            Some(Step::Check),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    /// The answers a step asks for, which its page shows as fields rather than carries hidden.
    fn asked_by(&self, step: Step) -> &'static [&'static str] {
        match (step, self.kind()) {
            (Step::Kind, _) => &["kind"],
            (Step::Details, "github") => &["repository", "ref", "path"],
            (Step::Details, "confluence") => &["flavour", "url", "space_key", "credential"],
            (Step::Details, "drive") => &["folder", "credential"],
            (Step::Space, _) => &["space", "name"],
            (Step::Owners, _) => &["owners"],
            (Step::Extras, _) => &["resource", "schedule_choice", "schedule"],
            _ => &[],
        }
    }

    pub fn schedule(&self) -> Option<String> {
        match self.get("schedule_choice") {
            "custom" => Some(self.get("schedule").to_string()).filter(|cron| !cron.is_empty()),
            choice => SCHEDULES
                .iter()
                .find(|(name, _, _)| *name == choice)
                .map(|(_, _, cron)| (*cron).to_string())
                .filter(|cron| !cron.is_empty()),
        }
    }

    pub fn kind_label(&self) -> &'static str {
        KINDS.iter().find(|(name, _, _)| *name == self.kind()).map_or("", |(_, label, _)| label)
    }

    pub fn schedule_label(&self) -> String {
        match self.get("schedule_choice") {
            "custom" => self.get("schedule").to_string(),
            choice => SCHEDULES
                .iter()
                .find(|(name, _, _)| *name == choice)
                .map_or("Only when asked", |(_, label, _)| label)
                .to_string(),
        }
    }

    /// What is wrong with a step's answers, field by field.
    fn problems(&self, step: Step) -> Vec<(String, String)> {
        let mut problems = Vec::new();
        let missing = |name: &str, message: &str| {
            self.get(name).is_empty().then(|| (name.to_string(), message.to_string()))
        };
        match step {
            Step::Kind => {
                if !KINDS.iter().any(|(name, _, _)| *name == self.kind()) {
                    problems.push(("kind".into(), "Choose what kind of source it is.".into()));
                }
            }
            Step::Details => match self.kind() {
                "github" => {
                    let repository = self.get("repository");
                    let parts: Vec<&str> = repository.split('/').collect();
                    let written = parts.len() == 2
                        && parts.iter().all(|part| !part.is_empty())
                        && !repository.contains(char::is_whitespace);
                    if !written {
                        problems.push((
                            "repository".into(),
                            "Write the repository as owner/name, such as acme/payments-docs."
                                .into(),
                        ));
                    }
                }
                "confluence" => {
                    if !matches!(self.get("flavour"), "cloud" | "datacenter") {
                        problems.push(("flavour".into(), "Choose Cloud or Data Center.".into()));
                    }
                    let url = self.get("url");
                    if !(url.starts_with("https://") || url.starts_with("http://")) {
                        problems.push((
                            "url".into(),
                            "Give the site's address, such as https://acme.atlassian.net.".into(),
                        ));
                    }
                    problems.extend(missing(
                        "space_key",
                        "Give the Confluence space's key, such as PAY.",
                    ));
                    problems.extend(missing("credential", "Name the credential it is read with."));
                }
                "drive" => {
                    problems.extend(missing("folder", "Give the folder or shared drive's ID."));
                    problems.extend(missing("credential", "Name the credential it is read with."));
                }
                _ => {}
            },
            Step::Space => {
                if slug(self.get("space")).is_empty() {
                    problems.push((
                        "space".into(),
                        "Name the space, such as payments-docs, using letters and numbers.".into(),
                    ));
                }
            }
            Step::Owners => {
                if let Err(refusal) = crate::owners::checked(&self.owners()) {
                    problems.push(("owners".into(), sentence(&refusal.detail)));
                } else if self.owners().is_empty() {
                    problems.push((
                        "owners".into(),
                        "Choose the team or teams who look after this space, or the organisation."
                            .into(),
                    ));
                }
            }
            Step::Extras => {
                let resource = self.get("resource");
                if !resource.is_empty()
                    && let Err(refusal) = checked_resource(resource)
                {
                    problems.push(("resource".into(), sentence(&refusal.detail)));
                }
                if !self.uploads() && self.get("schedule_choice") == "custom" {
                    match self.schedule() {
                        None => problems.push((
                            "schedule".into(),
                            "Write a cron schedule, such as 0 3 * * *.".into(),
                        )),
                        Some(cron) => {
                            if let Err(refusal) = api::checked_schedule(Some(cron)) {
                                problems.push(("schedule".into(), sentence(&refusal.detail)));
                            }
                        }
                    }
                }
            }
            Step::Check => {}
        }
        problems
    }

    /// The first step whose answers are missing or wrong, if any.
    fn first_unfinished(&self) -> Option<Step> {
        self.steps().into_iter().find(|step| !self.problems(*step).is_empty())
    }
}

/// One answer on the check page, and the step that changes it.
pub struct Checked {
    pub key: &'static str,
    pub value: String,
    pub step: &'static str,
}

#[derive(Template)]
#[template(path = "source_wizard.html")]
pub struct WizardPage {
    pub flash: Flash,
    pub writes: bool,
    pub step: Step,
    pub number: usize,
    pub of: usize,
    pub answers: Answers,
    pub problems: Vec<(String, String)>,
    /// Came from the check page, so continuing goes back there.
    pub checking: bool,
    pub spaces: Vec<(String, String)>,
    pub owner_choices: Vec<crate::owners::Choice>,
}

impl WizardPage {
    fn kinds(&self) -> &'static [(&'static str, &'static str, &'static str)] {
        &KINDS
    }

    fn schedules(&self) -> &'static [(&'static str, &'static str, &'static str)] {
        &SCHEDULES
    }

    fn at(&self, step: &str) -> bool {
        self.step.name() == step
    }

    fn problem(&self, name: &str) -> Option<&str> {
        self.problems.iter().find(|(field, _)| field == name).map(|(_, message)| message.as_str())
    }

    fn value(&self, name: &str) -> &str {
        self.answers.get(name)
    }

    fn chosen(&self, name: &str, value: &str) -> bool {
        self.answers.get(name) == value
    }

    /// Answers from other steps, carried as hidden fields.
    fn carried(&self) -> Vec<(&str, &str)> {
        let asked = self.answers.asked_by(self.step);
        self.answers
            .0
            .iter()
            .filter(|(name, _)| !asked.contains(&name.as_str()))
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect()
    }

    /// The owners as the check page reads them: their names, not their `kind:name`.
    fn owner_names(&self) -> String {
        self.answers
            .owners()
            .iter()
            .map(|owner| owner.split_once(':').map_or(owner.clone(), |(_, name)| name.to_string()))
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn owners_problem(&self) -> Option<&str> {
        self.problem("owners")
    }

    fn checked(&self) -> Vec<Checked> {
        let answers = &self.answers;
        let row = |key, value: &str, step: Step| Checked {
            key,
            value: if value.is_empty() { "None".to_string() } else { value.to_string() },
            step: step.name(),
        };
        let mut rows = vec![row("Kind", answers.kind_label(), Step::Kind)];
        match answers.kind() {
            "github" => {
                rows.push(row("Repository", answers.get("repository"), Step::Details));
                let reference = match answers.get("ref") {
                    "" => "HEAD",
                    reference => reference,
                };
                rows.push(row("Branch, tag or commit", reference, Step::Details));
                rows.push(row("Folder", answers.get("path"), Step::Details));
            }
            "confluence" => {
                let flavour = match answers.get("flavour") {
                    "datacenter" => "Data Center",
                    _ => "Cloud",
                };
                rows.push(row("Confluence", flavour, Step::Details));
                rows.push(row("Site", answers.get("url"), Step::Details));
                rows.push(row("Space key", answers.get("space_key"), Step::Details));
                rows.push(row("Credential", answers.get("credential"), Step::Details));
            }
            "drive" => {
                rows.push(row("Folder or shared drive", answers.get("folder"), Step::Details));
                rows.push(row("Credential", answers.get("credential"), Step::Details));
            }
            _ => {}
        }
        rows.push(row("Space", &slug(answers.get("space")), Step::Space));
        if !answers.get("name").is_empty() {
            rows.push(row("Space name", answers.get("name"), Step::Space));
        }
        rows.push(row("Looked after by", &self.owner_names(), Step::Owners));
        rows.push(row("Documents", answers.get("resource"), Step::Extras));
        if !answers.uploads() {
            rows.push(row("Syncs", &answers.schedule_label(), Step::Extras));
        }
        rows
    }
}

async fn page(
    backend: &Backend,
    store: &Store<'_>,
    mut answers: Answers,
    step: Step,
    problems: Vec<(String, String)>,
    checking: bool,
    flash: Flash,
) -> Result<String, Refusal> {
    let steps = answers.steps();
    let number = steps.iter().position(|each| *each == step).map_or(1, |at| at + 1);
    let spaces = match step {
        Step::Space => {
            store.spaces().await?.into_iter().map(|space| (space.key, space.name)).collect()
        }
        _ => Vec::new(),
    };
    let owner_choices = match step {
        Step::Owners => {
            // A space that already exists arrives with whoever keeps it ticked, so adding a second
            // source to it is one click rather than the same answer given again.
            if answers.owners().is_empty()
                && let Some(held) = store.space(&slug(answers.get("space"))).await?
                && !held.owners.is_empty()
            {
                answers.0.insert("owners".into(), held.owners.join(","));
            }
            crate::owners::choices(backend, &answers.owners()).await
        }
        _ => Vec::new(),
    };
    let of = steps.len();
    render(&WizardPage {
        flash,
        writes: true,
        step,
        number,
        of,
        answers,
        problems,
        checking,
        spaces,
        owner_choices,
    })
}

/// The first step, with nothing answered yet.
pub async fn start(backend: &Backend, store: &Store<'_>) -> Result<String, Refusal> {
    writer(backend)?;
    page(backend, store, Answers::default(), Step::Kind, Vec::new(), false, Flash::default()).await
}

/// A step's answers: onwards when they are right, the same step with what is wrong when not, or
/// back, or to the step a "Change" link names.
pub async fn answer(backend: &Backend, store: &Store<'_>, form: &Form) -> Result<String, Refusal> {
    writer(backend)?;
    let answers = Answers::of(form);
    let at = field(form, "step").and_then(|name| Step::named(&name)).unwrap_or(Step::Kind);
    let checking = field(form, "checking").is_some();
    let steps = answers.steps();
    let position = steps.iter().position(|step| *step == at).unwrap_or(0);
    match field(form, "go").as_deref().unwrap_or("next") {
        "back" => {
            let back = match checking {
                true => Step::Check,
                false => steps[position.saturating_sub(1)],
            };
            page(backend, store, answers, back, Vec::new(), false, Flash::default()).await
        }
        "add" => add(backend, store, answers).await,
        "next" => {
            let problems = answers.problems(at);
            if !problems.is_empty() {
                return page(backend, store, answers, at, problems, checking, Flash::default())
                    .await;
            }
            let next = match checking {
                true => answers.first_unfinished().unwrap_or(Step::Check),
                false => steps.get(position + 1).copied().unwrap_or(Step::Check),
            };
            // A changed kind can leave the next step unanswered, which it then asks for.
            let checking = checking && next == Step::Check;
            page(backend, store, answers, next, Vec::new(), checking, Flash::default()).await
        }
        change => {
            let step = Step::named(change).unwrap_or(Step::Check);
            page(backend, store, answers, step, Vec::new(), true, Flash::default()).await
        }
    }
}

async fn add(backend: &Backend, store: &Store<'_>, answers: Answers) -> Result<String, Refusal> {
    if let Some(step) = answers.first_unfinished() {
        let problems = answers.problems(step);
        return page(backend, store, answers, step, problems, true, Flash::default()).await;
    }
    let mut asked = json!({ "kind": answers.kind(), "space": answers.get("space") });
    asked["owners"] = json!(answers.owners());
    for name in [
        "name",
        "resource",
        "repository",
        "ref",
        "path",
        "flavour",
        "url",
        "space_key",
        "credential",
        "folder",
    ] {
        let value = answers.get(name);
        if !value.is_empty() {
            asked[name] = json!(value);
        }
    }
    if let Some(schedule) = answers.schedule() {
        asked["schedule"] = json!(schedule);
    }
    match api::add_source(backend, store, asked).await {
        Ok(made) => {
            let notice = format!(
                "Added {} for {}. Sync it now, or leave it to its schedule.",
                answers.kind_label().to_lowercase(),
                made["space"].as_str().unwrap_or_default()
            );
            sources_page(backend, store, Flash::done(notice)).await
        }
        Err(refusal) => {
            page(backend, store, answers, Step::Check, Vec::new(), false, Flash::refused(&refusal))
                .await
        }
    }
}

/// The last step for Markdown: the answers and the files, sent together as `multipart/form-data`.
pub async fn upload(
    backend: &Backend,
    store: &Store<'_>,
    request: &Request,
) -> Result<String, Refusal> {
    writer(backend)?;
    let content_type = request.headers.get("content-type").map_or("", String::as_str);
    let parts = multipart::parse(content_type, &request.body).map_err(Refusal::bad)?;
    let form: Form = parts
        .iter()
        .filter(|part| part.filename.is_none())
        .map(|part| (part.name.clone(), String::from_utf8_lossy(&part.body).into_owned()))
        .collect();
    let answers = Answers::of(&form);
    if let Some(step) = answers.first_unfinished() {
        let problems = answers.problems(step);
        return page(backend, store, answers, step, problems, true, Flash::default()).await;
    }
    if answers.git() {
        return git_upload(backend, store, answers, parts).await;
    }
    let files: BTreeMap<String, Vec<u8>> = parts
        .into_iter()
        .filter(|part| part.name == "files")
        .filter_map(|part| {
            let name = part.filename?;
            let name = name.rsplit(['/', '\\']).next().unwrap_or_default().to_string();
            let lower = name.to_lowercase();
            let wanted = !name.is_empty() && UPLOADABLE.iter().any(|end| lower.ends_with(end));
            wanted.then_some((name, part.body))
        })
        .collect();
    if files.is_empty() {
        let problems = vec![(
            "files".to_string(),
            "Choose at least one Markdown file (.md or .markdown).".to_string(),
        )];
        return page(backend, store, answers, Step::Check, problems, false, Flash::default()).await;
    }
    let space = slug(answers.get("space"));
    let resource = Some(answers.get("resource")).filter(|resource| !resource.is_empty());
    let owners = answers.owners();
    match imports::start(backend, store, &files, Some(&space), resource, &owners, None).await {
        Ok(started) => {
            let notice = format!(
                "Importing {} pages into {}. They appear in the space as soon as they are read.",
                started.pages, started.space.key
            );
            sources_page(backend, store, Flash::done(notice)).await
        }
        Err(refusal) => {
            page(backend, store, answers, Step::Check, Vec::new(), false, Flash::refused(&refusal))
                .await
        }
    }
}

/// A git repository's zip, imported into its space through the space's source for it.
async fn git_upload(
    backend: &Backend,
    store: &Store<'_>,
    answers: Answers,
    parts: Vec<multipart::Part>,
) -> Result<String, Refusal> {
    let zipped = parts.into_iter().find(|part| {
        part.name == "repository"
            && part.filename.as_deref().is_some_and(|name| name.to_lowercase().ends_with(".zip"))
    });
    let problem = |said: &str| vec![("repository".to_string(), said.to_string())];
    let Some(zipped) = zipped else {
        let problems = problem("Choose the repository's .zip.");
        return page(backend, store, answers, Step::Check, problems, false, Flash::default()).await;
    };
    let files = match crate::archive::zipped(&zipped.body) {
        Ok(files) => files,
        Err(said) => {
            let problems = problem(&sentence(&said));
            return page(backend, store, answers, Step::Check, problems, false, Flash::default())
                .await;
        }
    };
    let space = slug(answers.get("space"));
    let resource = Some(answers.get("resource")).filter(|resource| !resource.is_empty());
    match crate::git::import(backend, store, files, &space, resource, &answers.owners()).await {
        Ok(started) => {
            let notice = format!(
                "Importing {} pages from {} into {}. They appear in the space as soon as they are \
                 read.",
                started.pages, started.repository, started.space
            );
            sources_page(backend, store, Flash::done(notice)).await
        }
        Err(refusal) => {
            page(backend, store, answers, Step::Check, Vec::new(), false, Flash::refused(&refusal))
                .await
        }
    }
}

/// Just enough of `multipart/form-data` for a form's fields and files.
mod multipart {
    pub struct Part {
        pub name: String,
        pub filename: Option<String>,
        pub body: Vec<u8>,
    }

    fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
        haystack
            .get(from..)?
            .windows(needle.len())
            .position(|window| window == needle)
            .map(|at| at + from)
    }

    fn attribute(disposition: &str, name: &str) -> Option<String> {
        let marker = format!("{name}=\"");
        let start = disposition.find(&marker)? + marker.len();
        let end = disposition[start..].find('"')?;
        Some(disposition[start..start + end].to_string())
    }

    pub fn parse(content_type: &str, body: &[u8]) -> Result<Vec<Part>, String> {
        let boundary = content_type
            .split(';')
            .map(str::trim)
            .find_map(|param| param.strip_prefix("boundary="))
            .map(|boundary| boundary.trim_matches('"'))
            .filter(|boundary| {
                content_type.starts_with("multipart/form-data") && !boundary.is_empty()
            })
            .ok_or("the files were not sent as a form upload")?;
        let delimiter = format!("--{boundary}").into_bytes();
        let mut parts = Vec::new();
        let mut at = find(body, &delimiter, 0).ok_or("the upload is empty")? + delimiter.len();
        loop {
            if body.get(at..at + 2) == Some(b"--") {
                return Ok(parts);
            }
            let headers_start = at + 2;
            let headers_end = find(body, b"\r\n\r\n", headers_start)
                .ok_or("an upload part has no end to its headers")?;
            let headers = String::from_utf8_lossy(&body[headers_start..headers_end]);
            let content_start = headers_end + 4;
            let next = find(body, &delimiter, content_start).ok_or("the upload was cut short")?;
            let content_end = next
                .checked_sub(2)
                .filter(|end| *end >= content_start)
                .ok_or("an upload part is malformed")?;
            let disposition = headers
                .lines()
                .find(|line| line.to_ascii_lowercase().starts_with("content-disposition:"))
                .unwrap_or_default();
            if let Some(name) = attribute(disposition, "name") {
                parts.push(Part {
                    name,
                    filename: attribute(disposition, "filename").filter(|name| !name.is_empty()),
                    body: body[content_start..content_end].to_vec(),
                });
            }
            at = next + delimiter.len();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(pairs: &[(&str, &str)]) -> Form {
        pairs.iter().map(|(name, value)| ((*name).to_string(), (*value).to_string())).collect()
    }

    #[test]
    fn markdown_skips_the_step_that_asks_where_pages_come_from() {
        let github = Answers::of(&form(&[("kind", "github")]));
        assert_eq!(
            github.steps(),
            [Step::Kind, Step::Details, Step::Space, Step::Owners, Step::Extras, Step::Check]
        );
        let markdown = Answers::of(&form(&[("kind", "markdown")]));
        assert_eq!(
            markdown.steps(),
            [Step::Kind, Step::Space, Step::Owners, Step::Extras, Step::Check]
        );
    }

    #[test]
    fn each_step_says_what_is_wrong_with_its_answers() {
        let answers = Answers::of(&form(&[("kind", "github"), ("repository", "just-a-name")]));
        assert_eq!(answers.problems(Step::Details)[0].0, "repository");
        assert_eq!(answers.first_unfinished(), Some(Step::Details));

        let answers =
            Answers::of(&form(&[("kind", "confluence"), ("flavour", "cloud"), ("url", "acme")]));
        let wrong: Vec<String> =
            answers.problems(Step::Details).into_iter().map(|(name, _)| name).collect();
        assert_eq!(wrong, ["url", "space_key", "credential"]);

        let answers = Answers::of(&form(&[
            ("kind", "drive"),
            ("folder", "abc"),
            ("credential", "drive"),
            ("space", "Payments docs"),
            ("schedule_choice", "custom"),
            ("schedule", "not cron"),
        ]));
        assert!(answers.problems(Step::Space).is_empty(), "a space's key is made from its name");
        assert_eq!(answers.problems(Step::Extras)[0].0, "schedule");
    }

    #[test]
    fn a_schedule_is_a_preset_or_ones_own() {
        let preset = Answers::of(&form(&[("schedule_choice", "daily"), ("schedule", "ignored")]));
        assert_eq!(preset.schedule().as_deref(), Some("0 3 * * *"));
        let none = Answers::of(&form(&[("schedule_choice", "none")]));
        assert_eq!(none.schedule(), None);
        let own = Answers::of(&form(&[("schedule_choice", "custom"), ("schedule", "*/5 * * * *")]));
        assert_eq!(own.schedule().as_deref(), Some("*/5 * * * *"));
    }

    #[test]
    fn a_step_carries_every_other_answer_hidden_and_shows_its_own() {
        let answers = Answers::of(&form(&[
            ("kind", "github"),
            ("repository", "acme/docs"),
            ("space", "docs"),
        ]));
        let page = WizardPage {
            flash: Flash::default(),
            writes: true,
            step: Step::Details,
            number: 2,
            of: 6,
            answers,
            problems: vec![("repository".into(), "Write it as owner/name.".into())],
            checking: false,
            spaces: Vec::new(),
            owner_choices: Vec::new(),
        };
        let carried: Vec<&str> = page.carried().into_iter().map(|(name, _)| name).collect();
        assert_eq!(carried, ["kind", "space"]);
        let html = page.render().expect("rendered");
        assert!(html.contains("Step 2 of 6"));
        assert!(html.contains(r#"name="repository""#) && html.contains(r#"value="acme/docs""#));
        assert!(html.contains("Write it as owner/name."));
    }

    #[test]
    fn an_upload_is_read_into_fields_and_files() {
        let body = b"--XyZ\r\nContent-Disposition: form-data; name=\"space\"\r\n\r\ndocs\r\n\
--XyZ\r\nContent-Disposition: form-data; name=\"files\"; filename=\"guide.md\"\r\nContent-Type: text/markdown\r\n\r\n# Guide\r\n\r\nHello\r\n\
--XyZ\r\nContent-Disposition: form-data; name=\"files\"; filename=\"\"\r\n\r\n\r\n\
--XyZ--\r\n";
        let parts = multipart::parse("multipart/form-data; boundary=XyZ", body).expect("parsed");
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].name, "space");
        assert_eq!(parts[0].body, b"docs");
        assert_eq!(parts[1].filename.as_deref(), Some("guide.md"));
        assert_eq!(parts[1].body, b"# Guide\r\n\r\nHello");
        assert_eq!(parts[2].filename, None, "an empty file field is no file");
        assert!(multipart::parse("application/x-www-form-urlencoded", body).is_err());
    }
}
