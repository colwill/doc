//! When a process is due. Each occurrence is a wall-clock time in the process's own zone, so a
//! Monday 10:00 in London stays 10:00 through daylight saving, and a day past the end of a short
//! month means that month's last day.

use chrono::{
    DateTime, Datelike, Duration, Months, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc,
};
use chrono_tz::Tz;

pub const LOCAL: &str = "%Y-%m-%dT%H:%M:%S";

/// How far ahead to look: far enough for a dozen occurrences of a yearly process.
const SEARCH_DAYS: usize = 13 * 366;

pub const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

pub const WEEKDAYS: [&str; 7] =
    ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cadence {
    Daily,
    Weekly,
    Monthly,
    Quarterly,
    Yearly,
}

impl Cadence {
    pub const ALL: [Self; 5] =
        [Self::Daily, Self::Weekly, Self::Monthly, Self::Quarterly, Self::Yearly];

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|cadence| cadence.name().eq_ignore_ascii_case(text.trim()))
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Weekly => "weekly",
            Self::Monthly => "monthly",
            Self::Quarterly => "quarterly",
            Self::Yearly => "yearly",
        }
    }
}

pub fn weekday_name(weekday: u32) -> &'static str {
    WEEKDAYS.get(weekday.saturating_sub(1) as usize).copied().unwrap_or("Monday")
}

pub fn month_name(month: u32) -> &'static str {
    MONTHS.get(month.saturating_sub(1) as usize).copied().unwrap_or("January")
}

fn ordinal(day: u32) -> String {
    let suffix = match (day % 10, day % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{day}{suffix}")
}

fn last_day(year: i32, month: u32) -> u32 {
    let next = match month {
        12 => NaiveDate::from_ymd_opt(year + 1, 1, 1),
        _ => NaiveDate::from_ymd_opt(year, month + 1, 1),
    };
    next.and_then(|first| first.pred_opt()).map_or(28, |last| last.day())
}

/// The instant a wall-clock time means in `zone`; in a gap made by daylight saving, the time after it.
pub fn instant(zone: Tz, at: NaiveDateTime) -> DateTime<Tz> {
    zone.from_local_datetime(&at)
        .earliest()
        .or_else(|| zone.from_local_datetime(&(at + Duration::hours(1))).earliest())
        .unwrap_or_else(|| zone.from_utc_datetime(&at))
}

/// A process's cadence, with the day it falls on and its time.
#[derive(Debug, Clone, Copy)]
pub struct Rule {
    pub cadence: Cadence,
    /// 1 for Monday to 7 for Sunday, for a weekly process.
    pub weekday: u32,
    /// 1 to 12 for a yearly process; 1 to 3, the month of each quarter, for a quarterly one.
    pub month: u32,
    pub day: u32,
    pub time: NaiveTime,
    pub zone: Tz,
}

/// One time a process is due: the wall-clock time that names it, and the instant it means.
#[derive(Debug, Clone)]
pub struct Due {
    pub local: NaiveDateTime,
    pub at: DateTime<Tz>,
}

impl Due {
    pub fn named(&self) -> String {
        self.local.format(LOCAL).to_string()
    }
}

impl Rule {
    pub fn check(&self) -> Result<(), String> {
        let (weekdays, months, days) = (1..=7, 1..=12, 1..=31);
        match self.cadence {
            Cadence::Weekly if !weekdays.contains(&self.weekday) => {
                Err("a weekly process falls on a weekday, 1 for Monday to 7 for Sunday".into())
            }
            Cadence::Quarterly if !(1..=3).contains(&self.month) => {
                Err("a quarterly process falls in the first, second or third month of each quarter, 1 to 3".into())
            }
            Cadence::Yearly if !months.contains(&self.month) => {
                Err("a yearly process falls in a month, 1 to 12".into())
            }
            Cadence::Monthly | Cadence::Quarterly | Cadence::Yearly if !days.contains(&self.day) => {
                Err("a process falls on a day of the month, 1 to 31".into())
            }
            _ => Ok(()),
        }
    }

    /// Whether it is due on `date`, a date in its own zone.
    pub fn falls_on(&self, date: NaiveDate) -> bool {
        let day_matches = || date.day() == self.day.min(last_day(date.year(), date.month()));
        match self.cadence {
            Cadence::Daily => true,
            Cadence::Weekly => date.weekday().number_from_monday() == self.weekday,
            Cadence::Monthly => day_matches(),
            Cadence::Quarterly => (date.month() - 1) % 3 + 1 == self.month && day_matches(),
            Cadence::Yearly => date.month() == self.month && day_matches(),
        }
    }

    fn due_on(&self, date: NaiveDate) -> Due {
        let local = date.and_time(self.time);
        Due { local, at: instant(self.zone, local) }
    }

    /// Each time it is due from `from` up to `until`, and at least the next `at_least` after `from`.
    pub fn upcoming(&self, from: DateTime<Utc>, until: DateTime<Utc>, at_least: usize) -> Vec<Due> {
        let first = from.with_timezone(&self.zone).date_naive() - Duration::days(1);
        let mut found = Vec::new();
        for date in first.iter_days().take(SEARCH_DAYS) {
            if !self.falls_on(date) {
                continue;
            }
            let due = self.due_on(date);
            let at = due.at.with_timezone(&Utc);
            if at < from {
                continue;
            }
            if at > until && found.len() >= at_least {
                break;
            }
            found.push(due);
        }
        found
    }

    /// The day, week, month, quarter or year holding `date`, from its first day to the next one's.
    pub fn period(&self, date: NaiveDate) -> (NaiveDate, NaiveDate) {
        let month_start = |month: u32| NaiveDate::from_ymd_opt(date.year(), month, 1);
        let (start, months) = match self.cadence {
            Cadence::Daily => return (date, date + Duration::days(1)),
            Cadence::Weekly => {
                let monday =
                    date - Duration::days(i64::from(date.weekday().num_days_from_monday()));
                return (monday, monday + Duration::days(7));
            }
            Cadence::Monthly => (month_start(date.month()), 1),
            Cadence::Quarterly => (month_start((date.month() - 1) / 3 * 3 + 1), 3),
            Cadence::Yearly => (month_start(1), 12),
        };
        let start = start.unwrap_or(date);
        (start, start.checked_add_months(Months::new(months)).unwrap_or(date))
    }

    pub fn describe(&self) -> String {
        let time = self.time.format("%H:%M");
        let zone = self.zone.name();
        let clamped = |day: u32| match day {
            29..=31 => " (or the month's last day)",
            _ => "",
        };
        match self.cadence {
            Cadence::Daily => format!("Every day at {time} ({zone})"),
            Cadence::Weekly => {
                format!("Every {} at {time} ({zone})", weekday_name(self.weekday))
            }
            Cadence::Monthly => format!(
                "Monthly, on the {}{} at {time} ({zone})",
                ordinal(self.day),
                clamped(self.day)
            ),
            Cadence::Quarterly => {
                let months: Vec<&str> =
                    (0..4).map(|quarter| month_name(self.month + quarter * 3)).collect();
                format!(
                    "Quarterly, on the {}{} of {} at {time} ({zone})",
                    ordinal(self.day),
                    clamped(self.day),
                    months.join(", ")
                )
            }
            Cadence::Yearly => {
                let day = self.day.min(last_day(2024, self.month));
                let leap = match (self.month, day) {
                    (2, 29) => " (28 February in other years)",
                    _ => "",
                };
                format!("Every year on {day} {}{leap} at {time} ({zone})", month_name(self.month))
            }
        }
    }
}
