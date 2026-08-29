//! The playbooks: what an agent is asked to do for the common jobs. The MCP server offers them as
//! prompts, and a job DOC runs starts from the same words, so both kinds of agent work alike.

use serde_json::{Value, json};

pub struct Argument {
    pub name: &'static str,
    pub about: &'static str,
    pub required: bool,
}

pub struct Playbook {
    pub name: &'static str,
    pub title: &'static str,
    pub about: &'static str,
    pub arguments: &'static [Argument],
    body: &'static str,
}

/// The playbook that runs a runbook from the Knowledge Base.
pub const RUNBOOK: &str = "runbook";

/// How to reach the rest of DOC, which every playbook shares.
pub const DOC: &str =
    "You are working in DOC, an internal developer platform. Everything you do is \
done as the person you work for, with exactly their access: a refusal means they may not do it, \
so say so rather than working round it.

Tools:
- doc_search and doc_resource read the Catalogue: services, teams, repositories, documentation, \
cloud resources and how they connect.
- doc_readiness asks every plugin that measures a service whether it is ready.
- doc_status says how DOC itself and each plugin are.
- The <plugin>_<tool> tools are the plugins' own: dora_* for DORA metrics, cicd_* for pipelines, \
reliability_* for availability and outages, eol_* for end of life, roadmap_* for releases, kb_* \
for documentation.
- doc_api calls any plugin's API. Common routes:
  - resources POST apply with a JSON array of documents, each {kind, name, title, description, \
owner (a team's name), organisation, metadata, connections: {Repository: [owner/name], ...}}, \
where kind is Organisation, Service, Team, Repository, Documentation or CloudResource. Add \
?dry_run=true to see what would change first.
  - kb POST imports/pages with {space, title, owners: [team names], pages: [{path, content}]}, \
the content being Markdown.
  - water POST threads with {title, body, tags}, a tag being kind:name such as service:payments-api.
  - rbac POST groups with {plugin, name, kind: user|service, description, permissions}, and POST \
assignments with {holder: {kind: user|service|team, id} or {kind: group, plugin, name}, permission}.
  - vacuum GET and POST runs, to stage an import for an administrator to approve instead.
- doc_add_person adds somebody by email address into a team, under the organisation's approved \
email domains. doc_remind and doc_notify tell people things. doc_discuss starts a discussion.
Be economical: read what you need, not everything.";

pub const ALL: [Playbook; 6] = [
    Playbook {
        name: "onboard",
        title: "Onboard an existing platform",
        about: "Brings documentation, services, teams, people and permissions into DOC from \
                Backstage, Roadie, Confluence or wherever they are now.",
        arguments: &[
            Argument {
                name: "from",
                about: "Where they are now, such as \"Backstage at https://backstage.acme.dev\"",
                required: true,
            },
            Argument {
                name: "what",
                about: "What to bring: documentation, catalogue, teams, people, permissions; all when empty",
                required: false,
            },
        ],
        body: "Onboard an organisation's existing platform into DOC, from: {from}. Bring: {what}.

1. Learn what DOC holds already (doc_search, and rbac GET groups), so you reuse names rather \
than duplicating them.
2. Read the source. Backstage and Roadie answer GET /api/catalog/entities (filter with \
?filter=kind=component, and so on) and GET /api/techdocs/...; Confluence answers \
/wiki/rest/api/content.
3. Map it:
   - Backstage Domain and System become an Organisation or a Service's metadata.
   - Component becomes a Service, with its repository as a Repository connected to it.
   - Resource becomes a CloudResource.
   - Group becomes a Team, with its parent as the team it sits in.
   - User becomes a person added by email (doc_add_person) to the teams they were in.
   - Documentation becomes Knowledge Base pages in a space owned by the right team.
   - A group's permissions become RBAC assignments to the team.
4. Write through DOC as the person. Apply the Catalogue with a dry run first, then for real. \
When the person asked for review rather than writing, stage it through the Data Vacuum instead.
5. Finish with what was brought in, what was left out and why, and what someone should check.",
    },
    Playbook {
        name: "root_cause",
        title: "Find a root cause",
        about: "Explains a failure from what DORA, CI/CD/CT, reliability, Kubernetes and the \
                Catalogue saw.",
        arguments: &[
            Argument {
                name: "subject",
                about: "The service, pipeline or outage, such as payments-api",
                required: true,
            },
            Argument { name: "happened", about: "What was seen, if you know", required: false },
        ],
        body: "Find the root cause of a problem with {subject}. What was seen: {happened}.

1. Read the service in the Catalogue: what it is connected to, who owns it.
2. Build a timeline from what DOC measured: failed deployments (dora), broken pipelines and \
re-runs (cicd), outages (reliability), rollouts (kubernetes through doc_api), merged changes, \
and anything end of life (eol).
3. Say what most likely caused it, what else could have, and what the evidence is for each. Say \
plainly what you could not see.
4. Recommend what to do now and what would stop it happening again.
5. Finish with the report: the cause first, then the timeline, the evidence and the \
recommendations.",
    },
    Playbook {
        name: "health",
        title: "Report on platform health",
        about: "An organisation or team's delivery, reliability, maturity and support, worst first.",
        arguments: &[Argument {
            name: "scope",
            about: "The organisation or team, or everything when empty",
            required: false,
        }],
        body: "Report on the health of {scope}.

Read DORA metrics, pipeline success, reliability against objectives, what is near or past its \
end of life, maturity grades and whether upcoming releases are ready, and how DOC itself is. \
Put the worst first, say what changed since the period before where the data says, and suggest \
one or two things to do about each problem. Finish with the report, short enough to read in two \
minutes.",
    },
    Playbook {
        name: "reminders",
        title: "Set reminders for what is coming",
        about: "Sets reminders for end of life dates, releases and expiring secrets.",
        arguments: &[Argument {
            name: "scope",
            about: "The organisation, team or service, or everything when empty",
            required: false,
        }],
        body: "Find what is coming up for {scope}: products reaching end of life, releases due, \
secrets expiring in Secret Storage (secrets GET secrets), and set a reminder (doc_remind) for \
each, a week before, for the person or team who has to act. Do not set one twice. Finish with \
the list you set.",
    },
    Playbook {
        name: "custom",
        title: "Custom",
        about: "Only the brief, in your own words.",
        arguments: &[Argument { name: "brief", about: "What to do", required: true }],
        body: "{brief}",
    },
    Playbook {
        name: RUNBOOK,
        title: "Run a runbook",
        about: "Follows a runbook from the Knowledge Base: works out which of its situations \
                applies, does the steps DOC has tools for, and says what is left to do by hand.",
        arguments: &[
            Argument {
                name: "runbook",
                about: "The runbook as space/path, such as card-gateway/docs/runbook.md",
                required: true,
            },
            Argument {
                name: "environment",
                about: "development, test or production; production when empty",
                required: false,
            },
        ],
        body: "Run the runbook {runbook} against {environment}. Read it first with \
kb_get_document, its space being what comes before the first / and its path the rest.

1. Work out from what DOC can see which of its situations applies, if any, before acting on one.
2. Do each step you have a tool for, and check it worked. Against production change nothing: put \
each change in the report as a step for the person, with exactly what to do.
3. For each step outside DOC, such as a command or a console, say exactly what to do.
4. Finish with what you found, what you did, what was refused and what is left to do by hand.",
    },
];

pub fn named(name: &str) -> Option<&'static Playbook> {
    ALL.iter().find(|playbook| playbook.name == name)
}

impl Playbook {
    /// The request, with each argument put in; one not given reads as not said.
    pub fn asked(&self, arguments: &Value) -> String {
        let mut text = self.body.to_string();
        for argument in self.arguments {
            let given =
                arguments[argument.name].as_str().map(str::trim).filter(|given| !given.is_empty());
            let fallback = match argument.name {
                "what" => "everything it can",
                "scope" => "the whole platform",
                "environment" => "production",
                _ => "not said",
            };
            text = text.replace(&format!("{{{}}}", argument.name), given.unwrap_or(fallback));
        }
        text
    }

    /// What an agent is told first: how DOC works, then the request.
    pub fn instructions(&self, arguments: &Value) -> String {
        format!("{DOC}\n\n{}", self.asked(arguments))
    }

    pub fn described(&self) -> Value {
        json!({
            "name": self.name,
            "title": self.title,
            "description": self.about,
            "arguments": self.arguments.iter().map(|argument| json!({
                "name": argument.name, "description": argument.about, "required": argument.required,
            })).collect::<Vec<_>>(),
        })
    }
}
