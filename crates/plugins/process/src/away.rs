//! Who will not be there, from their own calendar.
//!
//! A recurring process falls to people, and people book leave in their calendar rather than
//! telling every plugin that cares. `calendar-events` answers `discovery/away` with the days somebody
//! is not working, so an occurrence due while everyone it falls to is away can say so — on its
//! page, in its reminder, and as `plugin.process.occurrence.uncovered` for an automation to pass
//! on to whoever owns the process.
//!
//! Only people are judged, never teams. A process a whole team is responsible for is not
//! uncovered because one of them is on holiday, and this plugin has no business deciding which of
//! them it would have fallen to.

use std::collections::BTreeSet;

use chrono::NaiveDate;
use doc_plugin_sdk::Backend;

use crate::store::{Assignee, Process};

/// One person, not working, for these days.
#[derive(Debug, Clone)]
struct Span {
    user: String,
    starts_on: NaiveDate,
    ends_on: NaiveDate,
    note: String,
}

/// Who is away, over the days asked about.
#[derive(Debug, Default, Clone)]
pub struct Away {
    spans: Vec<Span>,
}

impl Away {
    /// Asked once for everybody, rather than per process: a sweep looks at every process there is.
    ///
    /// A calendar that cannot be reached leaves nobody away, which is the safe way round — a
    /// process that is announced to somebody who turns out to be on holiday is a smaller problem
    /// than one silently marked uncovered because a plugin was restarting.
    pub async fn read(
        backend: &Backend,
        processes: &[Process],
        from: NaiveDate,
        to: NaiveDate,
    ) -> Self {
        let logins: BTreeSet<String> = processes
            .iter()
            .flat_map(|process| process.assignees.iter())
            .filter_map(|assignee| match assignee {
                Assignee::User(login) => Some(login.clone()),
                Assignee::Team(_) => None,
            })
            .collect();
        if logins.is_empty() {
            return Self::default();
        }
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("users", &logins.into_iter().collect::<Vec<_>>().join(","))
            .append_pair("from", &from.to_string())
            .append_pair("to", &to.to_string())
            .finish();
        let answer = backend.discovery("calendar-events", "GET", "away", Some(&query), None).await;
        let body = match answer {
            Ok((200, body)) => body,
            Ok((status, body)) => {
                tracing::debug!(
                    status,
                    detail = body["detail"].as_str().unwrap_or_default(),
                    "the calendar would not say who is away"
                );
                return Self::default();
            }
            Err(err) => {
                tracing::debug!(%err, "the calendar could not be asked who is away");
                return Self::default();
            }
        };
        let spans = body["away"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|span| {
                let day = |key: &str| span[key].as_str()?.parse::<NaiveDate>().ok();
                Some(Span {
                    user: span["user"].as_str()?.to_string(),
                    starts_on: day("starts_on")?,
                    ends_on: day("ends_on")?,
                    note: match span["away"].as_str() {
                        Some("sick") => "off sick".to_string(),
                        Some("holiday") => "on holiday".to_string(),
                        _ => "away".to_string(),
                    },
                })
            })
            .collect();
        Self { spans }
    }

    /// Why somebody is not working on `day`, if they are not.
    fn why(&self, login: &str, day: NaiveDate) -> Option<&str> {
        self.spans
            .iter()
            .find(|span| span.user == login && span.starts_on <= day && day <= span.ends_on)
            .map(|span| span.note.as_str())
    }

    /// The people a process falls to who will not be there on `day`, each as "ada is on holiday".
    pub fn among(&self, process: &Process, day: NaiveDate) -> Vec<String> {
        process
            .assignees
            .iter()
            .filter_map(|assignee| match assignee {
                Assignee::User(login) => {
                    self.why(login, day).map(|why| format!("{login} is {why}"))
                }
                Assignee::Team(_) => None,
            })
            .collect()
    }

    /// Whether every named person is away, and there was at least one to be away: the case worth
    /// telling somebody about, since nobody it falls to will be there.
    pub fn nobody_left(&self, process: &Process, day: NaiveDate) -> bool {
        let people: Vec<&String> = process
            .assignees
            .iter()
            .filter_map(|assignee| match assignee {
                Assignee::User(login) => Some(login),
                Assignee::Team(_) => None,
            })
            .collect();
        // A team among the assignees means somebody in it can pick it up.
        let teams = process.assignees.iter().any(|a| matches!(a, Assignee::Team(_)));
        !people.is_empty() && !teams && people.iter().all(|login| self.why(login, day).is_some())
    }
}
