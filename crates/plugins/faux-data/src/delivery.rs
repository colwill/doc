//! Faux delivery data, in the shape GitHub's delivery data exports it (`deployments` and
//! `pull-requests`, DOC-SPEC §9.2), and the incidents an automation would count with `dora`'s
//! `increment`: deployments with the commits they shipped, the pull requests merged for them,
//! and, now and then, a failure — a revert, a rollback, a hotfix or an incident — and the
//! deployment that fixed it. Deployments were rarer, slower and riskier a year ago.

use chrono::{DateTime, Datelike, Utc, Weekday};
use serde::Serialize;
use uuid::Uuid;

use crate::dice::{DAY, Dice, hours, midnight_of, progress};
use crate::estate;
use crate::settings::Config;

#[derive(Debug, Clone, Serialize)]
pub struct Commit {
    pub sha: String,
    pub at: DateTime<Utc>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Deployment {
    pub id: String,
    pub repository: String,
    pub environment: String,
    pub kind: String,
    pub state: String,
    pub sha: String,
    pub url: Option<String>,
    pub finished_at: Option<DateTime<Utc>>,
    pub rollback: bool,
    pub commits: Vec<Commit>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PullRequest {
    pub repository: String,
    pub title: String,
    pub labels: Vec<String>,
    pub merged_at: DateTime<Utc>,
    pub merge_sha: Option<String>,
    pub url: Option<String>,
}

/// An incident, as `dora` keeps what its `increment` operation counted.
#[derive(Debug, Clone, Serialize)]
pub struct Incident {
    pub id: Uuid,
    pub counter: String,
    pub amount: i64,
    pub service: Option<String>,
    pub repository: Option<String>,
    pub at: DateTime<Utc>,
    pub by: Option<String>,
    pub note: Option<String>,
}

/// How a repository delivers now. Hours throughout.
struct Behaviour {
    per_weekday: f64,
    lead: (f64, f64),
    fails: f64,
    noticed: (f64, f64),
    recovered: (f64, f64),
}

/// One for each profile, best first.
const BEHAVIOURS: [Behaviour; 4] = [
    Behaviour {
        per_weekday: 3.0,
        lead: (0.5, 20.0),
        fails: 0.05,
        noticed: (0.05, 0.3),
        recovered: (0.2, 1.5),
    },
    Behaviour {
        per_weekday: 0.9,
        lead: (12.0, 200.0),
        fails: 0.12,
        noticed: (0.2, 3.0),
        recovered: (1.5, 20.0),
    },
    Behaviour {
        per_weekday: 0.15,
        lead: (72.0, 900.0),
        fails: 0.2,
        noticed: (1.0, 12.0),
        recovered: (26.0, 150.0),
    },
    Behaviour {
        per_weekday: 0.03,
        lead: (400.0, 2400.0),
        fails: 0.4,
        noticed: (2.0, 20.0),
        recovered: (200.0, 900.0),
    },
];

/// Everything made up for one repository.
#[derive(Debug, Default, Serialize)]
pub struct Delivered {
    pub deployments: Vec<Deployment>,
    pub pull_requests: Vec<PullRequest>,
    pub incidents: Vec<Incident>,
}

fn deployment(repository: &str, id: String, dice: &mut Dice, at: DateTime<Utc>) -> Deployment {
    Deployment {
        id,
        repository: repository.to_string(),
        environment: "production".into(),
        kind: "deployment".into(),
        state: "success".into(),
        sha: dice.sha(),
        url: None,
        finished_at: Some(at),
        rollback: false,
        commits: Vec::new(),
    }
}

/// One day of one repository's delivery.
fn day(config: &Config, repository: &str, index: i64, now: DateTime<Utc>, out: &mut Delivered) {
    let Some(midnight) = midnight_of(index) else { return };
    let profile = estate::profile(config, estate::of_repository(repository));
    let behaviour = &BEHAVIOURS[profile.index()];
    let mut dice = Dice::seeded(config.seed, &format!("{repository}#delivery"), index);
    // From a year ago to now, deploying more often, faster and more safely.
    let progress = progress(now, midnight);
    let weekend = matches!(midnight.weekday(), Weekday::Sat | Weekday::Sun);
    let expected = behaviour.per_weekday * (0.6 + 0.6 * progress) * if weekend { 0.1 } else { 1.0 };
    let slower = 1.5 - 0.7 * progress;
    let riskier = 1.5 - 0.8 * progress;
    let mut times: Vec<f64> =
        (0..dice.count(expected)).map(|_| dice.between((9.0, 18.0))).collect();
    times.sort_by(f64::total_cmp);

    for (at, deployed) in times.into_iter().enumerate() {
        let deployed = midnight + hours(deployed);
        if deployed > now {
            break;
        }
        let id = format!("{repository}/{index}/{at}");
        let commits: Vec<Commit> = (0..1 + dice.count(2.5).min(11))
            .map(|change| Commit {
                sha: dice.sha(),
                at: deployed - hours(dice.spread(behaviour.lead) * slower),
                message: format!("Change {index}-{at}-{change}"),
            })
            .collect();
        let last = commits.iter().max_by_key(|commit| commit.at).map(|commit| commit.sha.clone());
        out.pull_requests.push(PullRequest {
            repository: repository.to_string(),
            title: format!("Change {index}-{at}"),
            labels: Vec::new(),
            merged_at: deployed - hours(dice.between((0.2, 2.0))),
            merge_sha: last,
            url: None,
        });
        let mut shipped = deployment(repository, id.clone(), &mut dice, deployed);
        shipped.commits = commits;
        out.deployments.push(shipped);

        if !dice.chance(behaviour.fails * riskier) {
            continue;
        }
        let noticed = deployed + hours(dice.spread(behaviour.noticed));
        let fixed = noticed + hours(dice.spread(behaviour.recovered));
        if noticed > now {
            continue;
        }
        let mut fix = deployment(repository, format!("{id}/fix"), &mut dice, fixed);
        fix.commits =
            vec![Commit { sha: dice.sha(), at: noticed, message: format!("Fix {index}-{at}") }];
        // A rollback is only a failure within the failure window, so a slow one reverts instead.
        let rolls_back = fixed - deployed < hours(20.0);
        match dice.next() % 4 {
            1 if rolls_back => {
                fix.rollback = true;
                fix.commits.clear();
            }
            0 | 1 => fix.commits[0].message = format!("Revert \"Change {index}-{at}\""),
            2 => out.pull_requests.push(PullRequest {
                repository: repository.to_string(),
                title: format!("Hotfix: after change {index}-{at}"),
                labels: vec!["hotfix".into()],
                merged_at: noticed,
                merge_sha: Some(fix.commits[0].sha.clone()),
                url: None,
            }),
            _ => out.incidents.push(Incident {
                id: Uuid::from_u64_pair(dice.next(), dice.next()),
                counter: "incidents".into(),
                amount: 1,
                service: None,
                repository: Some(repository.to_string()),
                at: noticed,
                by: Some(crate::ID.into()),
                note: None,
            }),
        }
        if fixed <= now {
            out.deployments.push(fix);
        }
    }
}

/// Everything made up for one repository from `from` to `to`.
pub fn delivered(
    config: &Config,
    repository: &str,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Delivered {
    let now = Utc::now();
    let mut out = Delivered::default();
    let first = from.timestamp().div_euclid(DAY);
    let last = to.min(now).timestamp().div_euclid(DAY);
    for index in first..=last {
        day(config, repository, index, now, &mut out);
    }
    out.deployments.sort_by_key(|deployment| deployment.finished_at);
    out
}
