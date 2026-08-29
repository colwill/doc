//! What the Maturity Model runs by: when it scores everything again, and whether a grade that
//! falls is told to whoever keeps the component.

use doc_plugin_sdk::{Setting, SettingKind};
use serde_json::json;

pub const SCORE_SCHEDULE: &str = "score-schedule";
/// Every hour, at twenty past rather than on the hour so it does not land with everything else.
/// Hourly rather than nightly because most of what moves a grade is not in the Catalogue — what
/// `eol`, `cicd` and `reliability` say about a service changes on their own schedules, and a
/// grade a day behind those is one nobody trusts.
pub const SCORE: &str = "20 * * * *";

pub fn declared() -> Vec<Setting> {
    vec![
        Setting::new(SCORE_SCHEDULE, "When to score everything again", SettingKind::Cron)
            .defaulting(json!(SCORE))
            .hinted(
                "A cron expression in UTC; hourly by default. Everything is also scored again \
                 as soon as somebody changes a model or one of its criteria, and a couple of \
                 minutes after the Catalogue changes, so this is for what moves without either — \
                 a service going past its end of life, a pipeline that started failing.",
            )
            .grouped("Scoring"),
    ]
}
