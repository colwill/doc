//! Faux release data, in the shape Jira's release data exports it (`projects`, `versions` and
//! `issues`, DOC-SPEC §9.2): a project for each service, with two releases shipped, one under
//! way, one planned and one further off, and issues in each, dated from today. A service that
//! fails also has a release past its date; one that thrives is ahead of its time.

use chrono::{Duration, NaiveDate, Utc};
use serde_json::{Value, json};

use crate::dice::Dice;
use crate::estate::{self, Profile};
use crate::settings::Config;

const SUMMARIES: [&str; 24] = [
    "Retry declined authorisations once",
    "Show settlement totals by currency",
    "Move secrets to the vault",
    "Upgrade the HTTP client",
    "Rate limit the public API",
    "Fix rounding on partial refunds",
    "Add an audit trail to manual adjustments",
    "Page the on-call team on failed payouts",
    "Split the reconciliation job by region",
    "Support 3-D Secure 2.2",
    "Remove the legacy export endpoint",
    "Cache exchange rates for an hour",
    "Fix timezone of daily statements",
    "Add a runbook link to every alert",
    "Stream events to the data platform",
    "Tighten the database connection pool",
    "Handle duplicate webhooks",
    "Accessibility fixes on the dispute form",
    "Contract tests for the ledger API",
    "Archive disputes older than seven years",
    "Load test the capture path",
    "Replace the deprecated logging library",
    "Nightly backup restore drill",
    "Document the chargeback states",
];

const KINDS: [&str; 3] = ["Story", "Bug", "Task"];

/// One made-up release: days from today it starts and is due, whether it shipped, how many
/// issues it has and the share of them done.
struct Plan {
    name: String,
    start: Option<i64>,
    due: Option<i64>,
    released: bool,
    issues: usize,
    done: f64,
}

fn plans(profile: Profile, major: u64, minor: u64) -> Vec<Plan> {
    let at = profile.index();
    let plan = |name: String, start, due, released, issues, done| Plan {
        name,
        start,
        due,
        released,
        issues,
        done,
    };
    let mut plans = vec![
        plan(format!("{major}.{}", minor - 2), Some(-75), Some(-40), true, 9, 1.0),
        plan(format!("{major}.{}", minor - 1), Some(-40), Some(-12), true, 11, 1.0),
        plan(
            format!("{major}.{minor}"),
            Some(-20),
            Some([10, 10, 9, 4][at]),
            false,
            [10, 14, 18, 22][at],
            [0.85, 0.7, 0.45, 0.25][at],
        ),
        plan(format!("{major}.{}", minor + 1), Some(12), Some(45), false, 8, 0.1),
        plan(
            format!("{}.0", major + 1),
            None,
            (profile != Profile::Failing).then_some(110),
            false,
            5,
            0.0,
        ),
    ];
    if profile == Profile::Failing {
        plans.push(plan(format!("{major}.{}.1", minor - 1), Some(-30), Some(-6), false, 4, 0.75));
    }
    plans
}

fn day(today: NaiveDate, days: Option<i64>) -> Option<NaiveDate> {
    days.map(|days| today + Duration::days(days))
}

/// The projects, releases and issues of these services.
pub fn held(config: &Config, services: &[String]) -> Value {
    let today = Utc::now().date_naive();
    let (mut projects, mut versions, mut issues) = (Vec::new(), Vec::new(), Vec::new());
    for service in services {
        let key = estate::project(service);
        let mut dice = Dice::seeded(config.seed, &format!("{service}#releases"), 0);
        let (major, minor) = (1 + dice.next() % 4, 3 + dice.next() % 7);
        projects.push(json!({ "key": key, "name": format!("{service} (faux)"), "url": null }));
        let mut number = 0;
        for (index, plan) in
            plans(estate::profile(config, service), major, minor).into_iter().enumerate()
        {
            let id = format!("{service}-v{index}");
            let done = (plan.issues as f64 * plan.done).round() as usize;
            // A release nobody has started has nothing in progress either.
            let doing = match plan.done > 0.0 {
                true => ((plan.issues - done) as f64 * 0.4).round() as usize,
                false => 0,
            };
            for place in 0..plan.issues {
                number += 1;
                let (category, status) = match place {
                    place if place < done => ("done", "Done"),
                    place if place < done + doing => ("indeterminate", "In Progress"),
                    _ => ("new", "To Do"),
                };
                let summary = SUMMARIES[(dice.next() as usize) % SUMMARIES.len()];
                issues.push(json!({
                    "id": format!("{service}-{number}"),
                    "key": format!("{key}-{number}"),
                    "project": key,
                    "summary": summary,
                    "type": KINDS[(number + index) % KINDS.len()],
                    "status": status,
                    "category": category,
                    "versions": [id],
                    "components": [],
                    "labels": [],
                    "url": null,
                }));
            }
            versions.push(json!({
                "id": id,
                "project": key,
                "name": plan.name,
                "description": null,
                "released": plan.released,
                "start_date": day(today, plan.start),
                "release_date": day(today, plan.due),
                "url": null,
            }));
        }
    }
    json!({ "projects": projects, "versions": versions, "issues": issues })
}
