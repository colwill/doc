//! Faux release cycles, in the shape endoflife.date's API v1 gives them: real product names with
//! made-up dates, counted from today, so a release is as far from its end of life whenever it is
//! looked at.

use chrono::{Duration, NaiveDate, Utc};
use serde_json::{Value, json};

/// One made-up release: days from today it came out, its active support ended, its end of life
/// and its extended support ended; `None` where the product gives no such date.
struct Made {
    name: &'static str,
    lts: bool,
    released: i64,
    support: Option<i64>,
    eol: Option<i64>,
    extended: Option<i64>,
    latest: &'static str,
}

const fn made(
    name: &'static str,
    lts: bool,
    (released, support, eol, extended): (i64, Option<i64>, Option<i64>, Option<i64>),
    latest: &'static str,
) -> Made {
    Made { name, lts, released, support, eol, extended, latest }
}

/// endoflife.date's name, its label, its category, and its releases, newest first.
type Catalogue = (&'static str, &'static str, &'static str, &'static [Made]);

const PRODUCTS: [Catalogue; 8] = [
    (
        "nodejs",
        "Node.js",
        "framework",
        &[
            made("24", true, (-150, Some(400), Some(950), None), "24.9.0"),
            made("22", true, (-520, Some(30), Some(560), None), "22.20.0"),
            made("20", true, (-880, Some(-340), Some(120), None), "20.19.5"),
            made("18", true, (-1250, Some(-700), Some(-150), None), "18.20.8"),
        ],
    ),
    (
        "python",
        "Python",
        "lang",
        &[
            made("3.13", false, (-350, Some(380), Some(1480), None), "3.13.7"),
            made("3.12", false, (-720, Some(-30), Some(1100), None), "3.12.11"),
            made("3.11", false, (-1070, Some(-400), Some(750), None), "3.11.13"),
            made("3.9", false, (-1810, Some(-1100), Some(-360), None), "3.9.23"),
        ],
    ),
    (
        "postgresql",
        "PostgreSQL",
        "database",
        &[
            made("17", false, (-360, Some(1460), Some(1460), None), "17.6"),
            made("16", false, (-730, Some(1100), Some(1100), None), "16.10"),
            made("15", false, (-1090, Some(730), Some(730), None), "15.14"),
            made("13", false, (-1820, Some(-320), Some(-320), None), "13.22"),
        ],
    ),
    (
        "eclipse-temurin",
        "Eclipse Temurin (Java)",
        "lang",
        &[
            made("21", true, (-700, None, Some(1500), None), "21.0.8+9"),
            made("17", true, (-1460, None, Some(700), None), "17.0.16+8"),
            made("11", true, (-2555, None, Some(90), None), "11.0.28+6"),
        ],
    ),
    (
        "spring-boot",
        "Spring Boot",
        "framework",
        &[
            made("3.5", false, (-120, Some(240), Some(600), Some(900)), "3.5.6"),
            made("3.4", false, (-300, Some(-30), Some(420), Some(720)), "3.4.10"),
            made("3.3", false, (-480, Some(-210), Some(60), Some(420)), "3.3.13"),
            made("2.7", false, (-1580, Some(-1000), Some(-300), Some(200)), "2.7.18"),
        ],
    ),
    (
        "react",
        "React",
        "framework",
        &[
            made("19", false, (-280, None, None, None), "19.1.1"),
            made("18", false, (-1640, Some(-280), None, None), "18.3.1"),
            made("17", false, (-2150, Some(-1640), Some(-280), None), "17.0.2"),
        ],
    ),
    (
        "redis",
        "Redis",
        "database",
        &[
            made("8.2", false, (-60, Some(700), Some(700), None), "8.2.1"),
            made("7.4", false, (-400, Some(300), Some(300), None), "7.4.5"),
            made("7.2", false, (-760, Some(150), Some(150), None), "7.2.10"),
            made("6.2", false, (-1600, Some(-200), Some(-200), None), "6.2.19"),
        ],
    ),
    (
        "ubuntu",
        "Ubuntu",
        "os",
        &[
            made("24.04", true, (-540, Some(1270), Some(1270), Some(3100)), "24.04.3"),
            made("22.04", true, (-1270, Some(580), Some(580), Some(2400)), "22.04.5"),
            made("20.04", true, (-2000, Some(-150), Some(-150), Some(1500)), "20.04.6"),
        ],
    ),
];

fn day(today: NaiveDate, days: Option<i64>) -> Value {
    match days {
        Some(days) => json!((today + Duration::days(days)).format("%Y-%m-%d").to_string()),
        None => Value::Null,
    }
}

/// Every product made up, by endoflife.date's name: what is given when none is asked for.
pub fn names() -> Vec<String> {
    PRODUCTS.iter().map(|(name, ..)| name.to_string()).collect()
}

/// A product's release cycles as endoflife.date's `api/v1/products/<product>` answers; `None` for
/// a product faux data does not make up, which a plugin then shows as not known.
pub fn product(key: &str) -> Option<Value> {
    let (name, label, category, releases) =
        PRODUCTS.iter().find(|(name, ..)| *name == key).copied()?;
    let today = Utc::now().date_naive();
    let releases: Vec<Value> = releases
        .iter()
        .map(|made| {
            let ended = |days: Option<i64>| days.is_some_and(|days| days <= 0);
            json!({
                "name": made.name,
                "label": if made.lts { format!("{} (LTS)", made.name) } else { made.name.to_string() },
                "releaseDate": day(today, Some(made.released)),
                "isLts": made.lts,
                "isEoas": ended(made.support),
                "eoasFrom": day(today, made.support),
                "isEol": ended(made.eol),
                "eolFrom": day(today, made.eol),
                "isEoes": made.extended.map(|days| days <= 0),
                "eoesFrom": day(today, made.extended),
                "isMaintained": !ended(made.eol),
                "latest": { "name": made.latest, "date": day(today, Some(-20)), "link": null },
            })
        })
        .collect();
    Some(json!({
        "schema_version": "1.2.1",
        "result": {
            "name": name,
            "label": label,
            "category": category,
            "links": { "html": format!("https://endoflife.date/{name}") },
            "releases": releases,
        },
    }))
}
