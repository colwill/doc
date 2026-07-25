//! Ready-made automations for the platform's own notifications. Each listens for an event another
//! plugin publishes and delivers it to the team of the resource it is applied to.

use serde_json::{Value, json};

use crate::model::{Action, Condition, Op, Trigger};

pub struct Template {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    /// The resource kinds it suits, as they are written in `kind:name`.
    pub kinds: &'static [&'static str],
    pub topic: &'static str,
    /// The payload field naming whom an event is for, compared with the resource it is applied to.
    pub field: &'static str,
    pub by_ref: bool,
    pub via: Via,
}

#[derive(Clone, Copy)]
pub enum Via {
    Slack(&'static str),
    Email(&'static str, &'static str),
}

pub const ALL: [Template; 7] = [
    Template {
        name: "kudos-to-slack",
        title: "Kudos to Slack",
        description: "Posts each kudos a team is given to the team's Slack channel.",
        kinds: &["team"],
        topic: "plugin.water.kudos.given",
        field: "payload.team",
        by_ref: false,
        via: Via::Slack(
            ":tada: Kudos from {{payload.from}} to {{payload.to}}: {{payload.message}} {{payload.url}}",
        ),
    },
    Template {
        name: "kudos-by-email",
        title: "Kudos by email",
        description: "Emails each kudos a team is given to the team's address.",
        kinds: &["team"],
        topic: "plugin.water.kudos.given",
        field: "payload.team",
        by_ref: false,
        via: Via::Email(
            "Kudos for {{payload.to}}",
            "{{payload.from}} gave kudos to {{payload.to}}:\n\n{{payload.message}}\n\n{{payload.url}}",
        ),
    },
    Template {
        name: "card-to-slack",
        title: "Cards to Slack",
        description: "Tells the team's Slack channel when a card for one of its people is delivered.",
        kinds: &["team"],
        topic: "plugin.water.card.delivered",
        field: "payload.team",
        by_ref: false,
        via: Via::Slack(
            ":envelope_with_arrow: {{payload.title}} for {{payload.recipient}} has been delivered. {{payload.url}}",
        ),
    },
    Template {
        name: "process-reminder",
        title: "Process reminders",
        description: "Reminds the resource's team in Slack when an occurrence of one of its processes is coming up.",
        kinds: &["organisation", "service", "repository", "team", "cloudresource"],
        topic: "plugin.process.occurrence.reminder",
        field: "payload.resource",
        by_ref: true,
        via: Via::Slack(
            ":alarm_clock: {{payload.process}} is due {{payload.due_at}}, assigned to {{payload.assignees}}. {{payload.url}}",
        ),
    },
    Template {
        name: "process-overdue",
        title: "Overdue processes",
        description: "Tells the resource's team in Slack when an occurrence of one of its processes is missed.",
        kinds: &["organisation", "service", "repository", "team", "cloudresource"],
        topic: "plugin.process.occurrence.missed",
        field: "payload.resource",
        by_ref: true,
        via: Via::Slack(
            ":warning: {{payload.process}}, due {{payload.due_at}}, was missed. {{payload.url}}",
        ),
    },
    Template {
        name: "event-reminder",
        title: "Event reminders",
        description: "Reminds the resource's team in Slack before each event on the resource's calendar.",
        kinds: &["organisation", "service", "team", "user"],
        topic: "plugin.calendar-events.reminder.due",
        field: "payload.calendar",
        by_ref: true,
        via: Via::Slack(
            ":calendar: {{payload.title}} starts {{payload.starts_at}}. {{payload.url}}",
        ),
    },
    Template {
        name: "infra-expiry",
        title: "Infra expiry warnings",
        description: "Emails the team before one of its cloud resources expires.",
        kinds: &["team"],
        topic: "plugin.infra.resource.expiring",
        field: "payload.team",
        by_ref: false,
        via: Via::Email(
            "{{payload.name}} expires {{payload.expires_at}}",
            "{{payload.name}} ({{payload.vendor}}) expires {{payload.expires_at}} and will then be deleted. Extend it in DOC if it is still needed: {{payload.url}}",
        ),
    },
];

pub fn named(name: &str) -> Option<&'static Template> {
    ALL.iter().find(|template| template.name == name)
}

impl Template {
    pub fn shown(&self) -> Value {
        let via = match self.via {
            Via::Slack(_) => "slack",
            Via::Email(..) => "email",
        };
        json!({
            "name": self.name,
            "title": self.title,
            "description": self.description,
            "kinds": self.kinds,
            "topic": self.topic,
            "via": via,
        })
    }

    /// Its trigger, conditions and action; `to` names a channel or address other than the team's.
    pub fn parts(&self, to: Option<String>) -> (Trigger, Vec<Condition>, Vec<Action>) {
        let value = match self.by_ref {
            true => "{{resource.ref}}",
            false => "{{resource.name}}",
        };
        let condition = Condition { field: self.field.into(), op: Op::Eq, value: json!(value) };
        let action = match self.via {
            Via::Slack(text) => Action::Slack { channel: to, text: text.into() },
            Via::Email(subject, body) => {
                Action::Email { to, subject: subject.into(), body: body.into() }
            }
        };
        (Trigger::Event { topic: self.topic.into() }, vec![condition], vec![action])
    }
}
