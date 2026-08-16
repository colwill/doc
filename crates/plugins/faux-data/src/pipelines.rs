//! Faux pipeline data, in the shape GitHub's pipeline data exports it (`workflow-runs`, DOC-SPEC
//! §9.2): four workflows a repository — `CI`, `E2E tests`, `Deploy` and a `Nightly regression` —
//! with each merge's integration, tests and deployment on the default branch, pull requests'
//! integration, re-runs, and each time a workflow broke and the run that fixed it. Pipelines were
//! slower and broke more a year ago, so the trends have somewhere to go.

use chrono::{DateTime, Datelike, Duration, Utc, Weekday};
use serde::Serialize;

use crate::dice::{DAY, Dice, hours, midnight_of, minutes, progress};
use crate::estate::{self, Profile};
use crate::settings::Config;

/// How far before a period runs are made up, so a branch already broken when it starts is read
/// from the run that broke it: longer than the slowest fix.
const BEFORE_DAYS: i64 = 8;

#[derive(Debug, Clone, Serialize)]
pub struct Run {
    pub id: String,
    pub repository: String,
    pub workflow_id: i64,
    pub workflow: String,
    pub path: Option<String>,
    pub event: Option<String>,
    pub branch: Option<String>,
    pub default_branch: bool,
    pub sha: String,
    pub conclusion: String,
    pub attempt: i64,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: DateTime<Utc>,
    pub url: Option<String>,
}

/// How a repository's pipelines run now. Minutes for runs, hours for fixes.
struct Behaviour {
    merges: f64,
    pulls: f64,
    breaks: f64,
    fix: (f64, f64),
    integration: (f64, f64),
    testing: (f64, f64),
    delivery: (f64, f64),
    flaky: f64,
    pulls_fail: f64,
}

/// One for each profile, best first.
const BEHAVIOURS: [Behaviour; 4] = [
    Behaviour {
        merges: 6.0,
        pulls: 10.0,
        breaks: 0.02,
        fix: (0.2, 1.5),
        integration: (3.0, 9.0),
        testing: (5.0, 14.0),
        delivery: (2.0, 6.0),
        flaky: 0.01,
        pulls_fail: 0.1,
    },
    Behaviour {
        merges: 3.0,
        pulls: 6.0,
        breaks: 0.18,
        fix: (0.6, 6.0),
        integration: (7.0, 18.0),
        testing: (12.0, 30.0),
        delivery: (5.0, 14.0),
        flaky: 0.035,
        pulls_fail: 0.18,
    },
    Behaviour {
        merges: 1.2,
        pulls: 3.0,
        breaks: 0.5,
        fix: (2.0, 20.0),
        integration: (15.0, 40.0),
        testing: (25.0, 70.0),
        delivery: (10.0, 30.0),
        flaky: 0.07,
        pulls_fail: 0.25,
    },
    Behaviour {
        merges: 0.5,
        pulls: 1.5,
        breaks: 1.0,
        fix: (15.0, 90.0),
        integration: (40.0, 100.0),
        testing: (60.0, 150.0),
        delivery: (30.0, 80.0),
        flaky: 0.2,
        pulls_fail: 0.35,
    },
];

/// One of the four workflows every faux repository has.
#[derive(Clone, Copy)]
struct Flow {
    id: i64,
    name: &'static str,
    path: &'static str,
}

const INTEGRATION: Flow = Flow { id: 101, name: "CI", path: ".github/workflows/ci.yml" };
const TESTING: Flow = Flow { id: 102, name: "E2E tests", path: ".github/workflows/e2e.yml" };
const DELIVERY: Flow = Flow { id: 103, name: "Deploy", path: ".github/workflows/deploy.yml" };
const NIGHTLY: Flow =
    Flow { id: 104, name: "Nightly regression", path: ".github/workflows/regression.yml" };
const FLOWS: [Flow; 4] = [INTEGRATION, TESTING, DELIVERY, NIGHTLY];

/// A repository being made up for: its name, the estate's seed and how it behaves.
struct Maker<'a> {
    repository: &'a str,
    seed: u64,
    behaviour: &'static Behaviour,
    now: DateTime<Utc>,
}

impl Maker<'_> {
    /// The times one workflow broke on the default branch on one day, each until its fix: made
    /// up from dice of their own, so any day's runs can ask whether an earlier day left it broken.
    fn breaks(&self, flow: Flow, index: i64) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
        let Some(midnight) = midnight_of(index) else { return Vec::new() };
        let behaviour = self.behaviour;
        let mut dice = Dice::seeded(self.seed, &format!("{}#{}", self.repository, flow.id), index);
        let progress = progress(self.now, midnight);
        let (slower, riskier) = (1.5 - 0.7 * progress, 1.5 - 0.8 * progress);
        let weekend = matches!(midnight.weekday(), Weekday::Sat | Weekday::Sun);
        let expected = match flow.id {
            104 => behaviour.breaks * 0.4,
            _ => behaviour.merges * behaviour.breaks * if weekend { 0.1 } else { 1.0 },
        };
        (0..dice.count(expected * riskier))
            .map(|_| {
                let broke = match flow.id {
                    104 => midnight + hours(2.0) + minutes(dice.spread(behaviour.testing) * slower),
                    _ => midnight + hours(dice.between((9.0, 18.0))),
                };
                (broke, broke + hours(dice.spread(behaviour.fix) * slower))
            })
            .collect()
    }

    /// Every spell one workflow was broken that could reach into day `index`, none overlapping:
    /// a break while already broken changes nothing.
    fn spells(&self, flow: Flow, index: i64) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
        let mut found: Vec<(DateTime<Utc>, DateTime<Utc>)> = Vec::new();
        for day in index - BEFORE_DAYS..=index {
            let mut broke = self.breaks(flow, day);
            broke.sort();
            for (start, end) in broke {
                if found
                    .iter()
                    .all(|(held_start, held_end)| start < *held_start || start >= *held_end)
                {
                    found.push((start, end));
                }
            }
        }
        found
    }

    /// One day of runs: each merge's integration, tests and deployment on the default branch,
    /// pull requests' integration, the nightly regression, and the run that broke each workflow
    /// and the one that fixed it.
    fn day(&self, index: i64, runs: &mut Vec<Run>) {
        let Some(midnight) = midnight_of(index) else { return };
        let (behaviour, now, repository) = (self.behaviour, self.now, self.repository);
        let mut dice = Dice::seeded(self.seed, repository, index);
        let progress = progress(now, midnight);
        let weekend = matches!(midnight.weekday(), Weekday::Sat | Weekday::Sun);
        let busy = (0.6 + 0.6 * progress) * if weekend { 0.1 } else { 1.0 };
        let (slower, riskier) = (1.5 - 0.7 * progress, 1.5 - 0.8 * progress);
        let broken: Vec<Vec<(DateTime<Utc>, DateTime<Utc>)>> =
            FLOWS.iter().map(|flow| self.spells(*flow, index)).collect();
        let broken_at = |flow: Flow, at: DateTime<Utc>| {
            let slot = FLOWS.iter().position(|each| each.id == flow.id).unwrap_or_default();
            broken[slot].iter().any(|(start, end)| *start <= at && at < *end)
        };
        let mut made = 0;
        // A run is made up from when it finished, so the one that broke a workflow and the one
        // that fixed it end exactly where the spell does.
        let mut run = |dice: &mut Dice,
                       flow: Flow,
                       finished: DateTime<Utc>,
                       took: f64,
                       branch: Option<&str>,
                       conclusion: &str| {
            if finished > now {
                return;
            }
            let started = finished - minutes(took);
            let created = started - Duration::seconds(dice.between((5.0, 90.0)) as i64);
            let passed = conclusion == "success";
            let attempt = if passed && dice.chance(behaviour.flaky * riskier) { 2 } else { 1 };
            let event = match (branch, flow.id) {
                (Some(_), _) => "pull_request",
                (None, 104) => "schedule",
                (None, _) => "push",
            };
            made += 1;
            runs.push(Run {
                id: format!("{repository}/runs/{index}{made:04}"),
                repository: repository.to_string(),
                workflow_id: flow.id,
                workflow: flow.name.to_string(),
                path: Some(flow.path.to_string()),
                event: Some(event.to_string()),
                branch: Some(branch.unwrap_or("main").to_string()),
                default_branch: branch.is_none(),
                sha: dice.sha(),
                conclusion: conclusion.to_string(),
                attempt,
                created_at: created,
                started_at: Some(started),
                finished_at: finished,
                url: None,
            });
        };
        let ended = |dice: &mut Dice, flow: Flow, finished: DateTime<Utc>| -> &'static str {
            match () {
                () if broken_at(flow, finished) => "failure",
                () if dice.chance(0.02) => "cancelled",
                () => "success",
            }
        };
        let queued = |dice: &mut Dice| Duration::seconds(dice.between((5.0, 90.0)) as i64);

        let mut merged: Vec<f64> =
            (0..dice.count(behaviour.merges * busy)).map(|_| dice.between((9.0, 18.0))).collect();
        merged.sort_by(f64::total_cmp);
        for at in merged {
            let created = midnight + hours(at);
            for (flow, took) in [(INTEGRATION, behaviour.integration), (TESTING, behaviour.testing)]
            {
                let took = dice.spread(took) * slower;
                let finished = created + queued(&mut dice) + minutes(took);
                let conclusion = ended(&mut dice, flow, finished);
                run(&mut dice, flow, finished, took, None, conclusion);
            }
            let deployed = created + minutes(dice.spread(behaviour.integration) * slower);
            let took = dice.spread(behaviour.delivery) * slower;
            let finished = deployed + queued(&mut dice) + minutes(took);
            let conclusion = ended(&mut dice, DELIVERY, finished);
            run(&mut dice, DELIVERY, finished, took, None, conclusion);
        }
        for pull in 0..dice.count(behaviour.pulls * busy) {
            let created = midnight + hours(dice.between((8.0, 20.0)));
            let took = dice.spread(behaviour.integration) * slower;
            let conclusion = match () {
                () if dice.chance(0.05) => "cancelled",
                () if dice.chance(behaviour.pulls_fail * riskier) => "failure",
                () => "success",
            };
            let branch = format!("change-{index}-{pull}");
            let finished = created + queued(&mut dice) + minutes(took);
            run(&mut dice, INTEGRATION, finished, took, Some(&branch), conclusion);
        }
        let took = dice.spread(behaviour.testing) * slower;
        let finished = midnight + hours(2.0) + queued(&mut dice) + minutes(took);
        let conclusion = ended(&mut dice, NIGHTLY, finished);
        run(&mut dice, NIGHTLY, finished, took, None, conclusion);

        // The run that broke each workflow today, and the one that fixed it, whenever that was.
        let tomorrow = midnight + Duration::days(1);
        for flow in FLOWS {
            for (broke, fixed) in self.spells(flow, index) {
                if broke < midnight || broke >= tomorrow {
                    continue;
                }
                let took = match flow.id {
                    103 => behaviour.delivery,
                    101 => behaviour.integration,
                    _ => behaviour.testing,
                };
                let took = dice.spread(took) * slower;
                run(&mut dice, flow, broke, took, None, "failure");
                run(&mut dice, flow, fixed, took, None, "success");
            }
        }
    }
}

/// Every run of one repository that finished from `from` to `to`, oldest first.
pub fn runs(config: &Config, repository: &str, from: DateTime<Utc>, to: DateTime<Utc>) -> Vec<Run> {
    let profile: Profile = estate::profile(config, estate::of_repository(repository));
    let maker = Maker {
        repository,
        seed: config.seed,
        behaviour: &BEHAVIOURS[profile.index()],
        now: Utc::now(),
    };
    let mut found = Vec::new();
    let first = from.timestamp().div_euclid(DAY) - BEFORE_DAYS;
    let last = to.min(maker.now).timestamp().div_euclid(DAY);
    for index in first..=last {
        maker.day(index, &mut found);
    }
    found.retain(|run| from <= run.finished_at && run.finished_at < to);
    found.sort_by_key(|run| run.finished_at);
    found
}
