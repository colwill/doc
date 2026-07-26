//! What a software template is: what it asks for, what it is made of, and what it does with the
//! answers. A definition is a YAML or JSON document, checked in full before it is stored, so a
//! template that is saved is one that can run.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::render;

pub const MAX_NAME: usize = 64;
pub const MAX_TITLE: usize = 120;
pub const MAX_DESCRIPTION: usize = 2_000;
pub const MAX_PARAMETERS: usize = 40;
pub const MAX_STEPS: usize = 20;
pub const MAX_FILES: usize = 400;
pub const MAX_FILE_BYTES: usize = 512 * 1024;
pub const MAX_TOTAL_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_TAGS: usize = 12;

/// What a parameter asks for. The plugin draws the field and checks the answer; `team` and
/// `resource` are the platform's own pickers rather than something to remember and type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ParameterKind {
    #[default]
    Text,
    Textarea,
    Number,
    Boolean,
    Select,
    /// Several values, one to a line, which `{% for %}` then goes round.
    List,
    Team,
    Resource,
}

/// One of a `select`'s choices: `private`, or `{ value: private, title: Private }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Choice {
    Value(String),
    Titled { value: String, title: String },
}

impl Choice {
    pub fn value(&self) -> &str {
        match self {
            Self::Value(value) => value,
            Self::Titled { value, .. } => value,
        }
    }

    pub fn title(&self) -> &str {
        match self {
            Self::Value(value) => value,
            Self::Titled { title, .. } => title,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Parameter {
    pub name: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub hint: Option<String>,
    #[serde(default)]
    pub kind: ParameterKind,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: Option<Value>,
    #[serde(default)]
    pub options: Vec<Choice>,
    /// A regular expression the answer must match, for a `text` parameter.
    #[serde(default)]
    pub pattern: Option<String>,
    /// What to say when the pattern does not match, since a regular expression is not an answer.
    #[serde(default)]
    pub expects: Option<String>,
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    #[serde(default)]
    pub placeholder: Option<String>,
    /// The page of the form this parameter is asked on; parameters with no group share the first.
    #[serde(default)]
    pub group: Option<String>,
    /// For a `resource` parameter: the catalogue kinds it takes, such as `Service,Team`.
    #[serde(default)]
    pub kinds: Vec<String>,
}

impl Parameter {
    pub fn label(&self) -> String {
        self.title.clone().unwrap_or_else(|| titled(&self.name))
    }

    pub fn page(&self) -> String {
        self.group.clone().unwrap_or_else(|| "About it".to_string())
    }
}

/// `payments-api` becomes `Payments api`, which is a better label than the name itself.
fn titled(name: &str) -> String {
    let words = name.replace(['-', '_'], " ");
    let mut letters = words.chars();
    match letters.next() {
        Some(first) => first.to_uppercase().collect::<String>() + letters.as_str(),
        None => String::new(),
    }
}

/// How far along the thing a template creates is. Everything created here has one, so a platform
/// can tell what may be depended on from what somebody is still finding out about. It is the
/// creation's own lifecycle and not the run's `state`, which is how the run itself went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Lifecycle {
    #[default]
    Experiment,
    Design,
    Production,
}

/// What the form calls the question, and what a template settles it with.
pub const LIFECYCLE: &str = "lifecycle";

impl Lifecycle {
    pub const ALL: [Self; 3] = [Self::Experiment, Self::Design, Self::Production];

    pub fn name(self) -> &'static str {
        match self {
            Self::Experiment => "experiment",
            Self::Design => "design",
            Self::Production => "production",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::Experiment => "Experiment",
            Self::Design => "Design",
            Self::Production => "Production ready",
        }
    }

    pub fn about(self) -> &'static str {
        match self {
            Self::Experiment => {
                "Somebody is finding out whether this is worth having. Nothing should depend on \
                 it, and it may be thrown away."
            }
            Self::Design => {
                "It is being designed and built. It is not ready to be depended on yet, and what \
                 it does may still change."
            }
            Self::Production => {
                "It is looked after and supported, and other things may depend on it."
            }
        }
    }

    /// The badge it is shown with: green for what may be depended on, amber for what may not.
    pub fn badge(self) -> &'static str {
        match self {
            Self::Experiment => "degraded",
            Self::Design => "loading",
            Self::Production => "ready",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|lifecycle| lifecycle.name() == text.trim())
    }

    /// What a template renders it with: `{{ lifecycle.title }}`, `{% if lifecycle.production %}`.
    pub fn context(self) -> Value {
        json!({
            "name": self.name(),
            "title": self.title(),
            "about": self.about(),
            "production": self == Self::Production,
        })
    }

    /// The question the form asks when a template does not settle it itself.
    pub fn question() -> Parameter {
        Parameter {
            name: LIFECYCLE.to_string(),
            title: Some("What state it is in".to_string()),
            hint: Some(
                "An experiment is somebody finding out whether it is worth having; a design is \
                 being built; something production ready is supported and may be depended on. It \
                 says so in the Catalogue and in its own README, and it can be changed later."
                    .to_string(),
            ),
            kind: ParameterKind::Select,
            required: true,
            default: Some(json!(Self::Experiment.name())),
            options: Self::ALL
                .into_iter()
                .map(|lifecycle| Choice::Titled {
                    value: lifecycle.name().to_string(),
                    title: lifecycle.title().to_string(),
                })
                .collect(),
            ..Parameter::default()
        }
    }
}

/// A file the template carries itself, rather than one it fetches from a repository.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct File {
    pub path: String,
    #[serde(default)]
    pub content: String,
}

/// One of the plugin's own scaffolds: a language and an application type. Both may be answers, so
/// one template can offer every pair.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scaffold {
    pub language: String,
    pub app: String,
}

/// A skeleton read from a repository, which the GitHub plugin gives a short-lived archive link to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Source {
    /// `owner/repository`.
    pub repository: String,
    #[serde(default, rename = "ref")]
    pub reference: Option<String>,
    /// Only what is under this path, as if it were the whole repository.
    #[serde(default)]
    pub path: Option<String>,
}

/// What a step does: `action:` names it and `with:` holds its settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", content = "with", rename_all = "kebab-case")]
pub enum Action {
    /// Creates a repository and commits the rendered files to it.
    Publish {
        #[serde(default)]
        owner: Option<String>,
        repository: String,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        visibility: Option<String>,
        #[serde(default)]
        branch: Option<String>,
        #[serde(default)]
        message: Option<String>,
    },
    /// Applies a document to the Catalogue, as whoever launched the run.
    Register {
        kind: String,
        name: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        owner: Option<String>,
        #[serde(default)]
        connections: Vec<String>,
        #[serde(default)]
        metadata: Map<String, Value>,
    },
    /// Another plugin's `api/` route, asked as whoever launched the run.
    Request {
        plugin: String,
        #[serde(default = "post")]
        method: String,
        route: String,
        #[serde(default)]
        query: Option<String>,
        #[serde(default)]
        body: Option<Value>,
    },
    /// Something in somebody's DOC inbox; the requester's, unless it names someone else.
    Notify {
        #[serde(default)]
        user: Option<String>,
        title: String,
        #[serde(default)]
        body: Option<String>,
        #[serde(default)]
        url: Option<String>,
    },
    /// Infrastructure for what the run is standing up, asked of the Infra plugin.
    ///
    /// Infra provisions in the background: this step asks and the run carries on, so nothing here
    /// knows the machine's address. Naming the service it is for is what closes that — the DNS
    /// plugin hears Infra say the machine is answering and points the service's name at it then,
    /// which is minutes after this run has finished.
    Infra {
        /// The Infra template it is asked from, such as `dev-vm`.
        template: String,
        /// The team it counts against, and whose members may tear it down.
        team: String,
        /// The service it belongs to, which is what its name in DNS follows. Left out, the
        /// resource is the team's alone and nothing is named after it.
        #[serde(default)]
        service: Option<String>,
        /// What to call it; the service's name when it is left out.
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        region: Option<String>,
        #[serde(default)]
        size: Option<String>,
        /// How long it lives, as `30m`, `12h` or `7d`; the template's usual when left out.
        #[serde(default)]
        lifetime: Option<String>,
    },
    /// A name for what the run stood up, in the DNS plugin's own records.
    ///
    /// A service nobody can reach by name is a service nobody uses, and the address of whatever
    /// was provisioned for it is not something anybody should be copying by hand. The `value` is
    /// usually a step's output — the address of the instance `infra` stood up — and the kind of
    /// record follows from it: an IP address is an `A` or `AAAA`, a name is a `CNAME`.
    Dns {
        /// The full name, such as `payments.internal.example.com`. DOC must answer for it.
        name: String,
        /// What it points at: an address or another name.
        value: String,
        /// `A`, `AAAA` or `CNAME`; worked out from the value when it is not given.
        #[serde(default)]
        kind: Option<String>,
        #[serde(default)]
        ttl: Option<u32>,
        #[serde(default)]
        note: Option<String>,
    },
    /// An event under `plugin.templates.`, for automations and other plugins to listen for.
    Event {
        topic: String,
        #[serde(default)]
        payload: Option<Value>,
    },
}

fn post() -> String {
    "POST".into()
}

impl Action {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Publish { .. } => "publish",
            Self::Register { .. } => "register",
            Self::Request { .. } => "request",
            Self::Notify { .. } => "notify",
            Self::Infra { .. } => "infra",
            Self::Dns { .. } => "dns",
            Self::Event { .. } => "event",
        }
    }

    /// What the step says it will do, before it does it. With the answers to hand it reads
    /// `Create acme/payments-api`; without them, or where a value comes from a step that has not
    /// run, it says what it can rather than repeating the template back.
    pub fn summary(&self, context: Option<&Value>) -> String {
        let filled = |source: &str| -> Option<String> {
            let text = match context {
                Some(context) => render::line(source, context).ok()?,
                None if source.contains("{{") => return None,
                None => source.to_string(),
            };
            (!text.trim().is_empty()).then_some(text)
        };
        match self {
            Self::Publish { owner, repository, .. } => {
                match (owner.as_deref().and_then(&filled), filled(repository)) {
                    (Some(owner), Some(repository)) => {
                        format!("Create {owner}/{repository} and commit the files to it")
                    }
                    (None, Some(repository)) => {
                        format!("Create the repository {repository} and commit the files to it")
                    }
                    (Some(owner), None) => {
                        format!("Create a repository in {owner} and commit the files to it")
                    }
                    (None, None) => "Create the repository and commit the files to it".to_string(),
                }
            }
            Self::Register { kind, name, .. } => match (filled(kind), filled(name)) {
                (Some(kind), Some(name)) => format!("Put {kind}:{name} in the Catalogue"),
                (Some(kind), None) => format!("Put the {kind} in the Catalogue"),
                _ => "Put it in the Catalogue".to_string(),
            },
            Self::Request { plugin, method, route, .. } => {
                let route = filled(route).unwrap_or_else(|| route.clone());
                format!("{} {route} on {plugin}", method.to_uppercase())
            }
            Self::Notify { user, .. } => match user.as_deref().and_then(&filled) {
                Some(user) => format!("Tell {user}"),
                None => "Tell whoever launched it".to_string(),
            },
            Self::Infra { template, service, .. } => {
                match (filled(template), service.as_deref().and_then(&filled)) {
                    (Some(template), Some(service)) => {
                        format!("Ask Infra for {template} for {service}")
                    }
                    (Some(template), None) => format!("Ask Infra for {template}"),
                    _ => "Ask Infra for the infrastructure".to_string(),
                }
            }
            Self::Dns { name, value, .. } => match (filled(name), filled(value)) {
                (Some(name), Some(value)) => format!("Point {name} at {value} in DNS"),
                (Some(name), None) => format!("Add {name} to DNS"),
                _ => "Add a name for it to DNS".to_string(),
            },
            Self::Event { topic, .. } => format!("Publish plugin.templates.{topic}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    /// Rendered before the step runs; anything that comes out false skips it.
    #[serde(default, rename = "if")]
    pub when: Option<String>,
    #[serde(flatten)]
    pub action: Action,
}

impl Step {
    pub fn label(&self) -> String {
        self.title.clone().unwrap_or_else(|| titled(&self.id))
    }
}

/// A link the run page offers once everything has run, such as the repository it made.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Link {
    pub title: String,
    pub url: String,
}

/// The whole of a template, as it is written and as it is stored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Definition {
    pub name: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: String,
    /// What it creates, as the cards say it: `Service`, `Library`, `Documentation`.
    #[serde(default)]
    pub kind: Option<String>,
    /// The lifecycle everything this template creates starts in. A template that leaves it out
    /// has the form ask, which is what most of them should do.
    #[serde(default)]
    pub lifecycle: Option<Lifecycle>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// The team that looks after the template, by name.
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub parameters: Vec<Parameter>,
    #[serde(default)]
    pub scaffold: Option<Scaffold>,
    #[serde(default)]
    pub source: Option<Source>,
    #[serde(default)]
    pub files: Vec<File>,
    #[serde(default)]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub links: Vec<Link>,
}

impl Definition {
    pub fn label(&self) -> String {
        self.title.clone().unwrap_or_else(|| titled(&self.name))
    }

    /// Everything the form asks: what the template asks for, and — unless the template settles it
    /// itself — what state the thing being created is in.
    pub fn asked(&self) -> Vec<Parameter> {
        let mut asked = self.parameters.clone();
        if self.lifecycle.is_none() {
            asked.push(Lifecycle::question());
        }
        asked
    }

    /// The lifecycle of what a run creates: the one the template settles on, or the answer.
    pub fn lifecycle_of(&self, values: &Value) -> Lifecycle {
        self.lifecycle
            .or_else(|| values.get(LIFECYCLE).and_then(Value::as_str).and_then(Lifecycle::parse))
            .unwrap_or_default()
    }

    /// The pages the form is asked over, in the order the parameters name them.
    pub fn pages(&self) -> Vec<String> {
        let mut pages: Vec<String> = Vec::new();
        for parameter in self.asked() {
            let page = parameter.page();
            if !pages.contains(&page) {
                pages.push(page);
            }
        }
        pages
    }

    pub fn on_page(&self, page: &str) -> Vec<Parameter> {
        self.asked().into_iter().filter(|parameter| parameter.page() == page).collect()
    }

    pub fn parameter(&self, name: &str) -> Option<Parameter> {
        self.asked().into_iter().find(|parameter| parameter.name == name)
    }

    /// Everything wrong with the definition, or nothing. Checked before it is ever stored, because
    /// a template that cannot run is worse than one that is not there.
    pub fn check(&self) -> Result<(), String> {
        slug(&self.name, "a template's name")?;
        if self.name.len() > MAX_NAME {
            return Err(format!("a template's name is at most {MAX_NAME} characters"));
        }
        if self.label().trim().is_empty() || self.label().len() > MAX_TITLE {
            return Err(format!("a template's title is 1 to {MAX_TITLE} characters"));
        }
        if self.description.len() > MAX_DESCRIPTION {
            return Err(format!(
                "a template's description is at most {MAX_DESCRIPTION} characters"
            ));
        }
        if self.tags.len() > MAX_TAGS {
            return Err(format!("a template has at most {MAX_TAGS} tags"));
        }
        if self.asked().len() > MAX_PARAMETERS {
            return Err(format!("a template asks at most {MAX_PARAMETERS} things"));
        }
        if self.steps.is_empty() {
            return Err("a template with no steps would do nothing".into());
        }
        if self.steps.len() > MAX_STEPS {
            return Err(format!("a template takes at most {MAX_STEPS} steps"));
        }
        let mut names = BTreeSet::new();
        for parameter in &self.parameters {
            slug(&parameter.name, "a parameter's name")?;
            if parameter.name == LIFECYCLE {
                return Err(format!(
                    "`{LIFECYCLE}` is the platform's own question: write `lifecycle: experiment` \
                     on the template to settle it, rather than asking it again"
                ));
            }
            if !names.insert(parameter.name.clone()) {
                return Err(format!("`{}` is asked for twice", parameter.name));
            }
            if parameter.kind == ParameterKind::Select && parameter.options.is_empty() {
                return Err(format!("`{}` is a select with no options", parameter.name));
            }
            if let Some(pattern) = &parameter.pattern
                && let Err(err) = regex::Regex::new(pattern)
            {
                return Err(format!("`{}` has a pattern that is not one: {err}", parameter.name));
            }
        }
        let mut ids = BTreeSet::new();
        for step in &self.steps {
            slug(&step.id, "a step's id")?;
            if !ids.insert(step.id.clone()) {
                return Err(format!("there are two steps called `{}`", step.id));
            }
            if let Some(when) = &step.when {
                render::check(when).map_err(|err| format!("step `{}`: {err}", step.id))?;
            }
            check_action(&step.action).map_err(|err| format!("step `{}`: {err}", step.id))?;
        }
        if self.files.len() > MAX_FILES {
            return Err(format!("a template carries at most {MAX_FILES} files"));
        }
        let mut total = 0usize;
        for file in &self.files {
            safe_path(&file.path)?;
            render::check(&file.path).map_err(|err| format!("the path `{}`: {err}", file.path))?;
            render::check(&file.content).map_err(|err| format!("`{}`: {err}", file.path))?;
            if file.content.len() > MAX_FILE_BYTES {
                return Err(format!(
                    "`{}` is larger than {} KiB",
                    file.path,
                    MAX_FILE_BYTES / 1024
                ));
            }
            total += file.content.len();
        }
        if total > MAX_TOTAL_BYTES {
            return Err(format!(
                "a template's files come to more than {} MiB",
                MAX_TOTAL_BYTES / 1024 / 1024
            ));
        }
        if let Some(source) = &self.source
            && source.repository.split('/').count() != 2
        {
            return Err("a source repository is written `owner/repository`".into());
        }
        if let Some(scaffold) = &self.scaffold {
            render::check(&scaffold.language).map_err(|err| format!("the language: {err}"))?;
            render::check(&scaffold.app).map_err(|err| format!("the application type: {err}"))?;
            // A pair that is answered is checked when the run renders it; one written into the
            // template is checked now, so a typo is refused rather than found by whoever runs it.
            let settled = |source: &str| !source.contains("{{");
            if settled(&scaffold.language)
                && settled(&scaffold.app)
                && !crate::scaffolds::holds(&scaffold.language, &scaffold.app)
            {
                return Err(format!(
                    "there is no {} scaffold for {}: the languages are {}, and the applications are {}",
                    scaffold.app,
                    scaffold.language,
                    crate::scaffolds::LANGUAGES
                        .iter()
                        .map(|language| language.name)
                        .collect::<Vec<_>>()
                        .join(", "),
                    crate::scaffolds::APPS
                        .iter()
                        .map(|app| app.name)
                        .collect::<Vec<_>>()
                        .join(", "),
                ));
            }
        }
        if self.source.is_none()
            && self.scaffold.is_none()
            && self.files.is_empty()
            && self.publishes()
        {
            return Err(
                "a template that publishes a repository needs a scaffold, files or a source".into(),
            );
        }
        for link in &self.links {
            render::check(&link.url).map_err(|err| format!("the link `{}`: {err}", link.title))?;
        }
        Ok(())
    }

    pub fn publishes(&self) -> bool {
        self.steps.iter().any(|step| matches!(step.action, Action::Publish { .. }))
    }

    /// Whether it writes any files, which is what a run offers to download.
    pub fn makes_files(&self) -> bool {
        self.scaffold.is_some() || self.source.is_some() || !self.files.is_empty()
    }
}

/// Every templated string in an action parses, and what it needs is there.
fn check_action(action: &Action) -> Result<(), String> {
    let templated = |source: &str| render::check(source);
    match action {
        Action::Dns { name, value, kind, ttl, note } => {
            if name.trim().is_empty() {
                return Err("a dns step names the record to add".into());
            }
            if value.trim().is_empty() {
                return Err("a dns step says what the name points at".into());
            }
            if let Some(kind) = kind
                && !matches!(kind.trim().to_ascii_uppercase().as_str(), "A" | "AAAA" | "CNAME")
            {
                return Err(format!("`{kind}` is not A, AAAA or CNAME"));
            }
            if ttl.is_some_and(|ttl| ttl > 86_400) {
                return Err("a record is kept for at most a day".into());
            }
            for source in [Some(name), Some(value), note.as_ref()].into_iter().flatten() {
                templated(source)?;
            }
        }
        Action::Infra { template, team, service, name, region, size, lifetime } => {
            if template.trim().is_empty() {
                return Err("an infra step names the Infra template to ask from".into());
            }
            if team.trim().is_empty() {
                return Err("an infra step names the team the resource is for".into());
            }
            for source in [
                Some(template),
                Some(team),
                service.as_ref(),
                name.as_ref(),
                region.as_ref(),
                size.as_ref(),
                lifetime.as_ref(),
            ]
            .into_iter()
            .flatten()
            {
                templated(source)?;
            }
        }
        Action::Publish { owner, repository, description, visibility, branch, message } => {
            if repository.trim().is_empty() {
                return Err("a publish step names the repository to create".into());
            }
            for source in [
                Some(repository),
                owner.as_ref(),
                description.as_ref(),
                visibility.as_ref(),
                branch.as_ref(),
                message.as_ref(),
            ]
            .into_iter()
            .flatten()
            {
                templated(source)?;
            }
        }
        Action::Register { kind, name, title, description, owner, connections, .. } => {
            if kind.trim().is_empty() || name.trim().is_empty() {
                return Err("a register step names a kind and a name".into());
            }
            for source in
                [Some(kind), Some(name), title.as_ref(), description.as_ref(), owner.as_ref()]
                    .into_iter()
                    .flatten()
            {
                templated(source)?;
            }
            for connection in connections {
                templated(connection)?;
            }
        }
        Action::Request { plugin, method, route, query, body } => {
            slug(plugin, "a request step's plugin")?;
            if !["GET", "POST", "PUT", "PATCH", "DELETE"].contains(&method.to_uppercase().as_str())
            {
                return Err(format!("`{method}` is not a method a step can use"));
            }
            templated(route)?;
            if let Some(query) = query {
                templated(query)?;
            }
            if let Some(body) = body {
                templated(&body.to_string())?;
            }
        }
        Action::Notify { user, title, body, url } => {
            if title.trim().is_empty() {
                return Err("a notify step needs a title".into());
            }
            for source in
                [Some(title), user.as_ref(), body.as_ref(), url.as_ref()].into_iter().flatten()
            {
                templated(source)?;
            }
        }
        Action::Event { topic, payload } => {
            for part in topic.split('.') {
                slug(part, "an event topic's segments")?;
            }
            if let Some(payload) = payload {
                templated(&payload.to_string())?;
            }
        }
    }
    Ok(())
}

/// Names that are safe in a URL, a topic and a column: a letter, then letters, digits and dashes.
pub fn slug(name: &str, what: &str) -> Result<(), String> {
    let first = name.chars().next();
    let shaped = first.is_some_and(|letter| letter.is_ascii_lowercase())
        && name
            .chars()
            .all(|letter| letter.is_ascii_lowercase() || letter.is_ascii_digit() || letter == '-');
    match shaped {
        true => Ok(()),
        false => Err(format!(
            "{what} is lowercase letters, digits and dashes, starting with a letter; `{name}` is not"
        )),
    }
}

/// A path inside the repository the template makes, and nowhere else.
pub fn safe_path(path: &str) -> Result<(), String> {
    let trimmed = path.trim();
    let wrong = trimmed.is_empty()
        || trimmed.starts_with('/')
        || trimmed.contains('\\')
        || trimmed.contains("//")
        || trimmed.split('/').any(|part| part == ".." || part == "." || part.is_empty())
        || trimmed.len() > 255;
    match wrong {
        true => Err(format!("`{path}` is not a path inside the repository")),
        false => Ok(()),
    }
}

/// What is wrong with one answer, for the form to show against the field.
#[derive(Debug)]
pub struct Problem {
    pub parameter: String,
    pub detail: String,
}

/// Checks what somebody filled in against what the template asks for, answering the values a run
/// will be given. Anything not asked for is dropped, so a form cannot smuggle a value into a step.
pub fn values(
    parameters: &[Parameter],
    given: &Map<String, Value>,
) -> Result<Map<String, Value>, Vec<Problem>> {
    let (mut values, mut problems) = (Map::new(), Vec::new());
    for parameter in parameters {
        let raw = given.get(&parameter.name).cloned().unwrap_or(Value::Null);
        match one(parameter, &raw) {
            Ok(Value::Array(items)) if items.is_empty() && parameter.required => {
                problems.push(Problem {
                    parameter: parameter.name.clone(),
                    detail: format!("{} is needed", parameter.label()),
                })
            }
            Ok(Value::Null) if parameter.required => problems.push(Problem {
                parameter: parameter.name.clone(),
                detail: format!("{} is needed", parameter.label()),
            }),
            Ok(value) => {
                values.insert(parameter.name.clone(), value);
            }
            Err(detail) => problems.push(Problem { parameter: parameter.name.clone(), detail }),
        }
    }
    match problems.is_empty() {
        true => Ok(values),
        false => Err(problems),
    }
}

fn one(parameter: &Parameter, raw: &Value) -> Result<Value, String> {
    let empty = match raw {
        Value::Null => true,
        Value::String(text) => text.trim().is_empty(),
        _ => false,
    };
    if empty {
        return Ok(parameter.default.clone().unwrap_or(match parameter.kind {
            ParameterKind::Boolean => json!(false),
            _ => Value::Null,
        }));
    }
    match parameter.kind {
        ParameterKind::Boolean => Ok(json!(truthy_answer(raw))),
        ParameterKind::List => {
            let items: Vec<String> = match raw {
                Value::Array(items) => items.iter().map(text_of).collect(),
                other => {
                    text_of(other).split(['\n', ',']).map(|item| item.trim().to_string()).collect()
                }
            };
            let items: Vec<String> = items.into_iter().filter(|item| !item.is_empty()).collect();
            let most = parameter.max.unwrap_or(50.0);
            if items.len() as f64 > most {
                return Err(format!("{} takes at most {most:.0} of them", parameter.label()));
            }
            Ok(json!(items))
        }
        ParameterKind::Number => {
            let number = match raw {
                Value::Number(number) => number.as_f64(),
                Value::String(text) => text.trim().parse::<f64>().ok(),
                _ => None,
            }
            .ok_or_else(|| format!("{} is a number", parameter.label()))?;
            if parameter.min.is_some_and(|min| number < min) {
                return Err(format!(
                    "{} is at least {}",
                    parameter.label(),
                    parameter.min.unwrap_or_default()
                ));
            }
            if parameter.max.is_some_and(|max| number > max) {
                return Err(format!(
                    "{} is at most {}",
                    parameter.label(),
                    parameter.max.unwrap_or_default()
                ));
            }
            Ok(json!(number))
        }
        ParameterKind::Select => {
            let text = text_of(raw);
            match parameter.options.iter().any(|choice| choice.value() == text) {
                true => Ok(Value::String(text)),
                false => Err(format!("{} is not one of the choices", parameter.label())),
            }
        }
        ParameterKind::Resource => {
            let text = text_of(raw);
            match text.split_once(':') {
                Some((kind, name)) if !kind.is_empty() && !name.is_empty() => {
                    Ok(Value::String(text))
                }
                _ => Err(format!("{} is a resource, written `Kind:name`", parameter.label())),
            }
        }
        ParameterKind::Text | ParameterKind::Textarea | ParameterKind::Team => {
            let text = text_of(raw);
            let longest = parameter.max.unwrap_or(match parameter.kind {
                ParameterKind::Textarea => 4_000.0,
                _ => 200.0,
            });
            if text.chars().count() as f64 > longest {
                return Err(format!("{} is at most {longest:.0} characters", parameter.label()));
            }
            if let Some(pattern) = &parameter.pattern {
                let matches = regex::Regex::new(pattern).map(|regex| regex.is_match(&text));
                if !matches.unwrap_or(false) {
                    return Err(parameter
                        .expects
                        .clone()
                        .unwrap_or_else(|| format!("{} does not look right", parameter.label())));
                }
            }
            Ok(Value::String(text))
        }
    }
}

fn text_of(raw: &Value) -> String {
    match raw {
        Value::String(text) => text.trim().to_string(),
        other => other.to_string(),
    }
}

/// How a checkbox and a JSON `true` both say yes.
fn truthy_answer(raw: &Value) -> bool {
    match raw {
        Value::Bool(yes) => *yes,
        Value::String(text) => matches!(text.trim(), "on" | "true" | "yes" | "1"),
        _ => false,
    }
}

/// Reads a definition from the YAML or JSON somebody wrote.
pub fn definition(text: &str) -> Result<Definition, String> {
    let value: Value = match text.trim_start().starts_with(['{', '[']) {
        true => serde_json::from_str(text)
            .map_err(|err| format!("the JSON could not be read: {err}"))?,
        false => serde_yaml_ng::from_str(text)
            .map_err(|err| format!("the YAML could not be read: {err}"))?,
    };
    let definition: Definition =
        serde_json::from_value(value).map_err(|err| format!("that is not a template: {err}"))?;
    definition.check()?;
    Ok(definition)
}
